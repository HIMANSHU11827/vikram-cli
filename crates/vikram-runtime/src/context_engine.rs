//! Token accounting, model limits and context budgeting.
//!
//! Three deliberately separate ideas live here. An *estimate* is what the runtime can know before a
//! call is billed. A *reported* figure is what the provider said it cost. A *limit* is what the
//! model accepts. Mixing them up is how a harness ends up with numbers nobody trusts, so each
//! carries its provenance all the way to the interface.

use vikram_contracts::{Block, Message, Role};

/// Where a context-window figure came from. The interface shows this, because a limit the operator
/// typed and a limit we assumed are not equally trustworthy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitsSource {
    /// `context_window` in the operator's config.
    Config,
    /// Advertised by the provider itself.
    Provider,
    /// Looked up in [`ModelLimits::registry`] for the active model name.
    Registry,
    /// Nothing knew, so we use a deliberately small floor. Compaction stays aggressive and the
    /// meter is labelled as an assumption rather than a measurement.
    Assumed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelLimits {
    pub context_window: u64,
    pub max_output_tokens: u64,
    /// Held back from every budget so a near-miss estimate cannot produce a hard provider error.
    pub safety_margin: u64,
    pub source: LimitsSource,
}

/// The conservative floor used when no source knows the model's window. Small enough that staying
/// under it keeps long sessions working on any current model.
pub const ASSUMED_CONTEXT_WINDOW: u64 = 32_768;
pub const ASSUMED_MAX_OUTPUT: u64 = 4_096;

/// One row of the declared model table. Keys are matched as case-insensitive prefixes, longest
/// first, so `gpt-4o-mini` is not swallowed by `gpt-4o`.
pub const REGISTRY: &[(&str, u64, u64)] = &[
    ("gpt-4o", 128_000, 16_384),
    ("gpt-4.1", 1_000_000, 32_768),
    ("gpt-4-turbo", 128_000, 16_384),
    ("gpt-4", 8_192, 8_192),
    ("gpt-3.5", 16_385, 4_096),
    ("o1", 200_000, 100_000),
    ("o3-mini", 200_000, 100_000),
    ("o3", 200_000, 100_000),
    ("o4-mini", 200_000, 100_000),
    ("claude-opus-4", 200_000, 32_000),
    ("claude-sonnet-4", 200_000, 64_000),
    ("claude-haiku", 200_000, 8_192),
    ("claude-3-7-sonnet", 200_000, 64_000),
    ("claude-3-5", 200_000, 8_192),
    ("claude-3", 200_000, 8_192),
    ("gemini-2.5-pro", 1_048_576, 65_536),
    ("gemini-2.5-flash", 1_048_576, 65_536),
    ("gemini-2.0", 1_048_576, 8_192),
    ("gemini-1.5", 1_048_576, 8_192),
    ("llama-3.1", 128_000, 4_096),
    ("llama-3.2", 128_000, 4_096),
    ("llama-4", 1_000_000, 4_096),
    ("qwen2.5", 128_000, 8_192),
    ("qwen3", 128_000, 8_192),
    ("deepseek-chat", 128_000, 8_192),
    ("deepseek-reasoner", 128_000, 64_000),
    ("mistral-large", 128_000, 8_192),
    ("devstral", 128_000, 8_192),
    ("phi-4", 16_384, 4_096),
    ("gemma-3", 128_000, 4_096),
];

impl ModelLimits {
    pub fn resolve(model: &str, configured: Option<u64>, advertised: Option<u64>) -> Self {
        Self::resolve_with_margin(model, configured, advertised, 8)
    }

    /// Resolve the window for a model name. Order is fixed: the operator's configured value, then
    /// what the provider advertised, then the declared registry, then the assumed floor. Never a
    /// silent guess when a real figure exists, and never silence when none does.
    pub fn resolve_with_margin(
        model: &str,
        configured: Option<u64>,
        advertised: Option<u64>,
        safety_margin_percent: u8,
    ) -> Self {
        let registry = Self::lookup(model);
        let (context_window, source) = if let Some(window) = configured {
            (window, LimitsSource::Config)
        } else if let Some(window) = advertised {
            (window, LimitsSource::Provider)
        } else if let Some(limits) = registry {
            return limits;
        } else {
            (ASSUMED_CONTEXT_WINDOW, LimitsSource::Assumed)
        };
        Self {
            context_window,
            max_output_tokens: registry
                .map(|limits| limits.max_output_tokens)
                .unwrap_or(ASSUMED_MAX_OUTPUT),
            safety_margin: Self::safety_margin_for_percent(context_window, safety_margin_percent),
            source,
        }
    }

