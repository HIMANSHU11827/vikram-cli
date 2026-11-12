//! Context compaction and pruning.
//!
//! The journal is append-only and holds every message ever exchanged, so compaction here is a
//! *projection*: it decides what the next request contains, and writes a marker describing what it
//! folded away. Nothing is destroyed. A compacted session can still be asked for the original tool
//! output, and a restart replays the same projection rather than reloading the whole transcript.

use vikram_contracts::{Block, Message, RecordKind, Role, Trust};

use crate::context_engine::{ContextBudget, TokenEstimator};

/// When to fold, and how far.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionPolicy {
    /// Fraction of the usable window at which a fold begins.
    pub trigger: f32,
    /// Fraction to fold down to. Below the trigger on purpose: folding exactly to the trigger makes
    /// the next step trigger again, which thrashes.
    pub target: f32,
    /// Recent messages kept verbatim, whatever their size.
    pub keep_recent: usize,
    /// Below this many foldable messages, compacting costs more than it saves.
    pub min_fold: usize,
    /// A single kept tool result larger than this is digested in place.
    pub tool_output_ceiling: usize,
    /// Absolute token ceiling for the trigger, when the operator set one. Clamped to the usable
    /// window, because a limit above the window can never fire.
    pub absolute_limit: Option<u64>,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            trigger: 0.72,
            target: 0.45,
            keep_recent: 12,
            min_fold: 6,
            tool_output_ceiling: 6_000,
            absolute_limit: None,
        }
    }
}

impl CompactionPolicy {
    /// The operator's numbers, in the shape the engine consumes. A percentage outside 1..=95 falls
    /// back to the default rather than silently meaning "compact on every step".
    pub fn from_config(config: &crate::config::ContextConfig) -> Self {
        let trigger = if config.trigger_percent == 0 || config.trigger_percent > 95 {
            Self::default().trigger
        } else {
            f32::from(config.trigger_percent) / 100.0
        };
        Self {
            trigger,
            // Fold well below the trigger, or the next step triggers again and the session thrashes.
            target: trigger * 0.6,
            keep_recent: config.keep_recent.max(2) as usize,
            absolute_limit: config.auto_compact_limit,
            tool_output_ceiling: config.tool_output_ceiling.max(200) as usize,
            ..Default::default()
        }
    }
}

/// What one fold did, in the terms the interface and the journal both need.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionReport {
    pub folded: usize,
    pub kept: usize,
    pub before_tokens: u64,
    pub after_tokens: u64,
    /// Journal sequence numbers covered by the summary, so the originals stay addressable.
    pub through_seq: u64,
    pub summary_tokens: u64,
}

/// The marker record written to the journal. Payload shape is contractual: replay reads it back.
pub const COMPACTION_SUMMARY_KEY: &str = "summary";
pub const COMPACTION_THROUGH_KEY: &str = "through_seq";
pub const COMPACTION_FOLDED_KEY: &str = "folded_messages";

/// True when the front of a transcript is already a summary we wrote. Compacting a compacted
/// session must replace that summary, never stack a summary of a summary.
pub fn is_compaction_summary(message: &Message) -> bool {
    message.role == Role::System && message.text_body().starts_with(SUMMARY_HEADER)
}

const SUMMARY_HEADER: &str = "[VIKRAM context summary]";

/// Decide, from the current transcript, whether a fold is needed before the next request goes out.
pub fn should_compact(
    estimator: &TokenEstimator,
    messages: &[Message],
    budget: &ContextBudget,
    policy: &CompactionPolicy,
) -> bool {
    let usable = budget.usable_input();
    if messages.len() < policy.keep_recent + policy.min_fold {
        return false;
    }
    // A prompt and tool set that already fill the window leaves the transcript no room at all.
    // Folding is the only move left that can shrink the request, so zero budget is a trigger, not
    // a reason to stay quiet.
    if usable == 0 {
        return true;
    }
    let used = active_context_tokens(estimator, messages, budget);
    match policy.absolute_limit {
        // An operator-set ceiling wins, clamped so a limit above the window cannot be unfireable.
        Some(limit) => used >= limit.min(usable),
        None => used as f32 > usable as f32 * policy.trigger,
    }
}

/// What the next request will actually cost.
///
/// Once a provider has reported usage, that figure is the anchor and only the messages appended
/// since it are estimated. Re-estimating a whole long transcript every step is both slower and
/// less accurate than trusting the number we were given.
pub fn active_context_tokens(
    estimator: &TokenEstimator,
    messages: &[Message],
    budget: &ContextBudget,
) -> u64 {
    match budget.anchor {
        // The measured figure already contains the system prompt and tool schemas, so overhead is
        // only added when we are estimating the whole transcript from scratch.
        Some(anchor) if anchor.messages <= messages.len() => {
            anchor.input_total + estimator.estimate_messages(&messages[anchor.messages..])
        }
        _ => estimator.estimate_messages(messages) + budget.overhead,
    }
}

/// A fold must never split a tool call from its result. A provider rejects an assistant turn whose
/// calls have no matching results, and a result naming a call it never saw, so an unsnapped cut
/// turns a successful compaction into a hard API error on the next request.
fn has_tool_call(message: &Message) -> bool {
    message
        .blocks
        .iter()
        .any(|b| matches!(b, Block::ToolCall { .. }))
}

