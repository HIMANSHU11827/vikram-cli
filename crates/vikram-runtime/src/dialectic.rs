//! Vikram-native dialectic user modelling.
//!
//! A deterministic, heuristic, fully local layer that derives structured insights about the
//! user — preferences, habits and goals — from conversation turns, stores them as structured
//! entries, and injects them as a second context layer (the *dialectic supplement*) alongside
//! the base context (retrieved memory / project instructions) in `turn.rs`.
//!
//! Design notes:
//! - No external API, no model call, no randomness: the same transcript always yields the
//!   same insights, in the same order.
//! - Extraction is pattern-based over user turns only. Anything not matching a known
//!   self-description pattern is ignored, so idle chat does not pollute the model.
//! - Insights are merged by `(kind, key)`: repeated evidence raises confidence
//!   deterministically instead of duplicating entries.
//! - Storage reuses [`crate::memory::MemoryStore`] under the host-controlled scope
//!   `"dialectic"` (deny-first, like `retrieved_memory`), one structured JSON entry per
//!   `(kind, key)`.
//! - Injection is budgeted: [`render_supplement`] stops adding lines once the token estimate
//!   would exceed the configured budget, so the supplement can never blow the window.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use vikram_contracts::TokenMeter;

use crate::memory::MemoryStore;

/// Host-controlled memory scope for dialectic entries. Never caller/model-chosen.
pub const DIALECTIC_SCOPE: &str = "dialectic";

/// Config knobs for the dialectic layer: cadence, depth and budget.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DialecticConfig {
    /// Master switch. `false` disables extraction and injection entirely.
    pub enabled: bool,
    /// Run extraction every N user turns. `0` means never run extraction
    /// (history is still tracked, so raising the cadence later resumes cleanly).
    pub cadence_turns: u64,
    /// Rolling history depth: how many recent user turns are kept for
    /// cross-turn heuristics (repeated-topic habits, communication style).
    pub depth: usize,
    /// Token budget for the rendered supplement injected per turn.
    pub budget_tokens: u64,
    /// Hard cap on insights retained in memory / storage.
    pub max_insights: usize,
}

impl Default for DialecticConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cadence_turns: 3,
            depth: 20,
            budget_tokens: 600,
            max_insights: 32,
        }
    }
}

/// What kind of user fact an [`Insight`] asserts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsightKind {
    Preference,
    Habit,
    Goal,
}

impl InsightKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preference => "preference",
            Self::Habit => "habit",
            Self::Goal => "goal",
        }
    }
}

/// One structured, derived fact about the user.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Insight {
    pub kind: InsightKind,
    /// Stable slug derived from the captured phrase, e.g. `focus-rust`.
    pub key: String,
    /// Human-readable statement, e.g. `prefers Rust`.
    pub value: String,
    /// How many turns evidenced this insight.
    pub evidence: u32,
    /// Deterministic confidence in `[0.0, 1.0]`: `min(1.0, 0.4 + 0.2 * evidence)`.
    pub confidence: f64,
    /// The user turn text that first evidenced it (clipped).
    pub source: String,
}

impl Insight {
    fn new(kind: InsightKind, key: String, value: String, source: String) -> Self {
        Self {
            kind,
            key,
            value,
            evidence: 1,
            confidence: confidence_for(1),
            source,
        }
    }

    /// Record one more piece of evidence for an existing insight.
    fn bump(&mut self) {
        self.evidence = self.evidence.saturating_add(1);
        self.confidence = confidence_for(self.evidence);
    }
}

fn confidence_for(evidence: u32) -> f64 {
    (0.4 + 0.2 * evidence as f64).min(1.0)
}

fn slugify(phrase: &str) -> String {
    let mut slug = String::new();
    for word in phrase.split_whitespace().take(4) {
        let cleaned: String = word
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>()
            .to_lowercase();
        if cleaned.is_empty() {
            continue;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&cleaned);
        if slug.len() >= 40 {
            break;
        }
    }
    if slug.is_empty() {
        "misc".to_owned()
    } else {
        slug
    }
}

/// The part of `text` after `marker`, clipped to the first clause (up to `.` or `,`)
/// and to a bounded length. Returns `None` when the marker is absent or nothing
/// meaningful follows it.
fn capture_after(text: &str, marker: &str) -> Option<String> {
    let idx = text.find(marker)?;
    let rest = text[idx + marker.len()..].trim_start();
    // Skip a leading "to " so "i want to ship" captures "ship".
    let rest = rest.strip_prefix("to ").unwrap_or(rest);
    let clause_end = rest.find(['.', '\n']).unwrap_or(rest.len());
    let captured = rest[..clause_end]
        .trim()
        .trim_matches(|c| c == '"' || c == '\'');
    if captured.len() < 2 {
        return None;
    }
    let clipped: String = captured.chars().take(80).collect();
    Some(clipped.trim().to_owned())
}

