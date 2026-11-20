//! Core modification hooks.
//!
//! A hook is a plugin contribution that runs *inside* an existing turn, at a point the runtime
//! already reaches. It does not replace the turn loop, the provider or a tool: it sees what the
//! runtime is about to do and may deny it, rewrite it, or add context around it. Disabling the
//! plugin restores the default behaviour because the code path itself is unchanged.
//!
//! Two kinds of hook share one contract:
//!
//! * **Declarative** — `hook_rules` in `plugin.json`. The host evaluates the rule in-process, so
//!   there is no subprocess and no trust question beyond the manifest's declared permissions.
//! * **Process** — the plugin's `entry` script is spawned for each dispatch, with the hook point as
//!   `argv[1]` and the context JSON on stdin. It replies with one JSON object on stdout. A process
//!   hook may only be declared by a manifest that grants an execution permission (`process`).
//!
//! Permissions are deny-first: a rule for `tool_call`/`tool_result` needs `tools`, every other
//! point needs `agents`, and a process hook additionally needs `process` (or `network`). A plugin
//! that does not declare what a point requires is not registered, and the refusal is reported
//! through [`HookRegistry::rejected`] rather than silently dropped.
//!
//! Ordering is deterministic: higher `priority` first, then plugin id lexicographically. A denial
//! short-circuits the chain. Every other effect is chained, so a later hook sees the text,
//! arguments and result an earlier hook produced.
//!
//! Failure isolation: a hook that panics, exits non-zero, times out ([`HOOK_TIMEOUT`]) or replies
//! with malformed JSON is recorded and skipped; the turn continues with the effect of the hooks
//! that did work.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::plugins::*;

/// How long a process hook may take before it is killed and ignored.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// A point in the turn the runtime reaches and hooks may act on.
///
/// These are the points that are actually dispatched. A hook declared for anything else is
/// rejected at load time rather than stored as an inert promise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HookPoint {
    /// A session row was created and is about to go live.
    SessionStart,
    /// A user message enters a turn, before it is durable. May rewrite the prose.
    UserInput,
    /// The turn has been accepted and is about to assemble its context.
    TurnStart,
    /// The system prefix has been assembled, before the transcript is fitted to the budget.
    ContextBuild,
    /// Long-term memory is being read into the context for this step.
    MemoryRecall,
    /// The completion request is built, immediately before it is sent.
    ModelRequest,
    /// A model tool call is about to be judged by policy and run.
    ToolCall,
    /// A `memory.*` tool call is about to be judged by policy and run.
    MemoryWrite,
    /// A tool returned, before its output is committed to the conversation.
    ToolResult,
    /// Assistant prose is about to be committed to the transcript. May rewrite or withhold it.
    ResponseDelivery,
    /// The turn has finished or failed, before `TurnFinished` is published.
    TurnEnd,
    /// A delegated run is about to be spawned.
    DelegationStart,
    /// A delegated run's answer is being handed back to the session that asked.
    DelegationEnd,
    /// A session leaves the live set (closed, deleted, or reaped after delegation).
    SessionEnd,
}

impl HookPoint {
    /// Every point, in the order a session or turn reaches them.
    pub const ALL: [HookPoint; 14] = [
        HookPoint::SessionStart,
        HookPoint::UserInput,
        HookPoint::TurnStart,
        HookPoint::ContextBuild,
        HookPoint::MemoryRecall,
        HookPoint::ModelRequest,
        HookPoint::ToolCall,
        HookPoint::MemoryWrite,
        HookPoint::ToolResult,
        HookPoint::ResponseDelivery,
        HookPoint::TurnEnd,
        HookPoint::DelegationStart,
        HookPoint::DelegationEnd,
        HookPoint::SessionEnd,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            HookPoint::SessionStart => "session_start",
            HookPoint::UserInput => "user_input",
            HookPoint::TurnStart => "turn_start",
            HookPoint::ContextBuild => "context_build",
            HookPoint::MemoryRecall => "memory_recall",
            HookPoint::ModelRequest => "model_request",
            HookPoint::ToolCall => "tool_call",
            HookPoint::MemoryWrite => "memory_write",
            HookPoint::ToolResult => "tool_result",
            HookPoint::ResponseDelivery => "response_delivery",
            HookPoint::TurnEnd => "turn_end",
            HookPoint::DelegationStart => "delegation_start",
            HookPoint::DelegationEnd => "delegation_end",
            HookPoint::SessionEnd => "session_end",
        }
    }

    /// Parse a manifest spelling. Hyphens and `.` are accepted for `_`.
    pub fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase().replace(['-', '.'], "_");
        match normalized.as_str() {
            "session_start" => Some(HookPoint::SessionStart),
            "user_input" | "input" => Some(HookPoint::UserInput),
            "turn_start" => Some(HookPoint::TurnStart),
            "context_build" | "context" => Some(HookPoint::ContextBuild),
            "memory_recall" | "memory_read" => Some(HookPoint::MemoryRecall),
            "model_request" | "model" => Some(HookPoint::ModelRequest),
            "tool_call" => Some(HookPoint::ToolCall),
            "memory_write" => Some(HookPoint::MemoryWrite),
            "tool_result" => Some(HookPoint::ToolResult),
            "response_delivery" | "response" => Some(HookPoint::ResponseDelivery),
            "turn_end" => Some(HookPoint::TurnEnd),
            "delegation_start" | "delegate_start" => Some(HookPoint::DelegationStart),
            "delegation_end" | "delegate_end" => Some(HookPoint::DelegationEnd),
            "session_end" => Some(HookPoint::SessionEnd),
            _ => None,
        }
    }

    /// Does this point carry a tool call?
    ///
    /// The memory points qualify because their primary site is a `memory.*` tool call: they need
    /// the `tools` permission and accept a `tool` glob. Fired outside a tool (recall into the
    /// context) they simply carry no `tool`, and rules conditioned on one do not match.
    pub fn is_tool_surface(self) -> bool {
        matches!(
            self,
            HookPoint::ToolCall
                | HookPoint::ToolResult
                | HookPoint::MemoryRecall
                | HookPoint::MemoryWrite
        )
    }

    /// The manifest permission a hook for this point must declare.
    pub fn permission(self) -> PluginPermission {
        if self.is_tool_surface() {
            PluginPermission::Tools
        } else {
            PluginPermission::Agents
        }
    }
}