    /// Prefix match against the registry, longest key winning.
    pub fn lookup(model: &str) -> Option<Self> {
        let name = model.trim().to_lowercase();
        if name.is_empty() {
            return None;
        }
        // A dated or routed name usually carries a suffix the registry key does not, so a
        // substring hit is accepted when no prefix does. Longest key wins either way.
        let row = REGISTRY
            .iter()
            .filter(|row| name.starts_with(row.0) || name.contains(row.0))
            .max_by_key(|row| row.0.len());
        row.map(|(_, window, output)| Self {
            context_window: *window,
            max_output_tokens: *output,
            safety_margin: Self::safety_margin_for(*window),
            source: LimitsSource::Registry,
        })
    }

    /// Eight percent of the window by default, floored and capped: large enough to absorb estimate
    /// error, small enough not to waste capacity on a million-token model.
    fn safety_margin_for(window: u64) -> u64 {
        Self::safety_margin_for_percent(window, 8)
    }

    fn safety_margin_for_percent(window: u64, percent: u8) -> u64 {
        let percent = percent.clamp(1, 50) as u64;
        window.saturating_mul(percent) / 100
    }

    pub fn is_assumed(&self) -> bool {
        self.source == LimitsSource::Assumed
    }
}

impl Default for ModelLimits {
    fn default() -> Self {
        Self {
            context_window: ASSUMED_CONTEXT_WINDOW,
            max_output_tokens: ASSUMED_MAX_OUTPUT,
            safety_margin: Self::safety_margin_for(ASSUMED_CONTEXT_WINDOW),
            source: LimitsSource::Assumed,
        }
    }
}

/// A measured baseline: what the provider said the request cost, and how many messages that
/// covered. Everything appended after it is estimated on top of the figure, never instead of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextAnchor {
    pub input_total: u64,
    pub messages: usize,
}

/// What one request is allowed to contain, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextBudget {
    pub limits: ModelLimits,
    /// System prompt, tool schemas, agent identity: the fixed overhead of a request.
    pub overhead: u64,
    /// Space reserved for the reply, so a full context cannot starve the answer.
    pub output_reserve: u64,
    /// Set only from provider-reported usage, and dropped whenever the transcript is folded.
    pub anchor: Option<ContextAnchor>,
}

impl ContextBudget {
    pub fn new(limits: ModelLimits, overhead: u64, configured_output: Option<u64>) -> Self {
        let output_reserve = configured_output
            .unwrap_or(limits.max_output_tokens)
            .min(limits.max_output_tokens.saturating_mul(2));
        Self {
            limits,
            overhead,
            output_reserve,
            anchor: None,
        }
    }

    /// Attach a measured baseline for the transcript length it covers.
    pub fn with_anchor(mut self, anchor: ContextAnchor) -> Self {
        self.anchor = Some(anchor);
        self
    }

    /// Tokens of conversation the model can actually be given.
    pub fn usable_input(&self) -> u64 {
        self.limits
            .context_window
            .saturating_sub(self.output_reserve)
            .saturating_sub(self.overhead)
            .saturating_sub(self.limits.safety_margin)
    }

    pub fn fit(&self, tokens: u64) -> bool {
        tokens <= self.usable_input()
    }

    pub fn remaining(&self, used: u64) -> u64 {
        self.usable_input().saturating_sub(used)
    }
}

/// Structural token estimate, plus a ratio learned from the provider's own usage figures.
///
/// A real BPE tokenizer would need a vendored vocabulary; instead the estimate is structural
/// (word runs, punctuation runs, digit groups, one token per CJK glyph, per-message framing) and
/// then *calibrated*: every completed request compares estimate against reported usage and moves
/// the ratio toward the truth. The first requests of a session are honest estimates; later ones
/// approach measured counts, and the provenance always says which it is.
#[derive(Clone, Debug)]
pub struct TokenEstimator {
    ratio: f64,
    observations: u64,
}

impl Default for TokenEstimator {
    fn default() -> Self {
        Self {
            ratio: 1.0,
            observations: 0,
        }
    }
}

/// Framing cost per message in a chat request. Not measured by `text_body`, and not free.
const MESSAGE_FRAMING: u64 = 4;
/// A tool call costs its name plus JSON scaffolding on top of its arguments.
const TOOL_CALL_FRAMING: u64 = 8;

impl TokenEstimator {
    pub fn estimate_text(&self, text: &str) -> u64 {
        (self.estimate_raw(text) as f64 * self.ratio).ceil() as u64
    }