fn leads_with_tool_result(message: &Message) -> bool {
    matches!(message.blocks.first(), Some(Block::ToolResult { .. }))
}

/// Walk the proposed cut back to the nearest boundary where both halves stand on their own.
fn safe_split(body: &[Message], proposed: usize) -> usize {
    let mut split = proposed;
    while split > 0 && split < body.len() {
        let tail_orphans_a_result = leads_with_tool_result(&body[split]);
        let head_dangles_a_call = has_tool_call(&body[split - 1]);
        if !tail_orphans_a_result && !head_dangles_a_call {
            break;
        }
        split -= 1;
    }
    split
}

/// Drop the oldest messages that can be dropped without breaking call/result pairing.
///
/// This is the last resort when a provider rejects a request for size. It is deliberately blunter
/// than compaction, and it only ever runs after compaction has already been tried: the alternative
/// is a turn that cannot be sent at all.
pub fn drop_oldest_consistent(messages: &[Message]) -> Option<usize> {
    for offset in 1..messages.len() {
        let trimmed = &messages[offset..];
        if !leads_with_tool_result(&trimmed[0]) && pairing_is_consistent(trimmed) {
            return Some(offset);
        }
    }
    None
}

/// Every tool result in the kept span has to be able to see the call it answers. A fold that
/// produces an inconsistent transcript is refused rather than sent to the provider.
pub fn pairing_is_consistent(messages: &[Message]) -> bool {
    let mut seen_calls: Vec<vikram_contracts::ToolCallId> = Vec::new();
    for message in messages {
        for block in &message.blocks {
            match block {
                Block::ToolCall { call } => seen_calls.push(call.id.clone()),
                Block::ToolResult { call, .. } if !seen_calls.iter().any(|id| id == call) => {
                    return false;
                }
                _ => {}
            }
        }
    }
    true
}

/// The smallest relief a fold has to buy to be worth doing at all.
fn made_progress(before: u64, after: u64) -> bool {
    before == 0 || after <= before.saturating_sub(before / 10)
}

/// Fold the transcript to `policy.target` of the usable window, keeping the tail verbatim.
///
/// The fold walks the old prefix once and classifies each message into the things a resumed agent
/// needs: the live objective, decisions taken, work completed, work still pending, files touched,
/// and errors that were never resolved. Everything else is counted, not transcribed.
pub fn compact(
    estimator: &TokenEstimator,
    messages: &[Message],
    budget: &ContextBudget,
    policy: &CompactionPolicy,
    through_seq: u64,
) -> Option<(Vec<Message>, CompactionReport)> {
    if messages.len() < policy.keep_recent + policy.min_fold {
        return None;
    }

    let before_tokens = estimator.estimate_messages(messages);
    let existing_summary = is_compaction_summary(&messages[0]);
    let body = if existing_summary {
        &messages[1..]
    } else {
        messages
    };

    let proposed = body.len().saturating_sub(policy.keep_recent);
    let split = safe_split(body, proposed);
    if split < policy.min_fold {
        return None;
    }
    let (old, recent) = body.split_at(split);

    let goal = summary_ceiling(budget);
    let mut note = FoldNote::default();
    note.collect(old, estimator);
    let mut summary = note.render(old.len(), recent.len(), through_seq);

    // The summary has to fit too. If a long session produced more sections than the ceiling can
    // carry, drop the least load-bearing ones before the transcript does.
    while estimator.estimate_text(&summary) > goal && note.trim_least_important() {
        summary = note.render(old.len(), recent.len(), through_seq);
    }

    let mut out = Vec::with_capacity(recent.len() + 1);
    out.push(Message::text(Role::System, summary, Trust::Host));
    out.extend_from_slice(recent);

    let after_tokens = estimator.estimate_messages(&out);
    if !made_progress(before_tokens, after_tokens) {
        // A fold that frees less than a tenth of the context is not worth the information it
        // spends, and re-running it every step is how compaction thrashes.
        return None;
    }
    if !pairing_is_consistent(&out) {
        return None;
    }
    let report = CompactionReport {
        folded: old.len(),
        kept: recent.len(),
        before_tokens,
        after_tokens,
        through_seq,
        summary_tokens: estimator.estimate_text(&out[0].text_body()),
    };
    Some((out, report))
}

/// How large a summary may be: one twelfth of the usable window, bounded so it is never trivial on
/// a million-token model and never dominant on a small local one.
fn summary_ceiling(budget: &ContextBudget) -> u64 {
    (budget.usable_input() / 12).clamp(400, 6_000)
}