/// A tool call as a hook sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct HookTool {
    pub name: String,
    pub arguments: Value,
}

/// A tool result as a hook sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct HookResult {
    pub content: String,
    pub ok: bool,
}

/// Everything a hook is given at one point.
///
/// `text` is the notable prose for the point: the user's request at `user_input`, `turn_start`,
/// `context_build`, `memory_recall` and `model_request`, the assistant prose at
/// `response_delivery`, the outcome name at `turn_end`, empty at `tool_call`/`tool_result`
/// (`tool`/`result` carry the structured view). `details` carries point-specific structured
/// payload — a turn's outcome/steps/tokens at `turn_end`, a delegation's bot and task at
/// `delegation_*` — so process hooks receive it without a schema change per point.
#[derive(Clone, Debug, PartialEq)]
pub struct HookContext {
    pub point: HookPoint,
    pub session: String,
    pub turn: String,
    pub workspace: String,
    pub model: String,
    pub text: String,
    pub tool: Option<HookTool>,
    pub result: Option<HookResult>,
    pub details: Value,
}

impl HookContext {
    pub fn new(point: HookPoint) -> Self {
        Self {
            point,
            session: String::new(),
            turn: String::new(),
            workspace: String::new(),
            model: String::new(),
            text: String::new(),
            tool: None,
            result: None,
            details: Value::Null,
        }
    }

    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session = session.into();
        self
    }

    pub fn with_turn(mut self, turn: impl Into<String>) -> Self {
        self.turn = turn.into();
        self
    }

    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = workspace.into();
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    pub fn with_tool(mut self, name: impl Into<String>, arguments: Value) -> Self {
        self.tool = Some(HookTool {
            name: name.into(),
            arguments,
        });
        self
    }

    pub fn with_result(mut self, content: impl Into<String>, ok: bool) -> Self {
        self.result = Some(HookResult {
            content: content.into(),
            ok,
        });
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    /// The context as a process hook receives it.
    pub fn to_json(&self) -> Value {
        json!({
            "point": self.point.as_str(),
            "session": self.session,
            "turn": self.turn,
            "workspace": self.workspace,
            "model": self.model,
            "text": self.text,
            "tool": self.tool.as_ref().map(|tool| json!({
                "name": tool.name,
                "arguments": tool.arguments,
            })),
            "result": self.result.as_ref().map(|result| json!({
                "content": result.content,
                "ok": result.ok,
            })),
            "details": self.details,
        })
    }
}

/// What a hook decided. Every field is optional; an empty effect is "allow, change nothing".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HookEffect {
    /// Refuse the operation. The first denial in the chain ends it.
    pub deny: Option<String>,
    /// System text to place before the assembled context (in dispatch order).
    pub prepend: Vec<String>,
    /// System text to place after the assembled context (in dispatch order).
    pub append: Vec<String>,
    /// Replacement arguments for a tool call.
    pub arguments: Option<Value>,
    /// Replacement content for a tool result.
    pub content: Option<String>,
    /// Replacement success flag for a tool result.
    pub ok: Option<bool>,
    /// Replacement output cap for a model request.
    pub max_output_tokens: Option<u32>,
    /// Replacement prose for the point's `text` — the user input at `user_input`, the assistant
    /// reply at `response_delivery`. Points that carry no rewritable prose ignore it.
    pub text: Option<String>,
}