/// Extract insights from a single user turn. Pure and deterministic.
pub fn extract_insights(user_text: &str) -> Vec<Insight> {
    let text = user_text.to_lowercase();
    let source: String = user_text.chars().take(120).collect();
    let mut out = Vec::new();

    // Preferences: explicit likes / dislikes / preferences.
    for marker in [
        "i prefer ",
        "i like ",
        "i love ",
        "i enjoy ",
        "i prefer to ",
    ] {
        if let Some(captured) = capture_after(&text, marker) {
            out.push(Insight::new(
                InsightKind::Preference,
                slugify(&captured),
                format!("prefers {captured}"),
                source.clone(),
            ));
            break;
        }
    }
    for marker in [
        "i don't like ",
        "i do not like ",
        "i dislike ",
        "i hate ",
        "i never want ",
    ] {
        if let Some(captured) = capture_after(&text, marker) {
            out.push(Insight::new(
                InsightKind::Preference,
                slugify(&captured),
                format!("avoids {captured}"),
                source.clone(),
            ));
            break;
        }
    }
    // Standing instructions that read as preferences.
    for marker in ["always use ", "please always use ", "please use "] {
        if let Some(captured) = capture_after(&text, marker) {
            out.push(Insight::new(
                InsightKind::Preference,
                slugify(&captured),
                format!("wants {captured} used consistently"),
                source.clone(),
            ));
            break;
        }
    }
    for marker in [
        "never use ",
        "please never ",
        "please don't use ",
        "please do not use ",
    ] {
        if let Some(captured) = capture_after(&text, marker) {
            out.push(Insight::new(
                InsightKind::Preference,
                slugify(&captured),
                format!("does not want {captured} used"),
                source.clone(),
            ));
            break;
        }
    }

    // Habits: self-described routines.
    for marker in [
        "i usually ",
        "i always ",
        "i tend to ",
        "i generally ",
        "every day i ",
        "i work by ",
    ] {
        if let Some(captured) = capture_after(&text, marker) {
            out.push(Insight::new(
                InsightKind::Habit,
                slugify(&captured),
                format!("habitually {captured}"),
                source.clone(),
            ));
            break;
        }
    }

    // Goals: stated aims.
    for marker in [
        "my goal is ",
        "i want to ",
        "i want ",
        "i need to ",
        "i plan to ",
        "i'm working on ",
        "i am working on ",
        "i aim to ",
    ] {
        if let Some(captured) = capture_after(&text, marker) {
            out.push(Insight::new(
                InsightKind::Goal,
                slugify(&captured),
                format!("goal: {captured}"),
                source.clone(),
            ));
            break;
        }
    }

    // Deterministic order regardless of pattern check order.
    out.sort_by(|a, b| (a.kind, &a.key).cmp(&(b.kind, &b.key)));
    out.dedup_by(|a, b| a.kind == b.kind && a.key == b.key);
    out
}

const STOP_WORDS: &[&str] = &[
    "the", "and", "with", "that", "this", "from", "have", "has", "are", "was", "were", "will",
    "would", "could", "should", "about", "into", "your", "you", "for", "not", "but", "can", "did",
    "does", "just", "please", "make", "them", "they", "their", "there", "then", "than", "here",
    "what", "when", "where", "which", "while", "want", "need", "using", "used", "also", "into",
];