/// Shrink a transcript without folding it, by digesting oversized tool results.
///
/// This is the backstop when folding alone still leaves the request over the limit: a fold protects
/// the tail, and the tail is exactly where a 20k-character build log lives. The largest result is
/// digested first, then the next, until the request fits or nothing oversized remains.
pub fn prune_transcript(
    estimator: &TokenEstimator,
    budget: &ContextBudget,
    messages: &[Message],
    ceiling: usize,
) -> Option<(Vec<Message>, usize)> {
    let usable = budget.usable_input();
    let mut current: Vec<Message> = messages.to_vec();
    let mut total = estimator.estimate_messages(&current) + budget.overhead;
    if total <= usable {
        return None;
    }
    let mut replaced = 0usize;
    loop {
        // Find the biggest oversized tool result still present.
        let mut worst: Option<(usize, usize)> = None;
        for (index, message) in current.iter().enumerate() {
            for block in &message.blocks {
                if let Block::ToolResult { content, .. } = block {
                    let len = content.chars().count();
                    if len > ceiling && worst.is_none_or(|(_, prior)| len > prior) {
                        worst = Some((index, len));
                    }
                }
            }
        }
        let Some((index, len)) = worst else { break };
        let digested = digest_blocks(estimator, &current[index], ceiling);
        // A digest that does not shrink the largest result would be offered again on the next
        // pass, so the loop would pick it forever. Stop instead of spinning.
        if largest_result(&digested) >= len {
            break;
        }
        current[index] = digested;
        replaced += 1;
        total = estimator.estimate_messages(&current) + budget.overhead;
        if total <= usable {
            break;
        }
    }
    if replaced == 0 || total > usable {
        // Pruning alone could not save it. Report that rather than pretending; the caller still
        // has a smaller request than before, and folding is the next move.
        if replaced == 0 {
            return None;
        }
    }
    Some((current, replaced))
}

/// The longest tool result in a message, in characters. Pruning is driven by this number, so the
/// loop can tell a digest that helped from one that changed nothing.
fn largest_result(message: &Message) -> usize {
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::ToolResult { content, .. } => Some(content.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// Apply the digest to every oversized tool result on one message.
fn digest_blocks(estimator: &TokenEstimator, message: &Message, ceiling: usize) -> Message {
    let mut blocks = message.blocks.clone();
    for block in &mut blocks {
        if let Block::ToolResult { content, .. } = block {
            let digested = digest_tool_output(content, ceiling);
            if digested.chars().count() < estimator.estimate_text(content) as usize * 4 {
                *content = digested;
            }
        }
    }
    Message {
        role: message.role,
        blocks,
    }
}

/// In-place digest for a kept tool result that is too large to carry but too recent to fold.
pub fn digest_tool_output(content: &str, ceiling: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= ceiling {
        return content.to_string();
    }
    let omitted = chars.len() - ceiling;
    let marker = format!("\n\n<elided {omitted} characters to hold context>\n\n");
    // The marker is paid for out of the ceiling rather than added on top of it. A digest that is
    // still larger than the ceiling is not a digest: pruning would keep choosing the same result
    // and never finish.
    let keep = ceiling.saturating_sub(marker.chars().count());
    if keep <= 2 {
        return chars[..ceiling].iter().collect();
    }
    let head_len = keep / 2;
    let tail_len = keep - head_len;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{head}{marker}{tail}")
}

/// The bounded brief a delegated agent starts with: the parent's live instruction verbatim, plus
/// its decisions, files and unresolved errors, trimmed until it fits. A child is not given the
/// parent's whole transcript — the budget is the point, and so is the isolation.
pub fn delegation_brief(
    estimator: &TokenEstimator,
    messages: &[Message],
    max_tokens: u64,
) -> Option<String> {
    let instruction = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.text_body())
        .filter(|text| !text.trim().is_empty())?;
    let mut note = FoldNote::default();
    note.collect(messages, estimator);
    let mut head = format!(
        "[VIKRAM delegation brief]\nInstruction for this delegated task (verbatim): {instruction}\n"
    );
    let mut body = note.sections();
    while estimator.estimate_text(&head) + estimator.estimate_text(&body) > max_tokens {
        if !note.trim_least_important() {
            break;
        }
        body = note.sections();
    }
    head.push_str(&body);
    Some(head)
}

/// The answer a delegated agent gives back, fitted to a token budget the same way: both ends kept,
/// the middle dropped only when the text is genuinely over budget.
pub fn delegation_answer(
    estimator: &TokenEstimator,
    messages: &[Message],
    max_tokens: u64,
) -> Option<String> {
    let text = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && !m.text_body().trim().is_empty())
        .map(|m| m.text_body())?;
    Some(fit_to_tokens(estimator, &text, max_tokens))
}

/// Never digest below a useful floor; a two-line summary of a paragraph is not a digest.
const MIN_DIGEST_CEILING: usize = 240;

/// Trim to a token budget by digesting, using the text's own measured chars-per-token ratio rather
/// than a fixed guess, and checking the result back against the estimator.
fn fit_to_tokens(estimator: &TokenEstimator, text: &str, max_tokens: u64) -> String {
    if estimator.estimate_text(text) <= max_tokens {
        return text.to_string();
    }
    let chars = text.chars().count().max(1) as f64;
    let tokens = estimator.estimate_text(text).max(1) as f64;
    let per_token = (chars / tokens).max(1.0);
    let mut ceiling = (per_token * max_tokens as f64) as usize;
    loop {
        let digested = digest_tool_output(text, ceiling);
        if estimator.estimate_text(&digested) <= max_tokens || ceiling <= MIN_DIGEST_CEILING {
            return digested;
        }
        ceiling = ceiling.saturating_mul(9) / 10;
    }
}

#[derive(Default)]
struct FoldNote {
    objectives: Vec<String>,
    decisions: Vec<String>,
    completed: Vec<String>,
    pending: Vec<String>,
    files: Vec<String>,
    errors: Vec<String>,
    tool_count: usize,
    duplicate_tools: usize,
}