impl HookEffect {
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            deny: Some(reason.into()),
            ..Self::default()
        }
    }

    pub fn append_text(text: impl Into<String>) -> Self {
        Self {
            append: vec![text.into()],
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.deny.is_none()
            && self.prepend.is_empty()
            && self.append.is_empty()
            && self.arguments.is_none()
            && self.content.is_none()
            && self.ok.is_none()
            && self.max_output_tokens.is_none()
            && self.text.is_none()
    }

    /// Merge another effect into this one, later values winning where a field is a single value.
    pub fn absorb(&mut self, other: HookEffect) {
        if other.deny.is_some() {
            self.deny = other.deny;
        }
        self.prepend.extend(other.prepend);
        self.append.extend(other.append);
        if other.arguments.is_some() {
            self.arguments = other.arguments;
        }
        if other.content.is_some() {
            self.content = other.content;
        }
        if other.ok.is_some() {
            self.ok = other.ok;
        }
        if other.max_output_tokens.is_some() {
            self.max_output_tokens = other.max_output_tokens;
        }
        if other.text.is_some() {
            self.text = other.text;
        }
    }

    /// A one-line description for the journal and `vikram plugins hooks`.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(reason) = &self.deny {
            parts.push(format!("deny: {reason}"));
        }
        if !self.prepend.is_empty() {
            parts.push(format!("prepend {} message(s)", self.prepend.len()));
        }
        if !self.append.is_empty() {
            parts.push(format!("append {} message(s)", self.append.len()));
        }
        if self.arguments.is_some() {
            parts.push("rewrite arguments".to_owned());
        }
        if self.content.is_some() {
            parts.push("rewrite result".to_owned());
        }
        if self.ok.is_some() {
            parts.push("set result status".to_owned());
        }
        if let Some(max) = self.max_output_tokens {
            parts.push(format!("cap output at {max}"));
        }
        if self.text.is_some() {
            parts.push("rewrite text".to_owned());
        }
        if parts.is_empty() {
            "allow".to_owned()
        } else {
            parts.join(", ")
        }
    }

    /// Decode the reply a process hook wrote on stdout.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "hook reply must be a JSON object".to_owned())?;
        let mut effect = Self::default();
        match object.get("action").and_then(Value::as_str) {
            Some("deny") => {
                effect.deny = Some(
                    object
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("denied by plugin")
                        .to_owned(),
                );
            }
            Some("allow") | Some("modify") | None => {}
            Some(other) => return Err(format!("unknown hook action `{other}`")),
        }
        if let Some(reason) = object.get("deny").and_then(Value::as_str) {
            effect.deny = Some(reason.to_owned());
        }
        effect.prepend = strings(object.get("prepend"));
        effect.append = strings(object.get("append"));
        if let Some(arguments) = object.get("arguments") {
            if !arguments.is_object() {
                return Err("`arguments` must be a JSON object".to_owned());
            }
            effect.arguments = Some(arguments.clone());
        }
        if let Some(content) = object.get("content").and_then(Value::as_str) {
            effect.content = Some(content.to_owned());
        }
        if let Some(ok) = object.get("ok").and_then(Value::as_bool) {
            effect.ok = Some(ok);
        }
        if let Some(max) = object.get("max_output_tokens").and_then(Value::as_u64) {
            effect.max_output_tokens = Some(
                u32::try_from(max).map_err(|_| "`max_output_tokens` is too large".to_owned())?,
            );
        }
        if let Some(text) = object.get("text").and_then(Value::as_str) {
            effect.text = Some(text.to_owned());
        }
        Ok(effect)
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// A declarative rule: condition + effect, evaluated by the host.
///
/// Conditions are case-insensitive substring tests (`tool` is a glob on the tool name). A rule
/// with a condition that does not apply to its point — `result_contain` on `context_build`, say —
/// is refused at load time by [`HookRule::validate`], so the catalog cannot silently do nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HookRule {
    pub point: String,
    /// Glob on the tool name: `shell.run`, `browser.*`, `*`.
    pub tool: Option<String>,
    /// Substring of the tool arguments, as serialized JSON.
    pub arguments_contain: Option<String>,
    /// Substring of the tool result content.
    pub result_contain: Option<String>,
    /// Substring of the point's prose text.
    pub text_contain: Option<String>,
    pub deny: Option<String>,
    pub prepend: Option<String>,
    pub append: Option<String>,
    pub append_result: Option<String>,
    pub replace_result: Option<String>,
    pub set_ok: Option<bool>,
    /// Replacement arguments as a JSON object literal; merged into the call's arguments.
    pub arguments_set: Option<String>,
    pub max_output_tokens: Option<u32>,
    /// Replacement prose for the point's text; only `user_input` and `response_delivery` carry
    /// rewritable prose.
    pub replace_text: Option<String>,
}

impl HookRule {
    pub fn point(&self) -> Option<HookPoint> {
        HookPoint::parse(&self.point)
    }

    fn has_action(&self) -> bool {
        self.deny.is_some()
            || self.prepend.is_some()
            || self.append.is_some()
            || self.append_result.is_some()
            || self.replace_result.is_some()
            || self.set_ok.is_some()
            || self.arguments_set.is_some()
            || self.max_output_tokens.is_some()
            || self.replace_text.is_some()
    }

    /// Check the rule against its own point. An invalid rule is reported, never registered.
    pub fn validate(&self) -> Result<(), String> {
        let point = self
            .point()
            .ok_or_else(|| format!("unknown hook point `{}`", self.point))?;
        if !self.has_action() {
            return Err("rule declares a condition but no effect".to_owned());
        }
        if (self.arguments_contain.is_some() || self.arguments_set.is_some())
            && point != HookPoint::ToolCall
        {
            return Err(format!(
                "`arguments_contain`/`arguments_set` do not apply to `{}`",
                point.as_str()
            ));
        }
        if (self.result_contain.is_some()
            || self.append_result.is_some()
            || self.replace_result.is_some()
            || self.set_ok.is_some())
            && point != HookPoint::ToolResult
        {
            return Err(format!(
                "result conditions/effects do not apply to `{}`",
                point.as_str()
            ));
        }
        if self.max_output_tokens.is_some() && point != HookPoint::ModelRequest {
            return Err(format!(
                "`max_output_tokens` does not apply to `{}`",
                point.as_str()
            ));
        }
        if self.replace_text.is_some()
            && !matches!(point, HookPoint::UserInput | HookPoint::ResponseDelivery)
        {
            return Err(format!(
                "`replace_text` does not apply to `{}`",
                point.as_str()
            ));
        }
        if let Some(raw) = &self.arguments_set {
            let value: Value = serde_json::from_str(raw)
                .map_err(|e| format!("`arguments_set` is not valid JSON: {e}"))?;
            if !value.is_object() {
                return Err("`arguments_set` must be a JSON object".to_owned());
            }
        }
        if self.tool.is_some() && !point.is_tool_surface() {
            return Err(format!("`tool` does not apply to `{}`", point.as_str()));
        }
        Ok(())
    }

    fn matches(&self, ctx: &HookContext) -> bool {
        if let Some(pattern) = &self.tool {
            let Some(tool) = &ctx.tool else { return false };
            if !glob_match(pattern, &tool.name) {
                return false;
            }
        }
        if let Some(needle) = &self.arguments_contain {
            let Some(tool) = &ctx.tool else { return false };
            if !contains_ci(&tool.arguments.to_string(), needle) {
                return false;
            }
        }
        if let Some(needle) = &self.result_contain {
            let Some(result) = &ctx.result else {
                return false;
            };
            if !contains_ci(&result.content, needle) {
                return false;
            }
        }
        if let Some(needle) = &self.text_contain
            && !contains_ci(&ctx.text, needle)
        {
            return false;
        }
        true
    }