/// Cross-turn heuristics over the rolling history window: repeated-topic habits and
/// communication style. Pure and deterministic.
pub fn extract_cross_turn_insights(history: &[String]) -> Vec<Insight> {
    let mut out = Vec::new();
    if history.is_empty() {
        return out;
    }

    // Repeated topics: a content word appearing in at least 3 distinct turns
    // (or 2 when the window is short) is a topic habit.
    let threshold = if history.len() >= 4 { 3 } else { 2 };
    let mut turn_counts: BTreeMap<String, usize> = BTreeMap::new();
    for turn in history {
        let mut seen: Vec<String> = Vec::new();
        for word in turn.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
            if word.len() >= 5 && !STOP_WORDS.contains(&word) && !seen.iter().any(|w| w == word) {
                seen.push(word.to_owned());
            }
        }
        for word in seen {
            *turn_counts.entry(word).or_insert(0) += 1;
        }
    }
    for (word, count) in turn_counts {
        if count >= threshold {
            out.push(Insight {
                kind: InsightKind::Habit,
                key: format!("focus-{word}"),
                value: format!("frequently discusses {word}"),
                evidence: count as u32,
                confidence: confidence_for(count as u32),
                source: history
                    .iter()
                    .find(|t| t.to_lowercase().contains(&word))
                    .cloned()
                    .unwrap_or_default(),
            });
        }
    }

    // Communication style from average user-turn length, once there is enough signal.
    if history.len() >= 3 {
        let avg: usize = history.iter().map(|t| t.chars().count()).sum::<usize>() / history.len();
        if avg < 50 {
            out.push(Insight {
                kind: InsightKind::Preference,
                key: "communication-style".to_owned(),
                value: "prefers concise answers".to_owned(),
                evidence: history.len() as u32,
                confidence: confidence_for(history.len() as u32),
                source: history[0].clone(),
            });
        } else if avg > 200 {
            out.push(Insight {
                kind: InsightKind::Preference,
                key: "communication-style".to_owned(),
                value: "prefers detailed answers".to_owned(),
                evidence: history.len() as u32,
                confidence: confidence_for(history.len() as u32),
                source: history[0].clone(),
            });
        }
    }

    out.sort_by(|a, b| (a.kind, &a.key).cmp(&(b.kind, &b.key)));
    out
}

/// Sort insights deterministically: highest confidence first, then kind, then key.
pub fn sort_insights(insights: &mut [Insight]) {
    insights.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (a.kind, &a.key).cmp(&(b.kind, &b.key)))
    });
}

/// Render the dialectic supplement (layer 2) within a token budget.
/// Returns `None` when there is nothing to say or the budget admits no line.
pub fn render_supplement(insights: &[Insight], budget_tokens: u64) -> Option<String> {
    if insights.is_empty() || budget_tokens == 0 {
        return None;
    }
    let mut sorted = insights.to_vec();
    sort_insights(&mut sorted);

    let header = "User model (derived heuristically from earlier turns; treat as a hint, verify when it matters):";
    let mut block = format!("{header}\n");
    let mut spent = TokenMeter::estimate(&block);
    let mut used = 0usize;
    for insight in &sorted {
        let line = format!(
            "- [{}] {} (confidence {:.1}, {} evidence)\n",
            insight.kind.as_str(),
            insight.value,
            insight.confidence,
            insight.evidence
        );
        let cost = TokenMeter::estimate(&line);
        if spent + cost > budget_tokens {
            break;
        }
        spent += cost;
        block.push_str(&line);
        used += 1;
    }
    if used == 0 {
        None
    } else {
        Some(block.trim_end().to_owned())
    }
}

/// The dialectic engine: rolling history + merged insights, thread-safe and deterministic.
#[derive(Debug)]
pub struct DialecticEngine {
    config: DialecticConfig,
    state: Mutex<DialecticState>,
}

#[derive(Debug, Default)]
struct DialecticState {
    turn_count: u64,
    history: VecDeque<String>,
    insights: BTreeMap<(InsightKind, String), Insight>,
}

impl DialecticEngine {
    pub fn new(config: DialecticConfig) -> Self {
        Self {
            config,
            state: Mutex::new(DialecticState::default()),
        }
    }

    pub fn config(&self) -> &DialecticConfig {
        &self.config
    }

    /// Whether extraction runs on this turn number (1-based user turns).
    pub fn should_run(&self, turn_number: u64) -> bool {
        self.config.enabled
            && self.config.cadence_turns > 0
            && turn_number > 0
            && turn_number.is_multiple_of(self.config.cadence_turns)
    }

    /// Observe one user turn. Records history always; extracts and merges only on cadence.
    /// Returns the insights extracted this call (empty on non-cadence turns).
    pub fn observe_turn(&self, user_text: &str) -> Vec<Insight> {
        if !self.config.enabled {
            return Vec::new();
        }
        let mut state = self.state.lock().expect("dialectic state lock");
        state.turn_count += 1;
        let turn_number = state.turn_count;
        state.history.push_back(user_text.to_owned());
        while state.history.len() > self.config.depth.max(1) {
            state.history.pop_front();
        }
        if !self.should_run(turn_number) {
            return Vec::new();
        }
        let history: Vec<String> = state.history.iter().cloned().collect();
        let mut fresh = extract_insights(user_text);
        fresh.extend(extract_cross_turn_insights(&history));
        self.merge_locked(&mut state, fresh)
    }