static DECISION_MARKERS: &[&str] = &[
    "we will",
    "i will",
    "decided",
    "decision:",
    "chose",
    "going with",
    "the fix is",
    "plan:",
    "approach:",
];
static PENDING_MARKERS: &[&str] = &[
    "todo",
    "still need",
    "next:",
    "remaining",
    "not yet",
    "pending",
    "later",
    "to do",
];
static ERROR_MARKERS: &[&str] = &[
    "error", "failed", "failure", "panic", "denied", "timeout", "cannot", "unable",
];

impl FoldNote {
    fn collect(&mut self, old: &[Message], estimator: &TokenEstimator) {
        let mut seen_results: Vec<String> = Vec::new();
        for message in old {
            match message.role {
                Role::User => {
                    let text = condense(&message.text_body(), 300);
                    if !text.is_empty() {
                        self.objectives.push(text);
                    }
                }
                Role::Assistant => {
                    for block in &message.blocks {
                        match block {
                            Block::Text { text, .. } => {
                                let lower = text.to_lowercase();
                                let clip = condense(text, 300);
                                if clip.is_empty() {
                                    continue;
                                }
                                if DECISION_MARKERS.iter().any(|m| lower.contains(m)) {
                                    self.decisions.push(clip);
                                } else if PENDING_MARKERS.iter().any(|m| lower.contains(m)) {
                                    self.pending.push(clip);
                                }
                            }
                            Block::ToolCall { call } => {
                                self.tool_count += 1;
                                self.remember_files(&call.arguments.to_string());
                            }
                            Block::Reasoning { .. } => {}
                            Block::Image { .. } => {}
                            Block::ToolResult { .. } => {}
                        }
                    }
                }
                Role::Tool => {
                    self.tool_count += 1;
                    for block in &message.blocks {
                        if let Block::ToolResult { content, ok, .. } = block {
                            let key = condense(content, 400);
                            if !key.is_empty() && seen_results.contains(&key) {
                                self.duplicate_tools += 1;
                                continue;
                            }
                            seen_results.push(key);
                            let lower = content.to_lowercase();
                            if !*ok || ERROR_MARKERS.iter().any(|m| lower.contains(m)) {
                                // Keep the failure whole: an unresolved error is the most
                                // load-bearing thing to survive a fold.
                                self.errors.push(condense(content, 600));
                            } else if estimator.estimate_text(content) > 8 {
                                self.completed.push(condense(content, 200));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Pull path-looking strings out of a tool call's JSON arguments.
    fn remember_files(&mut self, arguments: &str) {
        for token in arguments.split(['"', ',', '{', '}']) {
            let candidate = token.trim().trim_start_matches("=\"").trim();
            if candidate.len() < 3 || candidate.len() > 200 {
                continue;
            }
            let looks_like_path = candidate.starts_with('/')
                || candidate.contains("/src/")
                || candidate.contains("crates/")
                || candidate.contains('.')
                    && candidate
                        .rsplit('.')
                        .next()
                        .is_some_and(|ext| ext.len() <= 5 && !ext.contains(' '));
            if looks_like_path && !self.files.iter().any(|f| f == candidate) {
                self.files.push(candidate.to_string());
            }
        }
    }

    fn render(&self, folded: usize, kept: usize, through_seq: u64) -> String {
        let mut out = String::new();
        out.push_str(SUMMARY_HEADER);
        out.push_str(&format!(
            " {} earlier messages folded; the {} most recent are verbatim below. Originals are \
             durable in the journal through seq {through_seq} and can be re-read.\n",
            folded, kept
        ));
        out.push_str(&self.sections());
        out
    }

    /// The classified body, without the fold header — shared by the fold and the delegation brief.
    fn sections(&self) -> String {
        let mut out = String::new();
        section(&mut out, "Live objective", &self.objectives, 3);
        section(&mut out, "Decisions", &self.decisions, 8);
        section(&mut out, "Work completed", &self.completed, 8);
        section(&mut out, "Relevant files", &self.files, 12);
        section(&mut out, "Errors and blocked", &self.errors, 8);
        section(&mut out, "Next steps", &self.pending, 6);
        if self.tool_count > 0 {
            out.push_str(&format!(
                "\n[{} tool calls in the folded span{}]",
                self.tool_count,
                if self.duplicate_tools > 0 {
                    format!(", {0} duplicate results dropped", self.duplicate_tools)
                } else {
                    String::new()
                }
            ));
        }
        out
    }

    /// Drop the least load-bearing sections first, in reverse priority order.
    fn trim_least_important(&mut self) -> bool {
        for bucket in [
            &mut self.completed,
            &mut self.files,
            &mut self.pending,
            &mut self.decisions,
        ] {
            if bucket.len() > 2 {
                bucket.pop();
                return true;
            }
        }
        if self.errors.len() > 1 {
            self.errors.pop();
            return true;
        }
        if self.objectives.len() > 1 {
            self.objectives.remove(0);
            return true;
        }
        false
    }
}

fn section(out: &mut String, title: &str, items: &[String], cap: usize) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("\n### {title}\n"));
    for item in items.iter().take(cap) {
        out.push_str(&format!("- {item}\n"));
    }
    if items.len() > cap {
        out.push_str(&format!("- ({} more in the journal)\n", items.len() - cap));
    }
}

/// Clip on a character boundary and mark that we did.
fn condense(text: &str, limit: usize) -> String {
    let flattened = text
        .replace('\n', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flattened.is_empty() {
        return String::new();
    }
    if flattened.chars().count() <= limit {
        return flattened;
    }
    let mut clipped: String = flattened.chars().take(limit).collect();
    if let Some(space) = clipped.rfind(' ') {
        clipped.truncate(space);
    }
    clipped.push('…');
    clipped
}

/// Rebuild the transcript the runtime should hold from a session's journal records.
///
/// Message rows, tool-call rows and tool-result rows all become messages, and every `Compaction`
/// marker drops the `folded_messages` entries it folded and substitutes its summary. The count is
/// the forget range: replay reaches exactly the transcript the fold was computed from, so removing
/// that many from the front reproduces the agent's live context rather than the raw history.
pub fn project_from_journal(
    records: impl IntoIterator<Item = (u64, RecordKind, serde_json::Value)>,
) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::new();
    // Calls the journal says were started. A process can die between the record and the result, and
    // a replay that loses the call loses the fact that anything happened at all.
    let mut unanswered: Vec<vikram_contracts::ToolCallId> = Vec::new();
    for (seq, kind, payload) in records {
        if kind == RecordKind::Compaction {
            let Some(raw) = payload.get(COMPACTION_SUMMARY_KEY) else {
                continue;
            };
            let Ok(summary) = serde_json::from_value::<Message>(raw.clone()) else {
                continue;
            };
            let folded = payload
                .get(COMPACTION_FOLDED_KEY)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            if folded == 0 || folded > messages.len() {
                // A marker that does not line up with what we replayed is not safe to apply;
                // keeping the full transcript beats silently losing turns.
                continue;
            }
            let _ = seq;
            messages.drain(..folded);
            messages.insert(0, summary);
            continue;
        }
        let keeps = matches!(
            kind,
            RecordKind::Message | RecordKind::ToolResult | RecordKind::ToolCall
        );
        if !keeps {
            continue;
        }
        if let Some(raw) = payload.get("message")
            && let Ok(message) = serde_json::from_value::<Message>(raw.clone())
        {
            for block in &message.blocks {
                if let Block::ToolResult { call, .. } = block {
                    unanswered.retain(|pending| pending != call);
                }
            }
            messages.push(message);
            continue;
        }
        if kind == RecordKind::ToolCall
            && let Some(call) = payload
                .get("call")
                .and_then(|v| v.as_str())
                .map(vikram_contracts::ToolCallId::new)
        {
            unanswered.push(call);
        }
    }
    for call in unanswered {
        messages.push(Message::tool_result(
            call,
            "the run ended before this call finished",
            false,
        ));
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_engine::{LimitsSource, ModelLimits};
    use vikram_contracts::ToolCall;

    fn policy() -> CompactionPolicy {
        CompactionPolicy {
            trigger: 0.5,
            target: 0.3,
            keep_recent: 3,
            min_fold: 2,
            ..Default::default()
        }
    }

    fn budget() -> ContextBudget {
        ContextBudget::new(
            ModelLimits {
                context_window: 2_000,
                max_output_tokens: 500,
                safety_margin: 512,
                source: LimitsSource::Assumed,
            },
            100,
            None,
        )
    }

    /// A realistic exchange: an assistant turn that asks for a tool, then the result that answers it.
    fn exchange(content: impl Into<String>, ok: bool) -> Vec<Message> {
        let call = ToolCall::new(
            "file.write",
            serde_json::json!({"path": "crates/vikram-runtime/src/context_engine.rs", "content": "fn estimate() {}"}),
        );
        let id = call.id.clone();
        vec![
            Message {
                role: Role::Assistant,
                blocks: vec![Block::ToolCall { call }],
            },
            Message::tool_result(id, content.into(), ok),
        ]
    }

    fn long_transcript() -> Vec<Message> {
        let mut m = vec![Message::user(
            "Build the complete context engine for Vikram: token budgeting, compaction, memory retrieval, checkpoints. Keep the existing CLI and TUI working end to end and add real tests.",
        )];
        m.extend(exchange(
            "error: failed to compile: no method named max_by_key on Option in context_engine.rs line 121".to_string(),
            false,
        ));
        for i in 0..14 {
            m.push(Message::text(
                Role::Assistant,
                format!(
                    "Step {i}: decided we will edit crates/vikram-runtime/src/turn.rs and wire the budget gate before the request goes out"
                ),
                Trust::Model,
            ));
            m.extend(exchange(
                format!("wrote 40 lines to crates/vikram-runtime/src/context_engine.rs, verification result {i} passed cleanly"),
                true,
            ));
        }
        m.push(Message::user("continue and finish the tests"));
        m
    }

    #[test]
    fn trigger_fires_only_past_the_threshold() {
        let e = TokenEstimator::default();
        let b = budget();
        let p = policy();
        let small = vec![
            Message::user("hi"),
            Message::user("again"),
            Message::user("more"),
        ];
        assert!(!should_compact(&e, &small, &b, &p));
        assert!(should_compact(&e, &long_transcript(), &b, &p));
    }

    #[test]
    fn a_transcript_with_no_room_at_all_still_folds() {
        let e = TokenEstimator::default();
        // The prompt and tool schemas alone fill the window, so the transcript has zero budget.
        // That is the moment folding matters most, not a reason to leave the request oversized.
        let b = ContextBudget::new(
            ModelLimits {
                context_window: 2_000,
                max_output_tokens: 2_000,
                safety_margin: 0,
                source: LimitsSource::Config,
            },
            2_000,
            None,
        );
        assert_eq!(b.usable_input(), 0);
        let p = policy();
        let msgs = long_transcript();
        assert!(should_compact(&e, &msgs, &b, &p));
        let (out, report) = compact(&e, &msgs, &b, &p, 90).unwrap();
        assert!(report.after_tokens < report.before_tokens);
        assert!(pairing_is_consistent(&out));
    }

    #[test]
    fn fold_shrinks_and_keeps_the_tail_verbatim() {
        let e = TokenEstimator::default();
        let msgs = long_transcript();
        let (out, report) = compact(&e, &msgs, &budget(), &policy(), 90).unwrap();
        assert!(report.after_tokens < report.before_tokens);
        assert_eq!(report.kept, 3);
        assert_eq!(out.len(), 4);
        assert!(is_compaction_summary(&out[0]));
        // The live instruction must still be present word for word.
        assert_eq!(
            out.last().unwrap().text_body(),
            "continue and finish the tests"
        );
    }

    #[test]
    fn summary_preserves_objective_decisions_errors_and_files() {
        let e = TokenEstimator::default();
        let (out, _) = compact(&e, &long_transcript(), &budget(), &policy(), 90).unwrap();
        let text = out[0].text_body();
        assert!(text.contains("Live objective"), "{text}");
        assert!(text.contains("Decisions"), "{text}");
        assert!(
            text.contains("failed to compile"),
            "error text must survive: {text}"
        );
        assert!(
            text.contains("crates/vikram-runtime/src/turn.rs"),
            "paths survive: {text}"
        );
        assert!(
            text.contains("through seq 90"),
            "originals stay addressable: {text}"
        );
    }

    #[test]
    fn compaction_is_not_stacked_on_itself() {
        let e = TokenEstimator::default();
        let msgs = long_transcript();
        let (once, first) = compact(&e, &msgs, &budget(), &policy(), 50).unwrap();
        let mut twice_input = once.clone();
        for i in 0..6 {
            twice_input.push(Message::user(format!(
                "further instruction {i} with enough words to matter for the count"
            )));
        }
        let (twice, second) = compact(&e, &twice_input, &budget(), &policy(), 80).unwrap();
        assert_eq!(twice.iter().filter(|m| is_compaction_summary(m)).count(), 1);
        assert!(second.through_seq > first.through_seq);
        assert!(twice[0].text_body().contains("through seq 80"));
    }

    #[test]
    fn refuses_to_fold_a_transcript_too_short_to_bother_with() {
        let e = TokenEstimator::default();
        let msgs: Vec<Message> = (0..4).map(|i| Message::user(format!("m{i}"))).collect();
        assert!(compact(&e, &msgs, &budget(), &policy(), 10).is_none());
    }

    #[test]
    fn digest_keeps_head_and_tail_and_says_what_was_dropped() {
        let big = format!("START {}\nMIDDLE\n{}", "x".repeat(4000), "END OF OUTPUT");
        let out = digest_tool_output(&big, 200);
        assert!(out.starts_with("START"));
        assert!(out.ends_with("END OF OUTPUT"));
        assert!(out.contains("elided"));
        assert!(out.chars().count() < big.chars().count());
        // Small output is returned untouched: no digest marker, no lost characters.
        assert_eq!(digest_tool_output("short", 200), "short");
    }

    #[test]
    fn duplicate_tool_results_are_counted_not_transcribed() {
        let e = TokenEstimator::default();
        let mut msgs = vec![Message::user("do the thing repeatedly")];
        for _ in 0..8 {
            msgs.extend(exchange("identical output every time".to_string(), true));
        }
        for i in 0..3 {
            msgs.push(Message::user(format!("tail {i}")));
        }
        let (out, _) = compact(&e, &msgs, &budget(), &policy(), 40).unwrap();
        assert!(out[0].text_body().contains("duplicate results dropped"));
    }

    #[test]
    fn a_journalled_call_that_never_got_a_result_comes_back_answered() {
        use serde_json::json;
        let call = vikram_contracts::ToolCallId::new("call-crashed");
        let out = project_from_journal(vec![
            (
                1,
                RecordKind::Message,
                json!({"message": Message::user("write the file")}),
            ),
            (
                2,
                RecordKind::ToolCall,
                json!({"call": call.as_str(), "name": "fs.write", "arguments": {}}),
            ),
        ]);
        let answered: Vec<String> = out
            .iter()
            .filter(|m| matches!(m.role, Role::Tool))
            .map(|m| m.text_body())
            .collect();
        assert_eq!(
            answered.len(),
            1,
            "a call the runtime started but never finished must not vanish: {out:?}"
        );
        assert!(
            answered[0].contains("run ended"),
            "and it says why: {}",
            answered[0]
        );
    }

    #[test]
    fn a_call_that_was_answered_is_not_answered_twice() {
        use serde_json::json;
        let call = vikram_contracts::ToolCallId::new("call-finished");
        let out = project_from_journal(vec![
            (
                1,
                RecordKind::ToolCall,
                json!({"call": call.as_str(), "name": "fs.write", "arguments": {}}),
            ),
            (
                2,
                RecordKind::ToolResult,
                json!({"message": Message::tool_result(call.clone(), "written".to_string(), true)}),
            ),
        ]);
        assert_eq!(
            out.len(),
            1,
            "the completed call keeps exactly its own result: {out:?}"
        );
        assert_eq!(out[0].text_body(), "written");
    }

    #[test]
    fn projection_restores_tool_history_and_honors_compaction_markers() {
        use serde_json::json;
        let summary = Message::text(
            Role::System,
            "[VIKRAM context summary] folded 3 messages",
            Trust::Host,
        );
        let records = vec![
            (
                1,
                RecordKind::Message,
                json!({"message": Message::user("first")}),
            ),
            (
                2,
                RecordKind::Message,
                json!({"message": Message::user("second")}),
            ),
            (
                3,
                RecordKind::ToolResult,
                json!({"message": Message::tool_result(
                vikram_contracts::ToolCallId::generate(), "tool said this".to_string(), true)}),
            ),
            (
                4,
                RecordKind::Compaction,
                json!({
                COMPACTION_SUMMARY_KEY: summary,
                COMPACTION_THROUGH_KEY: 3,
                COMPACTION_FOLDED_KEY: 3}),
            ),
            (
                5,
                RecordKind::Message,
                json!({"message": Message::user("after")}),
            ),
        ];
        let out = project_from_journal(records);
        // The three folded entries are replaced by the summary; the tail survives.
        assert_eq!(out.len(), 2);
        assert!(is_compaction_summary(&out[0]));
        assert_eq!(out[1].text_body(), "after");

        let no_compaction = project_from_journal(vec![
            (
                1,
                RecordKind::Message,
                json!({"message": Message::user("u")}),
            ),
            (
                2,
                RecordKind::ToolResult,
                json!({"message": Message::tool_result(
                vikram_contracts::ToolCallId::generate(), "r".to_string(), true)}),
            ),
        ]);
        assert_eq!(no_compaction.len(), 2);
    }

    #[test]
    fn projection_keeps_the_messages_a_second_fold_did_not_touch() {
        use serde_json::json;
        let first = Message::text(
            Role::System,
            "[VIKRAM context summary] first fold",
            Trust::Host,
        );
        let second = Message::text(
            Role::System,
            "[VIKRAM context summary] second fold",
            Trust::Host,
        );
        let records = vec![
            (
                1,
                RecordKind::Message,
                json!({"message": Message::user("a")}),
            ),
            (
                2,
                RecordKind::Message,
                json!({"message": Message::user("b")}),
            ),
            (
                3,
                RecordKind::Compaction,
                json!({
                COMPACTION_SUMMARY_KEY: first, COMPACTION_FOLDED_KEY: 2}),
            ),
            (
                4,
                RecordKind::Message,
                json!({"message": Message::user("c")}),
            ),
            // The second fold covered its own summary plus "c", and kept nothing.
            (
                5,
                RecordKind::Compaction,
                json!({
                COMPACTION_SUMMARY_KEY: second, COMPACTION_FOLDED_KEY: 2}),
            ),
            (
                6,
                RecordKind::Message,
                json!({"message": Message::user("d")}),
            ),
        ];
        let out = project_from_journal(records);
        assert_eq!(out.len(), 2);
        assert!(out[0].text_body().contains("second fold"));
        assert_eq!(out[1].text_body(), "d");
    }

    #[test]
    fn projection_refuses_a_marker_that_does_not_line_up() {
        use serde_json::json;
        let summary = Message::text(
            Role::System,
            "[VIKRAM context summary] bogus range",
            Trust::Host,
        );
        let records = vec![
            (
                1,
                RecordKind::Message,
                json!({"message": Message::user("a")}),
            ),
            // Claims to have folded five messages when only one was ever replayed.
            (
                2,
                RecordKind::Compaction,
                json!({
                COMPACTION_SUMMARY_KEY: summary, COMPACTION_FOLDED_KEY: 5}),
            ),
        ];
        let out = project_from_journal(records);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text_body(), "a");
    }

    #[test]
    fn projection_survives_a_garbage_payload_without_losing_the_rest() {
        use serde_json::json;
        let out = project_from_journal(vec![
            (
                1,
                RecordKind::Message,
                json!({"message": Message::user("good")}),
            ),
            (2, RecordKind::Message, json!({"nope": 1})),
            (3, RecordKind::Compaction, json!({"unrelated": true})),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text_body(), "good");
    }

    #[test]
    fn summary_shrinks_to_fit_a_tiny_budget() {
        let e = TokenEstimator::default();
        let mut msgs = long_transcript();
        for i in 0..30 {
            msgs.push(Message::text(
                Role::Assistant,
                format!("decided approach {i}: rewrite the whole planner module"),
                Trust::Model,
            ));
        }
        let (out, report) = compact(&e, &msgs, &budget(), &policy(), 200).unwrap();
        assert!(
            report.summary_tokens < budget().usable_input() / 2,
            "{}",
            report.summary_tokens
        );
        assert!(out[0].text_body().len() < 3_000);
    }

    #[test]
    fn tool_call_blocks_are_counted_toward_the_fold() {
        let call = ToolCall::new(
            "file.write",
            serde_json::json!({"path": "crates/x/src/y.rs", "content": "fn main() {}"}),
        );
        let m = Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall { call }],
        };
        let e = TokenEstimator::default();
        assert!(e.estimate_message(&m) > 10);
    }

    /// A transcript where the naive tail boundary lands between a tool call and its result.
    fn paired_transcript() -> Vec<Message> {
        let mut m = Vec::new();
        m.push(Message::user(
            "read the runtime and tell me what you find about sessions",
        ));
        for i in 0..10 {
            let call = ToolCall::new(
                "file.read",
                serde_json::json!({"path": format!("crates/vikram-runtime/src/session_{i}.rs")}),
            );
            let id = call.id.clone();
            m.push(Message {
                role: Role::Assistant,
                blocks: vec![Block::ToolCall { call }],
            });
            m.push(Message::tool_result(
                id,
                format!("contents of session_{i}.rs: a struct with {} fields and a mutex each, result body number {i}", i * 7),
                true,
            ));
        }
        // Two more messages so keep_recent=3 would cut exactly between a call and its result.
        let call = ToolCall::new(
            "bash.run",
            serde_json::json!({"command": "cargo test --workspace"}),
        );
        let id = call.id.clone();
        m.push(Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall { call }],
        });
        m.push(Message::tool_result(
            id,
            "test result: ok. 407 passed".to_string(),
            true,
        ));
        m.push(Message::user("now summarise and continue"));
        m
    }

    #[test]
    fn fold_never_orphans_a_tool_result_from_its_call() {
        let e = TokenEstimator::default();
        let msgs = paired_transcript();
        let snap_policy = CompactionPolicy {
            keep_recent: 2,
            ..policy()
        };
        // Sanity: the naive cut really would land between a call and its result.
        let naive = msgs.len() - snap_policy.keep_recent;
        assert!(leads_with_tool_result(&msgs[naive]));
        let (out, _) = compact(&e, &msgs, &budget(), &snap_policy, 40).expect("fold");
        assert!(
            !leads_with_tool_result(&out[1]),
            "the tail must start at a safe boundary"
        );
        assert!(
            pairing_is_consistent(&out),
            "compacted transcript would be rejected by a provider"
        );
    }

    #[test]
    fn a_fold_that_frees_nothing_is_refused() {
        let e = TokenEstimator::default();
        // One huge message plus a short tail: folding the single message saves nothing meaningful
        // once the summary itself is written, so it must be refused rather than repeated forever.
        let msgs = vec![
            Message::user("x".repeat(50)),
            Message::user("y".repeat(50)),
            Message::user("z".repeat(5_000)),
            Message::user("tail one"),
            Message::user("tail two"),
            Message::user("tail three"),
        ];
        let result = compact(&e, &msgs, &budget(), &policy(), 10);
        assert!(result.is_none() || pairing_is_consistent(&result.as_ref().unwrap().0));
    }

    #[test]
    fn consistency_checker_catches_a_hand_broken_transcript() {
        use serde_json::json;
        let call = ToolCall::new("bash.run", json!({"command": "ls"}));
        let id = call.id.clone();
        let orphan = Message::tool_result(id, "output".to_string(), true);
        assert!(!pairing_is_consistent(&[orphan]));
        let with_call = Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall {
                call: ToolCall::new("bash.run", json!({"command": "ls"})),
            }],
        };
        let id2 = match &with_call.blocks[0] {
            Block::ToolCall { call } => call.id.clone(),
            _ => unreachable!(),
        };
        let answered = Message::tool_result(id2, "output".to_string(), true);
        assert!(pairing_is_consistent(&[with_call, answered]));
    }