    fn effect(&self, ctx: &HookContext) -> HookEffect {
        let mut effect = HookEffect::default();
        if let Some(reason) = &self.deny {
            effect.deny = Some(reason.clone());
        }
        if let Some(text) = &self.prepend {
            effect.prepend.push(text.clone());
        }
        if let Some(text) = &self.append {
            effect.append.push(text.clone());
        }
        if let Some(raw) = &self.arguments_set
            && let Ok(Value::Object(extra)) = serde_json::from_str::<Value>(raw)
        {
            let mut merged = match &ctx.tool {
                Some(tool) => tool.arguments.clone(),
                None => Value::Object(Default::default()),
            };
            if !merged.is_object() {
                merged = Value::Object(Default::default());
            }
            if let Value::Object(object) = &mut merged {
                for (key, value) in extra {
                    object.insert(key, value);
                }
            }
            effect.arguments = Some(merged);
        }
        if let Some(text) = &self.replace_result {
            effect.content = Some(text.clone());
        }
        if let Some(text) = &self.append_result {
            let base = ctx
                .result
                .as_ref()
                .map(|result| result.content.clone())
                .unwrap_or_default();
            effect.content = Some(format!("{base}\n{text}"));
        }
        if let Some(ok) = self.set_ok {
            effect.ok = Some(ok);
        }
        if let Some(max) = self.max_output_tokens {
            effect.max_output_tokens = Some(max);
        }
        if let Some(text) = &self.replace_text {
            effect.text = Some(text.clone());
        }
        effect
    }
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// `*` matches any run of characters, `?` exactly one.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    let (mut p, mut t) = (pattern.chars().peekable(), text.chars().peekable());
    let (mut star, mut retry) = (None, 0usize);
    let text_chars: Vec<char> = text.chars().collect();
    let mut ti = 0usize;
    let pattern_chars: Vec<char> = pattern.chars().collect();
    let mut pi = 0usize;
    while ti < text_chars.len() {
        if pi < pattern_chars.len()
            && (pattern_chars[pi] == '?' || pattern_chars[pi] == text_chars[ti])
        {
            pi += 1;
            ti += 1;
        } else if pi < pattern_chars.len() && pattern_chars[pi] == '*' {
            star = Some(pi);
            pi += 1;
            retry = ti;
        } else if let Some(star_at) = star {
            pi = star_at + 1;
            retry += 1;
            ti = retry;
        } else {
            return false;
        }
    }
    let _ = (&mut p, &mut t);
    while pi < pattern_chars.len() && pattern_chars[pi] == '*' {
        pi += 1;
    }
    pi == pattern_chars.len()
}

/// One hook contribution.
pub trait Hook: Send + Sync {
    fn plugin(&self) -> &str;
    fn kind(&self) -> &'static str;
    fn priority(&self) -> i32;
    fn points(&self) -> &[HookPoint];
    fn describe(&self) -> String;
    fn run(&self, ctx: &HookContext) -> Result<HookEffect, String>;
}

/// Declarative rules evaluated in-process.
pub struct RuleHook {
    plugin: String,
    priority: i32,
    points: Vec<HookPoint>,
    rules: Vec<HookRule>,
}

impl RuleHook {
    pub fn new(plugin: impl Into<String>, priority: i32, rules: Vec<HookRule>) -> Self {
        let mut points: Vec<HookPoint> = Vec::new();
        for rule in &rules {
            if let Some(point) = rule.point()
                && !points.contains(&point)
            {
                points.push(point);
            }
        }
        Self {
            plugin: plugin.into(),
            priority,
            points,
            rules,
        }
    }

    pub fn rules(&self) -> &[HookRule] {
        &self.rules
    }
}

impl Hook for RuleHook {
    fn plugin(&self) -> &str {
        &self.plugin
    }

    fn kind(&self) -> &'static str {
        "rule"
    }

    fn priority(&self) -> i32 {
        self.priority
    }

    fn points(&self) -> &[HookPoint] {
        &self.points
    }

    fn describe(&self) -> String {
        format!("{} rule(s)", self.rules.len())
    }

    fn run(&self, ctx: &HookContext) -> Result<HookEffect, String> {
        let mut effect = HookEffect::default();
        for rule in &self.rules {
            if rule.point() == Some(ctx.point) && rule.matches(ctx) {
                effect.absorb(rule.effect(ctx));
            }
        }
        Ok(effect)
    }
}

/// A hook implemented by the plugin's executable entry point.
pub struct ProcessHook {
    plugin: String,
    priority: i32,
    points: Vec<HookPoint>,
    dir: std::path::PathBuf,
    entry: String,
    inherit_env: bool,
}

impl ProcessHook {
    pub fn new(
        plugin: impl Into<String>,
        priority: i32,
        points: Vec<HookPoint>,
        dir: impl Into<std::path::PathBuf>,
        entry: impl Into<String>,
    ) -> Self {
        Self {
            plugin: plugin.into(),
            priority,
            points,
            dir: dir.into(),
            entry: entry.into(),
            inherit_env: false,
        }
    }

    /// Let the hook subprocess inherit the full environment; only a manifest declaring `env`
    /// may opt in. The default is the minimal allowlist.
    pub fn with_inherited_env(mut self, inherit_env: bool) -> Self {
        self.inherit_env = inherit_env;
        self
    }

    /// The command that runs `entry` for this plugin directory.
    fn command(&self) -> Result<Command, String> {
        plugin_command(&self.plugin, &self.dir, &self.entry, self.inherit_env)
    }
}

/// The command that runs `entry` inside `dir` under the env policy. Shared by process hooks and
/// process stage handlers so both spawn plugin entries identically.
pub(crate) fn plugin_command(
    plugin: &str,
    dir: &Path,
    entry: &str,
    inherit_env: bool,
) -> Result<Command, String> {
    let path = entry_path(dir, entry.trim()).map_err(|e| format!("plugin `{plugin}`: {e}"))?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut command = match extension.as_str() {
        // Interpreter dispatch is deliberate: `.sh`/`.py` hooks run through `sh`/`python3`
        // and do not need the executable bit.
        "sh" | "bash" => {
            let mut command = Command::new(if extension == "bash" { "bash" } else { "sh" });
            command.arg(&path);
            command
        }
        "py" => {
            let mut command = Command::new("python3");
            command.arg(&path);
            command
        }
        _ => Command::new(&path),
    };
    apply_env_policy(&mut command, inherit_env);
    command.current_dir(dir);
    Ok(command)
}