    /// Estimate a whole message, counting every block. [`Message::text_body`] drops tool-call
    /// arguments and reasoning, which silently undercounts the two largest payloads an agent
    /// produces; this walks the blocks instead.
    pub fn estimate_message(&self, message: &Message) -> u64 {
        let mut total = MESSAGE_FRAMING;
        for block in &message.blocks {
            total += match block {
                Block::Text { text, .. } | Block::Reasoning { text, .. } => self.estimate_raw(text),
                Block::ToolCall { call } => {
                    TOOL_CALL_FRAMING
                        + self.estimate_raw(&call.name)
                        + self.estimate_raw(&call.arguments.to_string())
                }
                Block::ToolResult { content, .. } => self.estimate_raw(content),
            };
        }
        (total as f64 * self.ratio).ceil() as u64
    }

    pub fn estimate_messages(&self, messages: &[Message]) -> u64 {
        messages.iter().map(|m| self.estimate_message(m)).sum()
    }

    /// Fold in one provider-reported figure. `estimated` is what we predicted for the same prompt.
    /// The ratio is clamped well away from zero and from absurd, and converges slowly so one
    /// outlier response cannot make the meter lie in the other direction.
    pub fn observe(&mut self, estimated: u64, reported: u64) {
        if estimated == 0 || reported == 0 {
            return;
        }
        let observed = reported as f64 / estimated as f64;
        let observed = observed.clamp(0.4, 2.5);
        let weight = (1.0_f64 / (self.observations as f64 + 1.0)).min(0.5);
        self.ratio = self.ratio * (1.0 - weight) + observed * weight;
        self.ratio = self.ratio.clamp(0.4, 2.5);
        self.observations += 1;
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    pub fn observations(&self) -> u64 {
        self.observations
    }

    /// Un-calibrated structural count, from the shared counter in the kernel
    /// ([`vikram_contracts::TokenMeter::estimate`]). There is deliberately no second one here: one
    /// algorithm, calibrated by a ratio, prices every message, file and tool schema in the runtime.
    fn estimate_raw(&self, text: &str) -> u64 {
        vikram_contracts::TokenMeter::estimate(text)
    }
}

/// Whether an estimate says we are over the limit. Reported usage, when present, outranks the
/// estimate for the figure the meter shows.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextStatus {
    pub used: u64,
    pub window: u64,
    pub usable: u64,
    pub remaining: u64,
    pub ratio: f32,
    pub limits_source: LimitsSource,
    /// True when `used` came from the provider rather than from the estimator.
    pub measured: bool,
}

impl ContextStatus {
    pub fn new(budget: &ContextBudget, used: u64, measured: bool) -> Self {
        let usable = budget.usable_input();
        Self {
            used,
            window: budget.limits.context_window,
            usable,
            remaining: usable.saturating_sub(used),
            ratio: if usable == 0 {
                1.0
            } else {
                (used as f32 / usable as f32).clamp(0.0, 1.0)
            },
            limits_source: budget.limits.source,
            measured,
        }
    }
}

/// System prompt plus tool schemas plus agent identity, measured the same way as the transcript so
/// the two numbers can be added without a unit mismatch.
pub fn estimate_overhead(estimator: &TokenEstimator, messages: &[Message]) -> u64 {
    estimator.estimate_messages(messages)
}