    /// Observe a turn and extract regardless of cadence (useful for tests / session end).
    pub fn observe_turn_forced(&self, user_text: &str) -> Vec<Insight> {
        if !self.config.enabled {
            return Vec::new();
        }
        let mut state = self.state.lock().expect("dialectic state lock");
        state.turn_count += 1;
        state.history.push_back(user_text.to_owned());
        while state.history.len() > self.config.depth.max(1) {
            state.history.pop_front();
        }
        let history: Vec<String> = state.history.iter().cloned().collect();
        let mut fresh = extract_insights(user_text);
        fresh.extend(extract_cross_turn_insights(&history));
        self.merge_locked(&mut state, fresh)
    }

    fn merge_locked(&self, state: &mut DialecticState, fresh: Vec<Insight>) -> Vec<Insight> {
        let mut extracted = Vec::new();
        for insight in fresh {
            extracted.push(insight.clone());
            match state.insights.get_mut(&(insight.kind, insight.key.clone())) {
                Some(existing) => existing.bump(),
                None => {
                    state
                        .insights
                        .insert((insight.kind, insight.key.clone()), insight);
                }
            }
        }
        // Cap total retained insights deterministically: drop lowest confidence,
        // tie-broken by (kind, key) descending so the cut is stable.
        let cap = self.config.max_insights.max(1);
        if state.insights.len() > cap {
            let mut all: Vec<(InsightKind, String)> = state.insights.keys().cloned().collect();
            all.sort_by(|a, b| {
                let ca = state.insights[&a.clone()].confidence;
                let cb = state.insights[&b.clone()].confidence;
                ca.partial_cmp(&cb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.cmp(a))
            });
            for key in all.iter().take(state.insights.len() - cap) {
                state.insights.remove(key);
            }
        }
        extracted
    }

    /// All currently held insights, deterministically sorted (confidence desc).
    pub fn insights(&self) -> Vec<Insight> {
        let state = self.state.lock().expect("dialectic state lock");
        let mut all: Vec<Insight> = state.insights.values().cloned().collect();
        sort_insights(&mut all);
        all
    }

    pub fn turn_count(&self) -> u64 {
        self.state.lock().expect("dialectic state lock").turn_count
    }

    /// The supplement for the next turn's prefix (layer 2), or `None`.
    pub fn supplement(&self) -> Option<String> {
        if !self.config.enabled {
            return None;
        }
        render_supplement(&self.insights(), self.config.budget_tokens)
    }

    /// Persist every insight as a structured JSON entry under [`DIALECTIC_SCOPE`].
    /// Returns how many entries were written. Errors are reported, never panicked on.
    pub fn persist_to(&self, store: &MemoryStore) -> usize {
        let mut written = 0usize;
        for insight in self.insights() {
            let key = format!("{}:{}", insight.kind.as_str(), insight.key);
            if let Ok(value) = serde_json::to_string(&insight)
                && store.save(&key, &value, DIALECTIC_SCOPE).is_ok()
            {
                written += 1;
            }
        }
        written
    }

    /// Load previously persisted insights (e.g. at boot). Returns how many were loaded.
    pub fn load_from(&self, store: &MemoryStore) -> usize {
        let Ok(entries) = store.list(DIALECTIC_SCOPE) else {
            return 0;
        };
        let mut state = self.state.lock().expect("dialectic state lock");
        let mut loaded = 0usize;
        for (_key, value) in entries {
            let Ok(insight) = serde_json::from_str::<Insight>(&value) else {
                continue;
            };
            state
                .insights
                .insert((insight.kind, insight.key.clone()), insight);
            loaded += 1;
        }
        loaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> DialecticEngine {
        DialecticEngine::new(DialecticConfig {
            cadence_turns: 1,
            ..DialecticConfig::default()
        })
    }

    #[test]
    fn extracts_preference_from_i_prefer() {
        let out = extract_insights("I prefer tabs over spaces");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, InsightKind::Preference);
        assert!(out[0].value.contains("tabs over spaces"));
        assert!((out[0].confidence - 0.6).abs() < 1e-9);
    }

    #[test]
    fn extracts_dispreference() {
        let out = extract_insights("I don't like verbose logs");
        assert!(out.iter().any(|i| i.value.starts_with("avoids")));
    }

    #[test]
    fn extracts_habit_and_goal() {
        assert!(
            extract_insights("I usually run tests before committing")
                .iter()
                .any(|i| i.kind == InsightKind::Habit)
        );
        assert!(
            extract_insights("My goal is to ship on Friday")
                .iter()
                .any(|i| i.kind == InsightKind::Goal)
        );
    }