/// What a plugin subprocess answered: whether it exited cleanly and what it wrote to stdout.
pub(crate) struct PluginReply {
    pub status: std::process::ExitStatus,
    pub stdout: String,
}

/// The largest serialized plugin payload a subprocess is sent. Bigger payloads are refused
/// before spawn so a plugin cannot be used to push unbounded data through the stdin pipe.
const MAX_PLUGIN_PAYLOAD: usize = 256 * 1024;

/// Spawn `command` with `point` as `argv[1]`, feed `payload` on stdin and collect stdout.
///
/// Shared by process hooks and process stage handlers. A non-zero exit is reported through
/// [`PluginReply::status`], not as an error: hooks treat a failed hook as "no effect" while a
/// stage handler fails the stage, so the policy stays with the caller.
pub(crate) fn run_plugin_process(
    mut command: Command,
    point: &str,
    payload: &str,
) -> Result<PluginReply, String> {
    if payload.len() > MAX_PLUGIN_PAYLOAD {
        return Err(format!(
            "plugin payload is {} bytes, over the {MAX_PLUGIN_PAYLOAD}-byte limit",
            payload.len()
        ));
    }
    let payload = payload.to_owned();
    command
        .arg(point)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start plugin process: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // Write on a helper thread: a plugin that never reads stdin must not deadlock the turn
        // once the payload outgrows the pipe buffer. The write ends when the child exits or is
        // killed at the deadline and the pipe closes.
        std::thread::spawn(move || {
            let _ = stdin.write_all(payload.as_bytes());
        });
    }
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "plugin has no stdout".to_owned())?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    if let Some(mut stderr) = child.stderr.take() {
        // Drain stderr too: a plugin that fills it must not block on the write either.
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = stderr.read_to_end(&mut buffer);
        });
    }
    let deadline = Instant::now() + HOOK_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "plugin timed out after {}s",
                    HOOK_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("cannot wait for plugin: {e}"));
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    Ok(PluginReply {
        status,
        stdout: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

impl Hook for ProcessHook {
    fn plugin(&self) -> &str {
        &self.plugin
    }

    fn kind(&self) -> &'static str {
        "process"
    }

    fn priority(&self) -> i32 {
        self.priority
    }

    fn points(&self) -> &[HookPoint] {
        &self.points
    }

    fn describe(&self) -> String {
        self.entry.clone()
    }

    fn run(&self, ctx: &HookContext) -> Result<HookEffect, String> {
        run_process_hook(self.command()?, ctx)
    }
}

fn run_process_hook(command: Command, ctx: &HookContext) -> Result<HookEffect, String> {
    let reply = run_plugin_process(command, ctx.point.as_str(), &ctx.to_json().to_string())?;
    let text = reply.stdout;
    if text.trim().is_empty() {
        return Ok(HookEffect::default());
    }
    let value: Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("hook reply is not JSON: {e}"))?;
    HookEffect::from_json(&value)
}

/// What one hook did, for the journal and for `vikram plugins hooks`.
#[derive(Clone, Debug, PartialEq)]
pub struct HookApplication {
    pub plugin: String,
    pub effect: String,
}

/// A refusal to register a hook, kept so the operator can see why a plugin is not modifying
/// anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookRejection {
    pub plugin: String,
    pub reason: String,
}

/// The result of dispatching one context through the chain.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HookOutcome {
    pub effect: HookEffect,
    pub applied: Vec<HookApplication>,
}

/// Metadata for the active hooks, in execution order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookInfo {
    pub plugin: String,
    pub kind: String,
    pub priority: i32,
    pub points: Vec<HookPoint>,
    pub description: String,
}

/// The ordered hook chain.
#[derive(Default)]
pub struct HookRegistry {
    hooks: Vec<Arc<dyn Hook>>,
    entries: Vec<HookInfo>,
    rejected: Vec<HookRejection>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every loaded plugin's hook contributions, in priority order.
    pub fn from_plugins(plugins: &PluginRegistry) -> Self {
        let mut registry = Self::new();
        for state in plugins.plugins() {
            // Workspace-discovered plugins are listed but never trusted: only plugins installed
            // into the operator plugins dir may modify a turn.
            if !state.loaded || !state.installed {
                continue;
            }
            let Ok(manifest) = PluginManifest::from_dir(&state.path) else {
                continue;
            };
            registry.absorb_manifest(&manifest, &state.path);
        }
        registry
    }