pub fn system_messages_only(messages: &[Message]) -> Vec<&Message> {
    messages.iter().filter(|m| m.role == Role::System).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::ToolCall;

    #[test]
    fn registry_prefers_the_longest_matching_key() {
        let mini = ModelLimits::lookup("gpt-4o-mini-2024-07-18").unwrap();
        assert_eq!(mini.source, LimitsSource::Registry);
        assert_eq!(mini.context_window, 128_000);
        let four = ModelLimits::lookup("gpt-4-0613").unwrap();
        assert_eq!(four.context_window, 8_192);
    }

    #[test]
    fn different_models_get_different_windows() {
        let claude = ModelLimits::resolve("claude-sonnet-4-5", None, None);
        let gpt4 = ModelLimits::resolve("gpt-4", None, None);
        let gemini = ModelLimits::resolve("gemini-2.5-pro", None, None);
        assert_eq!(claude.context_window, 200_000);
        assert_eq!(gpt4.context_window, 8_192);
        assert_eq!(gemini.context_window, 1_048_576);
        assert!(gpt4.safety_margin < gemini.safety_margin);
    }

    #[test]
    fn resolution_order_is_config_then_provider_then_registry_then_assumed() {
        let configured = ModelLimits::resolve("gpt-4o", Some(9_000), Some(7_000));
        assert_eq!(
            (configured.context_window, configured.source),
            (9_000, LimitsSource::Config)
        );
        let advertised = ModelLimits::resolve("gpt-4o", None, Some(7_000));
        assert_eq!(advertised.source, LimitsSource::Provider);
        assert_eq!(advertised.context_window, 7_000);
        let declared = ModelLimits::resolve("gpt-4o", None, None);
        assert_eq!(declared.source, LimitsSource::Registry);
        let unknown = ModelLimits::resolve("some-private-weights", None, None);
        assert_eq!(unknown.source, LimitsSource::Assumed);
        assert!(unknown.is_assumed());
    }

    #[test]
    fn budget_subtracts_every_reserve() {
        let limits = ModelLimits {
            context_window: 1000,
            max_output_tokens: 100,
            safety_margin: 50,
            source: LimitsSource::Assumed,
        };
        let budget = ContextBudget::new(limits, 200, None);
        assert_eq!(budget.usable_input(), 650);
        assert!(budget.fit(650));
        assert!(!budget.fit(651));
        assert_eq!(budget.remaining(600), 50);
    }

    #[test]
    fn output_reserve_never_exceeds_a_multiple_of_the_model_cap() {
        let limits = ModelLimits::resolve("gpt-4o", None, None);
        let budget = ContextBudget::new(limits, 0, Some(999_999));
        assert_eq!(budget.output_reserve, limits.max_output_tokens * 2);
    }

    #[test]
    fn estimator_ignores_the_flat_length_rule() {
        let e = TokenEstimator::default();
        // "the the the" is 3 tokens in every real vocabulary, not 11/4.
        assert_eq!(e.estimate_text("the the the"), 3);
        assert!(e.estimate_text("internationalization") > 3);
        assert_eq!(e.estimate_text(""), 0);
    }

    #[test]
    fn cjk_costs_more_than_a_quarter_of_its_length() {
        let e = TokenEstimator::default();
        assert_eq!(e.estimate_text("中文测试"), 4);
    }

    #[test]
    fn estimate_covers_tool_arguments_and_reasoning() {
        let e = TokenEstimator::default();
        let call = ToolCall::new(
            "bash.run",
            serde_json::json!({"command": "grep -rn 'context_window' crates/ --include=*.rs"}),
        );
        let message = Message {
            role: Role::Assistant,
            blocks: vec![
                Block::Reasoning {
                    text: "the search term appears in config".into(),
                    trust: vikram_contracts::Trust::Model,
                },
                Block::ToolCall { call },
            ],
        };
        // The old path scored this near zero because text_body() drops both block kinds.
        assert!(message.text_body().trim().is_empty());
        assert!(e.estimate_message(&message) > 25);
    }

    #[test]
    fn calibration_moves_the_estimate_toward_reported_usage() {
        let mut e = TokenEstimator::default();
        let text = "we need to compact the older turns and keep the recent ones";
        let estimated = e.estimate_text(text);
        // A provider that bills ~1.6x our structural count should raise the ratio.
        for _ in 0..8 {
            e.observe(estimated, (estimated as f64 * 1.6) as u64);
        }
        assert!(e.ratio() > 1.3);
        assert!(e.ratio() <= 2.5);
        assert_eq!(e.observations(), 8);
        e.observe(0, 100);
        assert_eq!(e.observations(), 8);
    }

    #[test]
    fn calibration_is_stable_against_one_absurd_response() {
        let mut e = TokenEstimator::default();
        e.observe(10, 10);
        e.observe(10, 1_000_000);
        assert!(
            e.ratio() < 2.0,
            "one outlier must not flip the meter: {}",
            e.ratio()
        );
    }

    #[test]
    fn status_reports_remaining_and_provenance() {
        let limits = ModelLimits {
            context_window: 2000,
            max_output_tokens: 200,
            safety_margin: 100,
            source: LimitsSource::Registry,
        };
        let budget = ContextBudget::new(limits, 700, None);
        let status = ContextStatus::new(&budget, 500, false);
        assert_eq!(status.usable, 1000);
        assert_eq!(status.remaining, 500);
        assert!((status.ratio - 0.5).abs() < 0.001);
        assert!(!status.measured);
        assert_eq!(status.limits_source, LimitsSource::Registry);
    }

    #[test]
    fn zero_usable_window_reads_as_full_not_div_by_zero() {
        let limits = ModelLimits {
            context_window: 100,
            max_output_tokens: 4_096,
            safety_margin: 512,
            source: LimitsSource::Assumed,
        };
        let budget = ContextBudget::new(limits, 0, None);
        assert_eq!(budget.usable_input(), 0);
        let status = ContextStatus::new(&budget, 0, false);
        assert_eq!(status.ratio, 1.0);
    }
}