    #[test]
    fn idle_chat_yields_no_insights() {
        assert!(extract_insights("hello, how are you?").is_empty());
        assert!(extract_insights("").is_empty());
    }

    #[test]
    fn extraction_is_deterministic() {
        let a = extract_insights("I prefer Rust. I want to learn more");
        let b = extract_insights("I prefer Rust. I want to learn more");
        assert_eq!(a, b);
    }

    #[test]
    fn cadence_gates_extraction() {
        let engine = DialecticEngine::new(DialecticConfig {
            cadence_turns: 3,
            ..DialecticConfig::default()
        });
        assert!(engine.observe_turn("I prefer tabs").is_empty());
        assert!(engine.observe_turn("I prefer tabs").is_empty());
        let third = engine.observe_turn("I prefer tabs");
        assert!(!third.is_empty(), "third turn is the cadence boundary");
        assert_eq!(engine.turn_count(), 3);
    }

    #[test]
    fn disabled_engine_never_extracts() {
        let engine = DialecticEngine::new(DialecticConfig {
            enabled: false,
            cadence_turns: 1,
            ..DialecticConfig::default()
        });
        assert!(engine.observe_turn("I prefer tabs").is_empty());
        assert!(engine.supplement().is_none());
    }

    #[test]
    fn repeated_evidence_merges_and_raises_confidence() {
        let engine = engine();
        engine.observe_turn_forced("I prefer tabs");
        engine.observe_turn_forced("I prefer tabs");
        let insights = engine.insights();
        let pref: Vec<_> = insights
            .iter()
            .filter(|i| i.kind == InsightKind::Preference)
            .collect();
        assert_eq!(pref.len(), 1, "same key merges into one entry: {pref:?}");
        assert_eq!(pref[0].evidence, 2);
        assert!((pref[0].confidence - 0.8).abs() < 1e-9);
    }

    #[test]
    fn supplement_respects_budget() {
        let engine = engine();
        engine
            .observe_turn_forced("I prefer tabs and I want to ship fast and I usually test first");
        let full = engine.supplement().expect("supplement exists");
        assert!(full.contains("User model"));
        // A tiny budget admits no line at all.
        assert!(render_supplement(&engine.insights(), 1).is_none());
        let capped = render_supplement(&engine.insights(), 60).expect("fits something");
        assert!(capped.len() <= full.len());
    }

    #[test]
    fn supplement_is_none_without_insights() {
        assert!(engine().supplement().is_none());
    }

    #[test]
    fn repeated_topic_becomes_a_habit() {
        let engine = DialecticEngine::new(DialecticConfig {
            cadence_turns: 1,
            depth: 20,
            ..DialecticConfig::default()
        });
        for _ in 0..3 {
            engine.observe_turn_forced("let us review the gateway deployment pipeline again");
        }
        assert!(
            engine
                .insights()
                .iter()
                .any(|i| i.key.starts_with("focus-")),
            "a word in 3 turns is a topic habit: {:?}",
            engine.insights()
        );
    }

    #[test]
    fn concise_style_is_detected() {
        let engine = engine();
        for _ in 0..3 {
            engine.observe_turn_forced("ok");
        }
        assert!(
            engine
                .insights()
                .iter()
                .any(|i| i.key == "communication-style" && i.value.contains("concise"))
        );
    }

    #[test]
    fn insights_persist_and_reload_as_structured_entries() {
        let store = MemoryStore::open_in_memory().unwrap();
        let engine = engine();
        engine.observe_turn_forced("I prefer tabs");
        assert_eq!(engine.persist_to(&store), 1);
        let reloaded = DialecticEngine::new(DialecticConfig::default());
        assert_eq!(reloaded.load_from(&store), 1);
        let insights = reloaded.insights();
        assert_eq!(insights.len(), 1);
        assert_eq!(insights[0].kind, InsightKind::Preference);
        // Stored value is structured JSON, not a bare string.
        let raw = store
            .get("preference:tabs", DIALECTIC_SCOPE)
            .unwrap()
            .unwrap();
        assert!(serde_json::from_str::<Insight>(&raw).is_ok());
    }

    #[test]
    fn max_insights_cap_is_enforced_deterministically() {
        let engine = DialecticEngine::new(DialecticConfig {
            cadence_turns: 1,
            max_insights: 2,
            ..DialecticConfig::default()
        });
        engine.observe_turn_forced("I prefer alpha1");
        engine.observe_turn_forced("I prefer beta22");
        engine.observe_turn_forced("I prefer gamma33");
        assert!(engine.insights().len() <= 2);
    }
}