    fn absorb_manifest(&mut self, manifest: &PluginManifest, dir: &Path) {
        let id = manifest.id().trim().to_owned();
        let declared: Vec<String> = manifest
            .components
            .as_ref()
            .and_then(|components| components.get("hooks"))
            .cloned()
            .unwrap_or_default();
        let mut points: Vec<HookPoint> = Vec::new();
        for raw in &declared {
            match HookPoint::parse(raw) {
                Some(point) if !points.contains(&point) => points.push(point),
                Some(_) => {}
                None => self.rejected.push(HookRejection {
                    plugin: id.clone(),
                    reason: format!("unknown hook point `{raw}`"),
                }),
            }
        }
        let mut rules: Vec<HookRule> = Vec::new();
        for rule in &manifest.hook_rules {
            if let Err(reason) = rule.validate() {
                self.rejected.push(HookRejection {
                    plugin: id.clone(),
                    reason,
                });
                continue;
            }
            let point = rule.point().expect("validated above");
            if !declared.is_empty() && !points.contains(&point) {
                self.rejected.push(HookRejection {
                    plugin: id.clone(),
                    reason: format!(
                        "rule targets `{}`, which `components.hooks` does not declare",
                        point.as_str()
                    ),
                });
                continue;
            }
            if !points.contains(&point) {
                points.push(point);
            }
            rules.push(rule.clone());
        }
        if points.is_empty() {
            if manifest.entry_point().is_some() {
                self.rejected.push(HookRejection {
                    plugin: id,
                    reason: "entry point is declared but no hook point is".to_owned(),
                });
            }
            return;
        }
        let granted = parse_permissions(&manifest.permissions);
        if let Some(missing) = points
            .iter()
            .find(|point| !granted.contains(&point.permission()))
        {
            self.rejected.push(HookRejection {
                plugin: id,
                reason: format!(
                    "hook point `{}` requires the `{}` permission",
                    missing.as_str(),
                    missing.permission().as_str()
                ),
            });
            return;
        }
        if !rules.is_empty() {
            self.register(Arc::new(RuleHook::new(
                id.clone(),
                manifest.priority,
                rules,
            )));
        }
        if let Some(entry) = manifest.entry_point() {
            if let Err(reason) = ensure_execution_permitted(&id, manifest) {
                self.rejected.push(HookRejection { plugin: id, reason });
                return;
            }
            if let Err(reason) = entry_path(dir, entry) {
                self.rejected.push(HookRejection {
                    plugin: id,
                    reason: format!("entry point: {reason}"),
                });
                return;
            }
            let inherit_env = granted.contains(&PluginPermission::Environment);
            self.register(Arc::new(
                ProcessHook::new(id, manifest.priority, points, dir, entry)
                    .with_inherited_env(inherit_env),
            ));
        }
    }

    /// Add a hook, restoring priority order (higher first, then plugin id).
    pub fn register(&mut self, hook: Arc<dyn Hook>) {
        self.entries.push(HookInfo {
            plugin: hook.plugin().to_owned(),
            kind: hook.kind().to_owned(),
            priority: hook.priority(),
            points: hook.points().to_vec(),
            description: hook.describe(),
        });
        self.hooks.push(hook);
        self.hooks.sort_by(|a, b| {
            b.priority()
                .cmp(&a.priority())
                .then_with(|| a.plugin().cmp(b.plugin()))
        });
    }

    pub fn hooks(&self) -> &[Arc<dyn Hook>] {
        &self.hooks
    }

    pub fn entries(&self) -> &[HookInfo] {
        &self.entries
    }

    pub fn rejected(&self) -> &[HookRejection] {
        &self.rejected
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Run the chain for `ctx`.
    ///
    /// The first denial ends the chain and is returned alone. Otherwise effects accumulate in
    /// order and the context is updated after each hook, so later hooks see earlier rewrites.
    /// A hook that panics or fails is recorded and skipped.
    pub fn dispatch(&self, ctx: &HookContext) -> HookOutcome {
        let mut current = ctx.clone();
        let mut outcome = HookOutcome::default();
        for hook in &self.hooks {
            if !hook.points().contains(&ctx.point) {
                continue;
            }
            let plugin = hook.plugin().to_owned();
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| hook.run(&current)));
            match result {
                Ok(Ok(effect)) => {
                    if effect.is_empty() {
                        continue;
                    }
                    outcome.applied.push(HookApplication {
                        plugin,
                        effect: effect.summary(),
                    });
                    apply_to_context(&mut current, &effect);
                    let denied = effect.deny.clone();
                    outcome.effect.absorb(effect);
                    if let Some(reason) = denied {
                        outcome.effect.deny = Some(reason);
                        break;
                    }
                }
                Ok(Err(error)) => outcome.applied.push(HookApplication {
                    plugin,
                    effect: format!("failed: {error}"),
                }),
                Err(_) => outcome.applied.push(HookApplication {
                    plugin,
                    effect: "panicked; ignored".to_owned(),
                }),
            }
        }
        outcome
    }
}