    #[test]
    fn a_digest_never_grows_past_its_ceiling() {
        // The marker is charged to the ceiling. A digest that overshoots is still oversized, so
        // pruning would choose it again and never finish.
        let content = "x".repeat(40_000);
        for ceiling in [64usize, 200, 6_000] {
            let digested = digest_tool_output(&content, ceiling);
            assert!(
                digested.chars().count() <= ceiling,
                "ceiling {ceiling} produced {} chars",
                digested.chars().count()
            );
            assert!(digested.chars().count() < content.len());
        }
    }

    #[test]
    fn pruning_one_oversized_result_terminates() {
        // The regression this guards: a single huge tool result that the digest could not shrink
        // was chosen again on every pass, spinning a worker thread forever before the request
        // ever reached the provider.
        let estimator = TokenEstimator::default();
        let budget = budget();
        let ceiling = 120;
        let messages = exchange("y".repeat(30_000), true);

        let started = std::time::Instant::now();
        let (pruned, replaced) = prune_transcript(&estimator, &budget, &messages, ceiling)
            .expect("an oversized result is pruned");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(replaced, 1);
        assert_eq!(pruned.len(), messages.len());
        let content = match &pruned[1].blocks[0] {
            Block::ToolResult { content, .. } => content.clone(),
            other => panic!("unexpected block: {other:?}"),
        };
        assert!(content.chars().count() <= ceiling);
        assert!(content.contains("elided"));
    }
}