fn apply_to_context(ctx: &mut HookContext, effect: &HookEffect) {
    if let Some(arguments) = &effect.arguments
        && let Some(tool) = &mut ctx.tool
    {
        tool.arguments = arguments.clone();
    }
    if let Some(content) = &effect.content
        && let Some(result) = &mut ctx.result
    {
        result.content = content.clone();
    }
    if let Some(ok) = effect.ok
        && let Some(result) = &mut ctx.result
    {
        result.ok = ok;
    }
    if let Some(text) = &effect.text {
        ctx.text = text.clone();
    }
    for text in &effect.append {
        ctx.text.push('\n');
        ctx.text.push_str(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn append_hook(plugin: &str, priority: i32, text: &str) -> Arc<dyn Hook> {
        Arc::new(RuleHook::new(
            plugin,
            priority,
            vec![HookRule {
                point: "context_build".to_owned(),
                append: Some(text.to_owned()),
                ..HookRule::default()
            }],
        ))
    }

    #[test]
    fn priority_orders_the_chain_and_later_hooks_see_rewrites() {
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(RuleHook::new(
            "b-sees-rewrite",
            5,
            vec![HookRule {
                point: "context_build".to_owned(),
                text_contain: Some("two".to_owned()),
                append: Some("three".to_owned()),
                ..HookRule::default()
            }],
        )));
        registry.register(Arc::new(RuleHook::new(
            "a-rewrites",
            10,
            vec![HookRule {
                point: "context_build".to_owned(),
                text_contain: Some("one".to_owned()),
                append: Some("two".to_owned()),
                ..HookRule::default()
            }],
        )));
        registry.register(append_hook("c-prepends", 11, "one"));

        let outcome = registry.dispatch(&HookContext::new(HookPoint::ContextBuild));
        assert_eq!(outcome.effect.append, vec!["one", "two", "three"]);
        assert_eq!(
            outcome
                .applied
                .iter()
                .map(|a| a.plugin.as_str())
                .collect::<Vec<_>>(),
            vec!["c-prepends", "a-rewrites", "b-sees-rewrite"],
            "priority desc, then plugin id"
        );
    }

    #[test]
    fn a_denial_short_circuits_later_hooks() {
        let mut registry = HookRegistry::new();
        registry.register(append_hook("later", 0, "should not run"));
        registry.register(Arc::new(RuleHook::new(
            "denier",
            100,
            vec![HookRule {
                point: "tool_call".to_owned(),
                tool: Some("shell.*".to_owned()),
                arguments_contain: Some("rm -rf".to_owned()),
                deny: Some("no".to_owned()),
                ..HookRule::default()
            }],
        )));
        let context = HookContext::new(HookPoint::ToolCall)
            .with_tool("shell.run", json!({"command": "rm -rf /tmp/x"}));
        let outcome = registry.dispatch(&context);
        assert_eq!(outcome.effect.deny.as_deref(), Some("no"));
        assert_eq!(outcome.applied.len(), 1);
        assert!(outcome.effect.append.is_empty());
    }

    #[test]
    fn a_rewritten_argument_is_what_the_next_hook_judges() {
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(RuleHook::new(
            "cap-timeout",
            10,
            vec![HookRule {
                point: "tool_call".to_owned(),
                tool: Some("shell.run".to_owned()),
                arguments_set: Some(r#"{"timeout_ms":5000}"#.to_owned()),
                ..HookRule::default()
            }],
        )));
        registry.register(Arc::new(RuleHook::new(
            "sees-timeout",
            5,
            vec![HookRule {
                point: "tool_call".to_owned(),
                tool: Some("shell.run".to_owned()),
                arguments_contain: Some("timeout_ms".to_owned()),
                deny: Some("saw the rewrite".to_owned()),
                ..HookRule::default()
            }],
        )));
        let context = HookContext::new(HookPoint::ToolCall)
            .with_tool("shell.run", json!({"command": "echo hi"}));
        let outcome = registry.dispatch(&context);
        assert_eq!(outcome.effect.deny.as_deref(), Some("saw the rewrite"));
    }

    struct PanicHook;

    impl Hook for PanicHook {
        fn plugin(&self) -> &str {
            "panicky"
        }
        fn kind(&self) -> &'static str {
            "test"
        }
        fn priority(&self) -> i32 {
            50
        }
        fn points(&self) -> &[HookPoint] {
            &[HookPoint::ContextBuild]
        }
        fn describe(&self) -> String {
            "panics".to_owned()
        }
        fn run(&self, _ctx: &HookContext) -> Result<HookEffect, String> {
            panic!("hook bug")
        }
    }

    #[test]
    fn a_panicking_hook_is_isolated_and_the_chain_continues() {
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(PanicHook));
        registry.register(append_hook("survivor", 0, "still ran"));
        let outcome = registry.dispatch(&HookContext::new(HookPoint::ContextBuild));
        assert_eq!(outcome.effect.append, vec!["still ran"]);
        assert!(
            outcome.applied[0].effect.contains("panicked"),
            "the panic is reported, not swallowed silently: {:?}",
            outcome.applied
        );
        assert_eq!(registry.hooks().len(), 2);
    }

    #[test]
    fn a_process_hook_receives_the_point_and_stdin_and_returns_an_effect() {
        let dir = tempfile::tempdir().expect("temp dir");
        let script = dir.path().join("hook.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat >/dev/null\nprintf '{\"append\":\"from process %s\"}' \"$1\"\n",
        )
        .expect("write hook script");
        let hook = ProcessHook::new(
            "processy",
            10,
            vec![HookPoint::ContextBuild],
            dir.path(),
            "hook.sh",
        );
        let effect = hook
            .run(&HookContext::new(HookPoint::ContextBuild))
            .expect("process hook runs");
        assert_eq!(effect.append, vec!["from process context_build"]);
    }

    #[test]
    fn a_plugin_without_the_required_permission_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plugin = dir.path().join("plugins").join("sneaky");
        std::fs::create_dir_all(&plugin).expect("plugin dir");
        std::fs::write(
            plugin.join("plugin.json"),
            json!({
                "id": "sneaky",
                "components": {"hooks": ["tool_call"]},
                "permissions": [],
                "hook_rules": [{
                    "point": "tool_call",
                    "tool": "*",
                    "deny": "nope"
                }]
            })
            .to_string(),
        )
        .expect("manifest");
        let plugins = PluginRegistry::scan(&dir.path().join("plugins"));
        let registry = HookRegistry::from_plugins(&plugins);
        assert!(registry.hooks().is_empty(), "no hook is registered");
        assert!(
            registry
                .rejected()
                .iter()
                .any(|rejection| rejection.reason.contains("requires the `tools` permission")),
            "the refusal is recorded: {:?}",
            registry.rejected()
        );
    }

    #[test]
    fn misplaced_effects_are_rejected_at_validation() {
        let rule = HookRule {
            point: "context_build".to_owned(),
            result_contain: Some("x".to_owned()),
            ..HookRule::default()
        };
        assert!(rule.validate().is_err());
        let rule = HookRule {
            point: "tool_call".to_owned(),
            arguments_set: Some("[1]".to_owned()),
            ..HookRule::default()
        };
        assert!(rule.validate().is_err());
        let rule = HookRule {
            point: "nope".to_owned(),
            append: Some("x".to_owned()),
            ..HookRule::default()
        };
        assert!(rule.validate().is_err());
    }

    fn env_hook(dir: &Path, entry: &str) -> ProcessHook {
        ProcessHook::new("envy", 10, vec![HookPoint::TurnStart], dir, entry)
    }

    #[test]
    fn a_hook_that_never_reads_stdin_times_out_instead_of_deadlocking() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("hook.sh"), "#!/bin/sh\nsleep 30\n")
            .expect("write hook script");
        // ~100 KiB exceeds the 64 KiB pipe buffer, so a synchronous stdin write would block
        // forever against a hook that never reads.
        let ctx = HookContext::new(HookPoint::TurnStart).with_text("x".repeat(100_000));
        let started = Instant::now();
        let err = env_hook(dir.path(), "hook.sh").run(&ctx).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the deadline still bounds the hook"
        );
    }

    #[test]
    fn an_oversized_hook_context_is_refused_before_spawn() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("hook.sh"),
            "#!/bin/sh\ncat >/dev/null\nprintf '{}'\n",
        )
        .expect("write hook script");
        let ctx = HookContext::new(HookPoint::TurnStart).with_text("x".repeat(300_000));
        let err = env_hook(dir.path(), "hook.sh").run(&ctx).unwrap_err();
        assert!(err.contains("byte limit"), "{err}");
    }

    #[test]
    fn a_process_hook_does_not_inherit_secrets_without_the_env_permission() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("hook.sh"),
            "#!/bin/sh\ncat >/dev/null\n\
             if [ -n \"$VIKRAM_HOOK_SECRET_TEST\" ]; then printf '{\"append\":\"leaked\"}\\n'; \
             else printf '{}\\n'; fi\n",
        )
        .expect("write hook script");
        unsafe { std::env::set_var("VIKRAM_HOOK_SECRET_TEST", "sentinel") };

        let ctx = HookContext::new(HookPoint::TurnStart);
        let effect = env_hook(dir.path(), "hook.sh")
            .run(&ctx)
            .expect("restricted hook runs");
        assert!(effect.append.is_empty(), "{effect:?}");

        let effect = env_hook(dir.path(), "hook.sh")
            .with_inherited_env(true)
            .run(&ctx)
            .expect("env-permitted hook runs");
        assert_eq!(effect.append, vec!["leaked".to_owned()]);
        unsafe { std::env::remove_var("VIKRAM_HOOK_SECRET_TEST") };
    }

    #[test]
    fn a_process_hook_refuses_an_entry_outside_the_plugin_dir() {
        let outside = tempfile::tempdir().expect("temp dir");
        std::fs::write(outside.path().join("evil.sh"), "#!/bin/sh\nprintf '{}'\n")
            .expect("write outside script");
        let dir = tempfile::tempdir().expect("temp dir");
        let ctx = HookContext::new(HookPoint::TurnStart);
        for entry in [
            "../outside.sh".to_owned(),
            outside.path().join("evil.sh").display().to_string(),
        ] {
            let err = env_hook(dir.path(), &entry).run(&ctx).unwrap_err();
            assert!(
                err.contains("inside the plugin directory"),
                "{entry}: {err}"
            );
        }
    }

    #[test]
    fn workspace_plugins_do_not_contribute_hooks() {
        let home = tempfile::tempdir().expect("temp dir");
        let workspace = tempfile::tempdir().expect("temp dir");
        let _guard = crate::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", home.path()) };
        for (root, name) in [
            (home.path().join("plugins"), "installed"),
            (workspace.path().join("plugins"), "workspace"),
        ] {
            let plugin = root.join(name);
            std::fs::create_dir_all(&plugin).expect("plugin dir");
            std::fs::write(
                plugin.join("plugin.json"),
                json!({
                    "id": name,
                    "components": {"hooks": ["context_build"]},
                    "permissions": ["agents", "process"],
                    "entry": "hook.sh"
                })
                .to_string(),
            )
            .expect("manifest");
            std::fs::write(
                plugin.join("hook.sh"),
                "#!/bin/sh\ncat >/dev/null\nprintf '{}'\n",
            )
            .expect("hook script");
        }
        let plugins = PluginRegistry::for_workspace(workspace.path());
        let hooks = HookRegistry::from_plugins(&plugins);
        assert_eq!(hooks.hooks().len(), 1, "only the installed plugin hooks");
        assert_eq!(hooks.hooks()[0].plugin(), "installed");
        unsafe { std::env::remove_var("VIKRAM_HOME") };
    }

    #[test]
    fn every_point_round_trips_through_its_manifest_spelling() {
        for point in HookPoint::ALL {
            assert_eq!(
                HookPoint::parse(point.as_str()),
                Some(point),
                "{} must parse back to itself",
                point.as_str()
            );
        }
        assert!(HookPoint::parse("not_a_point").is_none());
    }

    #[test]
    fn details_reach_the_process_payload() {
        let ctx = HookContext::new(HookPoint::TurnEnd)
            .with_details(json!({"outcome": "idle", "steps": 3}));
        assert_eq!(ctx.to_json()["details"]["steps"], json!(3));
        assert_eq!(
            HookContext::new(HookPoint::TurnStart).to_json()["details"],
            Value::Null
        );
    }

    #[test]
    fn replace_text_is_limited_to_prose_points() {
        let rule = HookRule {
            point: "user_input".to_owned(),
            replace_text: Some("rewritten".to_owned()),
            ..HookRule::default()
        };
        assert!(rule.validate().is_ok());
        let effect = rule.effect(&HookContext::new(HookPoint::UserInput));
        assert_eq!(effect.text.as_deref(), Some("rewritten"));

        for point in ["turn_start", "context_build", "tool_call"] {
            let rule = HookRule {
                point: point.to_owned(),
                replace_text: Some("rewritten".to_owned()),
                ..HookRule::default()
            };
            assert!(rule.validate().is_err(), "{point} has no rewritable prose");
        }
    }

    #[test]
    fn hooks_at_equal_priority_run_in_plugin_id_order() {
        let mut registry = HookRegistry::new();
        registry.register(append_hook("zeta", 0, "first"));
        registry.register(append_hook("alpha", 0, "second"));
        let outcome = registry.dispatch(&HookContext::new(HookPoint::ContextBuild));
        assert_eq!(outcome.effect.append, vec!["second", "first"]);
    }
}
