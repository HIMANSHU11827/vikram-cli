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
//! - Insights decay: an insight not re-evidenced for `decay_after_turns` user
//!   turns loses effective confidence in fixed windows, and falls below the
//!   forget threshold (omitted from the supplement, evicted on persist).
//! - Repeated corrections on the same topic (detected with the same markers as
//!   `learning::detect_corrections`) are aggregated here — single corrections
//!   already flow through the learning proposal pipeline — and at
//!   `correction_promote_at` the pattern is written to USER.md as a durable
//!   preference note. The corresponding insight is then filtered out of the
//!   supplement so the fact is not injected twice.
//! - A request repeated verbatim (normalized) at least `ROUTINE_THRESHOLD`
//!   times in the window surfaces as a `suggestion` insight — a candidate
//!   saved procedure. Only user text is visible here; tool sequences are not.
//! - Storage reuses [`crate::memory::MemoryStore`] under the host-controlled scope
//!   `"dialectic"` (deny-first, like `retrieved_memory`), one structured JSON entry per
//!   `(kind, key)`, plus `correction:*` stats and a `meta:turn-count` entry.
//! - Injection is budgeted: [`render_supplement`] stops adding lines once the token estimate
//!   would exceed the configured budget, so the supplement can never blow the window.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
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
    /// User turns an insight may go unobserved before its effective confidence
    /// starts decaying (see [`effective_confidence`]). `0` disables decay.
    pub decay_after_turns: u64,
    /// Corrections on the same topic required before the aggregate is promoted
    /// to a durable USER.md preference entry. `0` is treated as 1.
    pub correction_promote_at: u32,
    /// Override for the USER.md path correction promotions write to. `None`
    /// uses `memories/USER.md` under the Vikram home (same file `memory.note`
    /// writes). Exists so tests can point at a tempdir.
    pub user_file_path: Option<PathBuf>,
}

impl Default for DialecticConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cadence_turns: 3,
            depth: 20,
            budget_tokens: 600,
            max_insights: 32,
            decay_after_turns: 12,
            correction_promote_at: 3,
            user_file_path: None,
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
    /// A recurring request pattern worth surfacing as a candidate procedure.
    Suggestion,
}

impl InsightKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preference => "preference",
            Self::Habit => "habit",
            Self::Goal => "goal",
            Self::Suggestion => "suggestion",
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
    /// Turn counter value when this insight was last evidenced. `0` for
    /// insights built outside the engine (and legacy persisted entries).
    #[serde(default)]
    pub last_seen_turn: u64,
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
            last_seen_turn: 0,
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

/// Staleness decay: each full `window` of user turns without re-observation
/// multiplies confidence by [`DECAY_FACTOR`], floored at [`DECAY_FLOOR`].
/// Honest heuristic, not forgetting research: "unseen" means the extraction
/// patterns did not fire again inside the rolling history window.
const DECAY_FACTOR: f64 = 0.7;
const DECAY_FLOOR: f64 = 0.05;
/// Effective confidence below which an insight is treated as forgotten: hidden
/// from the supplement and evicted from storage on the next persist.
const FORGOTTEN_CONFIDENCE: f64 = 0.15;

fn effective_confidence(insight: &Insight, turn_count: u64, window: u64) -> f64 {
    if window == 0 {
        return insight.confidence;
    }
    let age = turn_count.saturating_sub(insight.last_seen_turn);
    let periods = (age / window).min(64) as i32;
    if periods == 0 {
        return insight.confidence;
    }
    (insight.confidence * DECAY_FACTOR.powi(periods)).max(DECAY_FLOOR)
}

/// Aggregate count of corrections on one topic. Persisted under
/// `correction:<topic>` in [`DIALECTIC_SCOPE`] so promotion survives restarts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CorrectionStat {
    pub topic: String,
    pub count: u32,
    pub last_turn: u64,
    pub promoted: bool,
    /// Bounded excerpt of the most recent correction on this topic.
    pub excerpt: String,
}

/// Identical normalized requests in the window needed before a routine
/// suggestion is raised. Deliberately small: the suggestion is a hint, not a
/// committed skill.
const ROUTINE_THRESHOLD: usize = 3;

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

/// Byte index of the first ASCII case-insensitive occurrence of `marker` in `text`.
fn find_marker(text: &str, marker: &str) -> Option<usize> {
    text.as_bytes()
        .windows(marker.len())
        .position(|w| w.eq_ignore_ascii_case(marker.as_bytes()))
}

/// The part of `text` after `marker` (matched case-insensitively), clipped to the
/// first clause (up to `.`, `,` or newline) and to a bounded length. Returns `None`
/// when the marker is absent or nothing meaningful follows it.
fn capture_after(text: &str, marker: &str) -> Option<String> {
    let idx = find_marker(text, marker)?;
    let rest = text[idx + marker.len()..].trim_start();
    // Skip a leading "to " so "i want to ship" captures "ship".
    let rest = match rest.get(..3) {
        Some(prefix) if prefix.eq_ignore_ascii_case("to ") => rest[3..].trim_start(),
        _ => rest,
    };
    let clause_end = rest.find(['.', ',', '\n']).unwrap_or(rest.len());
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
        if let Some(captured) = capture_after(user_text, marker) {
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
        if let Some(captured) = capture_after(user_text, marker) {
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
        if let Some(captured) = capture_after(user_text, marker) {
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
        if let Some(captured) = capture_after(user_text, marker) {
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
        if let Some(captured) = capture_after(user_text, marker) {
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
        if let Some(captured) = capture_after(user_text, marker) {
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
                last_seen_turn: 0,
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
                last_seen_turn: 0,
            });
        } else if avg > 200 {
            out.push(Insight {
                kind: InsightKind::Preference,
                key: "communication-style".to_owned(),
                value: "prefers detailed answers".to_owned(),
                evidence: history.len() as u32,
                confidence: confidence_for(history.len() as u32),
                source: history[0].clone(),
                last_seen_turn: 0,
            });
        }
    }

    // Recurring request patterns: the same normalized request seen at least
    // ROUTINE_THRESHOLD times is a candidate saved procedure. Matching is
    // verbatim-after-normalization — "run the tests" and "run tests" do not
    // count as the same request.
    let mut request_counts: BTreeMap<String, (usize, &String)> = BTreeMap::new();
    for turn in history {
        let normalized = normalize_request(turn);
        if normalized.split(' ').count() < 3 || normalized.len() < 12 {
            continue;
        }
        let entry = request_counts
            .entry(normalized)
            .or_insert_with(|| (0usize, turn));
        entry.0 += 1;
    }
    for (normalized, (count, first)) in request_counts {
        if count < ROUTINE_THRESHOLD {
            continue;
        }
        let excerpt: String = first.trim().chars().take(80).collect();
        out.push(Insight {
            kind: InsightKind::Suggestion,
            key: format!("routine-{}", slugify(&normalized)),
            value: format!("recurring request ({count}x): {excerpt}"),
            evidence: count as u32,
            confidence: confidence_for(count as u32),
            source: (*first).clone(),
            last_seen_turn: 0,
        });
    }

    out.sort_by(|a, b| (a.kind, &a.key).cmp(&(b.kind, &b.key)));
    out
}

/// Lowercase alphanumeric word stream of a request — the equality key for
/// recurring-pattern detection.
fn normalize_request(text: &str) -> String {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Words that carry no topic signal in a correction ("no, use pnpm not npm"
/// should key on `pnpm-npm`, not on `use`).
const CORRECTION_TOPIC_SKIP: &[&str] = &[
    "actually",
    "meant",
    "instead",
    "rather",
    "should",
    "have",
    "use",
    "using",
    "just",
    "really",
    "very",
    "wrong",
    "incorrect",
    "nope",
    "dont",
    "cant",
    "wont",
    "isnt",
    "arent",
    "thats",
];

/// Topic slug for a correction: up to three content words (len >= 3, not a
/// stop word or correction filler), joined by `-`. `None` when the correction
/// names nothing specific — vague pushback is not a topic.
fn correction_topic(text: &str) -> Option<String> {
    let mut words: Vec<String> = Vec::new();
    for word in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        if word.len() < 3
            || STOP_WORDS.contains(&word)
            || CORRECTION_TOPIC_SKIP.contains(&word)
            || words.iter().any(|w| w == word)
        {
            continue;
        }
        words.push(word.to_owned());
        if words.len() == 3 {
            break;
        }
    }
    if words.is_empty() {
        None
    } else {
        Some(words.join("-"))
    }
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

/// Cap total retained insights deterministically: drop lowest confidence,
/// tie-broken by (kind, key) descending so the cut is stable.
fn enforce_cap(state: &mut DialecticState, cap: usize) {
    if state.insights.len() <= cap {
        return;
    }
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
    /// Correction frequency per topic, for aggregate promotion to USER.md.
    corrections: BTreeMap<String, CorrectionStat>,
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

    fn lock(&self) -> std::sync::MutexGuard<'_, DialecticState> {
        self.state.lock().unwrap_or_else(|e| {
            eprintln!("vikram: dialectic state mutex poisoned; recovering");
            e.into_inner()
        })
    }

    /// Whether extraction runs on this turn number (1-based user turns).
    pub fn should_run(&self, turn_number: u64) -> bool {
        self.config.enabled
            && self.config.cadence_turns > 0
            && turn_number > 0
            && turn_number.is_multiple_of(self.config.cadence_turns)
    }

    /// Observe one user turn. Records history always; extracts and merges only on cadence.
    /// Correction aggregation runs every turn (it is cheap and cadence-independent).
    /// Returns the insights extracted or promoted this call (empty on quiet turns).
    pub fn observe_turn(&self, user_text: &str) -> Vec<Insight> {
        if !self.config.enabled {
            return Vec::new();
        }
        let mut state = self.lock();
        state.turn_count += 1;
        let turn_number = state.turn_count;
        state.history.push_back(user_text.to_owned());
        while state.history.len() > self.config.depth.max(1) {
            state.history.pop_front();
        }
        let mut out = self.track_correction_locked(&mut state, user_text, turn_number);
        if !self.should_run(turn_number) {
            return out;
        }
        let history: Vec<String> = state.history.iter().cloned().collect();
        let mut fresh = extract_insights(user_text);
        fresh.extend(extract_cross_turn_insights(&history));
        out.extend(self.merge_locked(&mut state, fresh, turn_number));
        out
    }

    /// Observe a turn and extract regardless of cadence (useful for tests / session end).
    pub fn observe_turn_forced(&self, user_text: &str) -> Vec<Insight> {
        if !self.config.enabled {
            return Vec::new();
        }
        let mut state = self.lock();
        state.turn_count += 1;
        let turn_number = state.turn_count;
        state.history.push_back(user_text.to_owned());
        while state.history.len() > self.config.depth.max(1) {
            state.history.pop_front();
        }
        let mut out = self.track_correction_locked(&mut state, user_text, turn_number);
        let history: Vec<String> = state.history.iter().cloned().collect();
        let mut fresh = extract_insights(user_text);
        fresh.extend(extract_cross_turn_insights(&history));
        out.extend(self.merge_locked(&mut state, fresh, turn_number));
        out
    }

    fn merge_locked(
        &self,
        state: &mut DialecticState,
        fresh: Vec<Insight>,
        turn_number: u64,
    ) -> Vec<Insight> {
        let mut extracted = Vec::new();
        for mut insight in fresh {
            extracted.push(insight.clone());
            insight.last_seen_turn = turn_number;
            match state.insights.get_mut(&(insight.kind, insight.key.clone())) {
                Some(existing) => {
                    existing.bump();
                    existing.last_seen_turn = turn_number;
                }
                None => {
                    state
                        .insights
                        .insert((insight.kind, insight.key.clone()), insight);
                }
            }
        }
        enforce_cap(state, self.config.max_insights.max(1));
        extracted
    }

    /// Aggregate correction signals. Single corrections already reach the
    /// learning proposal pipeline; this tracks the *repeat* pattern per topic
    /// and, at `correction_promote_at`, promotes it to a durable USER.md entry
    /// plus a preference insight (which the supplement suppresses once USER.md
    /// carries it, so the fact is not injected twice). Returns the promoted
    /// insight, if any.
    fn track_correction_locked(
        &self,
        state: &mut DialecticState,
        user_text: &str,
        turn_number: u64,
    ) -> Vec<Insight> {
        if crate::learning::detect_corrections(user_text).is_empty() {
            return Vec::new();
        }
        let Some(topic) = correction_topic(user_text) else {
            return Vec::new();
        };
        let stat = state
            .corrections
            .entry(topic.clone())
            .or_insert_with(|| CorrectionStat {
                topic: topic.clone(),
                count: 0,
                last_turn: 0,
                promoted: false,
                excerpt: String::new(),
            });
        stat.count = stat.count.saturating_add(1);
        stat.last_turn = turn_number;
        stat.excerpt = user_text.trim().chars().take(120).collect();
        if stat.promoted || stat.count < self.config.correction_promote_at.max(1) {
            return Vec::new();
        }
        stat.promoted = true;

        let value = format!(
            "repeated correction on {topic} ({}x): {}",
            stat.count, stat.excerpt
        );
        let insight = Insight {
            kind: InsightKind::Preference,
            key: format!("correction-{topic}"),
            value: value.clone(),
            evidence: stat.count,
            confidence: confidence_for(stat.count),
            source: stat.excerpt.clone(),
            last_seen_turn: turn_number,
        };
        match state.insights.get_mut(&(insight.kind, insight.key.clone())) {
            Some(existing) => {
                existing.bump();
                existing.last_seen_turn = turn_number;
            }
            None => {
                state
                    .insights
                    .insert((insight.kind, insight.key.clone()), insight.clone());
            }
        }
        enforce_cap(state, self.config.max_insights.max(1));

        // Durable form: a USER.md entry via the same MemoryFile path
        // `memory.note` uses. Best-effort — a full or unreadable file must not
        // break the turn; the insight still reaches the supplement.
        let _ = self
            .user_memory_file()
            .add(&format!("correction pattern: {value}"));
        vec![insight]
    }

    /// The USER.md `MemoryFile` correction promotions write to: the configured
    /// override when set, else `memories/USER.md` under the Vikram home.
    fn user_memory_file(&self) -> crate::memory_files::MemoryFile {
        match &self.config.user_file_path {
            Some(path) => {
                crate::memory_files::MemoryFile::new(path.clone(), crate::memory_files::USER_BUDGET)
            }
            None => crate::memory_files::user_file(&crate::config::vikram_home()),
        }
    }

    /// All currently held insights, deterministically sorted (confidence desc).
    /// Returns the *stored* records: `confidence` here is the raw merged value,
    /// not the staleness-decayed one — see [`Self::decayed_insights`].
    pub fn insights(&self) -> Vec<Insight> {
        let state = self.lock();
        let mut all: Vec<Insight> = state.insights.values().cloned().collect();
        sort_insights(&mut all);
        all
    }

    /// The effective view: stored insights with staleness decay applied and
    /// forgotten ones (effective confidence below [`FORGOTTEN_CONFIDENCE`])
    /// removed. This is what the supplement renders and what `persist_to`
    /// treats as live.
    pub fn decayed_insights(&self) -> Vec<Insight> {
        let state = self.lock();
        let mut all: Vec<Insight> = state
            .insights
            .values()
            .map(|insight| {
                let mut decayed = insight.clone();
                decayed.confidence =
                    effective_confidence(insight, state.turn_count, self.config.decay_after_turns);
                decayed
            })
            .filter(|insight| insight.confidence >= FORGOTTEN_CONFIDENCE)
            .collect();
        sort_insights(&mut all);
        all
    }

    pub fn turn_count(&self) -> u64 {
        self.lock().turn_count
    }

    /// The supplement for the next turn's prefix (layer 2), or `None`.
    /// Insights whose value already appears verbatim in USER.md (e.g. a
    /// promoted correction pattern, or an operator note) are dropped so the
    /// same preference is not injected by both layers.
    pub fn supplement(&self) -> Option<String> {
        if !self.config.enabled {
            return None;
        }
        let user_entries = self.user_memory_file().load();
        let lowered: Vec<String> = user_entries.iter().map(|e| e.to_lowercase()).collect();
        let insights: Vec<Insight> = self
            .decayed_insights()
            .into_iter()
            .filter(|insight| {
                let value = insight.value.to_lowercase();
                !lowered.iter().any(|entry| entry.contains(&value))
            })
            .collect();
        render_supplement(&insights, self.config.budget_tokens)
    }

    /// Persist every live insight and correction stat as structured JSON
    /// entries under [`DIALECTIC_SCOPE`], plus the turn counter so decay
    /// survives restarts. Insights whose evidence has fully decayed are
    /// skipped, which evicts them via the stale-entry sweep below.
    /// Returns how many insight/correction entries were written.
    /// Errors are reported, never panicked on.
    pub fn persist_to(&self, store: &MemoryStore) -> usize {
        let mut written_keys = BTreeSet::new();
        let mut written = 0usize;
        let (insights, corrections, turn_count) = {
            let state = self.lock();
            (
                state.insights.values().cloned().collect::<Vec<_>>(),
                state.corrections.values().cloned().collect::<Vec<_>>(),
                state.turn_count,
            )
        };
        for insight in &insights {
            if effective_confidence(insight, turn_count, self.config.decay_after_turns)
                < FORGOTTEN_CONFIDENCE
            {
                continue;
            }
            let key = format!("{}:{}", insight.kind.as_str(), insight.key);
            if let Ok(value) = serde_json::to_string(insight)
                && store.save(&key, &value, DIALECTIC_SCOPE).is_ok()
            {
                written += 1;
                written_keys.insert(key);
            }
        }
        for stat in &corrections {
            let key = format!("correction:{}", stat.topic);
            if let Ok(value) = serde_json::to_string(stat)
                && store.save(&key, &value, DIALECTIC_SCOPE).is_ok()
            {
                written += 1;
                written_keys.insert(key);
            }
        }
        let meta_key = "meta:turn-count";
        if store
            .save(meta_key, turn_count.to_string(), DIALECTIC_SCOPE)
            .is_ok()
        {
            written_keys.insert(meta_key.to_owned());
        }
        // Remove stale entries (evicted insights, renamed keys) so storage
        // mirrors the in-memory model.
        if let Ok(entries) = store.list(DIALECTIC_SCOPE) {
            for (key, _) in entries {
                if !written_keys.contains(&key) {
                    let _ = store.delete(&key, DIALECTIC_SCOPE);
                }
            }
        }
        written
    }

    /// Load previously persisted insights, correction stats and the turn
    /// counter (e.g. at boot). Returns how many entries were loaded.
    pub fn load_from(&self, store: &MemoryStore) -> usize {
        let Ok(entries) = store.list(DIALECTIC_SCOPE) else {
            return 0;
        };
        let mut state = self.lock();
        let mut loaded = 0usize;
        for (key, value) in entries {
            if key == "meta:turn-count" {
                if let Ok(count) = value.parse::<u64>() {
                    state.turn_count = state.turn_count.max(count);
                }
                continue;
            }
            if let Ok(insight) = serde_json::from_str::<Insight>(&value) {
                state
                    .insights
                    .insert((insight.kind, insight.key.clone()), insight);
                loaded += 1;
                continue;
            }
            if let Ok(stat) = serde_json::from_str::<CorrectionStat>(&value) {
                state.corrections.insert(stat.topic.clone(), stat);
                loaded += 1;
            }
        }
        enforce_cap(&mut state, self.config.max_insights.max(1));
        loaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn engine() -> DialecticEngine {
        DialecticEngine::new(DialecticConfig {
            cadence_turns: 1,
            // Point USER.md at a path that cannot exist so supplement() never
            // reads the developer's real memory files.
            user_file_path: Some(PathBuf::from("/nonexistent/vikram-dialectic-test/USER.md")),
            ..DialecticConfig::default()
        })
    }

    fn engine_with_user_file(dir: &Path) -> DialecticEngine {
        DialecticEngine::new(DialecticConfig {
            cadence_turns: 1,
            user_file_path: Some(dir.join("USER.md")),
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
    fn a_poisoned_state_lock_recovers_instead_of_panicking() {
        let engine = engine();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = engine.state.lock().unwrap();
            panic!("poison the dialectic mutex");
        }));
        assert!(poisoned.is_err());
        assert_eq!(engine.turn_count(), 0);
        assert!(engine.insights().is_empty());
        assert!(!engine.observe_turn("I prefer tabs").is_empty());
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

    #[test]
    fn repeated_corrections_on_a_topic_stay_below_promotion() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_with_user_file(dir.path());
        for _ in 0..2 {
            engine.observe_turn_forced("no, use pnpm not npm");
        }
        assert!(
            !dir.path().join("USER.md").exists(),
            "two corrections are not yet a pattern"
        );
        assert!(
            !engine
                .insights()
                .iter()
                .any(|i| i.key.starts_with("correction-"))
        );
    }

    #[test]
    fn repeated_corrections_promote_to_a_user_md_preference() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_with_user_file(dir.path());
        let mut promoted = Vec::new();
        for _ in 0..3 {
            promoted = engine.observe_turn_forced("no, use pnpm not npm");
        }
        assert!(
            promoted.iter().any(|i| i.key == "correction-pnpm-npm"),
            "the third correction promotes the aggregate: {promoted:?}"
        );
        let entries = crate::memory_files::MemoryFile::new(
            dir.path().join("USER.md"),
            crate::memory_files::USER_BUDGET,
        )
        .load();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].contains("pnpm-npm"));
        // A fourth correction does not re-promote or duplicate the entry.
        engine.observe_turn_forced("no, use pnpm not npm");
        let entries = crate::memory_files::MemoryFile::new(
            dir.path().join("USER.md"),
            crate::memory_files::USER_BUDGET,
        )
        .load();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn promoted_correction_is_not_double_injected_in_the_supplement() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_with_user_file(dir.path());
        for _ in 0..3 {
            engine.observe_turn_forced("no, use pnpm not npm");
        }
        let supplement = engine.supplement().expect("other insights still render");
        assert!(
            !supplement.contains("repeated correction"),
            "the fact lives in USER.md now, not in both layers: {supplement}"
        );
        // The insight itself is still held and persisted.
        assert!(
            engine
                .insights()
                .iter()
                .any(|i| i.key == "correction-pnpm-npm")
        );
    }

    #[test]
    fn a_user_md_entry_matching_an_insight_value_dedupes_the_supplement() {
        let dir = tempfile::tempdir().unwrap();
        let mut user_file = crate::memory_files::MemoryFile::new(
            dir.path().join("USER.md"),
            crate::memory_files::USER_BUDGET,
        );
        user_file.add("prefers tabs over spaces").unwrap();
        let engine = engine_with_user_file(dir.path());
        engine.observe_turn_forced("I prefer tabs over spaces");
        let supplement = engine.supplement().unwrap_or_default();
        assert!(
            !supplement.contains("prefers tabs over spaces"),
            "USER.md already carries it: {supplement}"
        );
    }

    #[test]
    fn correction_stats_and_turn_count_survive_persist_and_reload() {
        let store = MemoryStore::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_with_user_file(dir.path());
        for _ in 0..2 {
            engine.observe_turn_forced("no, use pnpm not npm");
        }
        engine.persist_to(&store);
        let reloaded = engine_with_user_file(dir.path());
        assert!(reloaded.load_from(&store) >= 1);
        assert_eq!(reloaded.turn_count(), 2, "turn counter round-trips");
        // One more correction crosses the threshold on the reloaded engine.
        let out = reloaded.observe_turn_forced("no, use pnpm not npm");
        assert!(out.iter().any(|i| i.key == "correction-pnpm-npm"));
        assert!(dir.path().join("USER.md").exists());
    }

    #[test]
    fn stale_insights_decay_and_are_eventually_forgotten() {
        let engine = DialecticEngine::new(DialecticConfig {
            cadence_turns: 1,
            decay_after_turns: 2,
            user_file_path: Some(PathBuf::from("/nonexistent/vikram-dialectic-test/USER.md")),
            ..DialecticConfig::default()
        });
        engine.observe_turn_forced("I prefer tabs");
        let fresh = engine
            .decayed_insights()
            .into_iter()
            .find(|i| i.key == "tabs")
            .expect("fresh insight is live");
        assert!((fresh.confidence - 0.6).abs() < 1e-9);

        // Idle turns do not re-evidence the preference; each window of 2 turns
        // multiplies effective confidence by 0.7.
        for _ in 0..2 {
            engine.observe_turn_forced("tell me a story about the sea");
        }
        let weakened = engine
            .decayed_insights()
            .into_iter()
            .find(|i| i.key == "tabs")
            .expect("still above the forget threshold");
        assert!((weakened.confidence - 0.42).abs() < 1e-9);

        // Enough unobserved turns later it is forgotten entirely.
        for _ in 0..8 {
            engine.observe_turn_forced("tell me another story about the sea");
        }
        assert!(
            !engine.decayed_insights().iter().any(|i| i.key == "tabs"),
            "a fully decayed insight leaves the effective view"
        );
        // ...but the stored record is still there until the next persist evicts it.
        assert!(engine.insights().iter().any(|i| i.key == "tabs"));
        let store = MemoryStore::open_in_memory().unwrap();
        engine.persist_to(&store);
        assert!(
            store
                .get("preference:tabs", DIALECTIC_SCOPE)
                .unwrap()
                .is_none(),
            "persist evicts forgotten insights"
        );
    }

    #[test]
    fn decay_is_disabled_when_the_window_is_zero() {
        let engine = DialecticEngine::new(DialecticConfig {
            cadence_turns: 1,
            decay_after_turns: 0,
            user_file_path: Some(PathBuf::from("/nonexistent/vikram-dialectic-test/USER.md")),
            ..DialecticConfig::default()
        });
        engine.observe_turn_forced("I prefer tabs");
        for _ in 0..20 {
            engine.observe_turn_forced("tell me a story about the sea");
        }
        let insight = engine
            .decayed_insights()
            .into_iter()
            .find(|i| i.key == "tabs")
            .expect("no decay configured");
        assert!((insight.confidence - 0.6).abs() < 1e-9);
    }

    #[test]
    fn a_recurring_request_surfaces_as_a_suggestion() {
        let engine = engine();
        for _ in 0..3 {
            engine.observe_turn_forced("please run the full test suite now");
        }
        let suggestion = engine
            .insights()
            .into_iter()
            .find(|i| i.kind == InsightKind::Suggestion)
            .expect("three identical requests earn a routine suggestion");
        assert!(suggestion.key.starts_with("routine-"));
        assert!(suggestion.value.contains("recurring request"));
        let supplement = engine.supplement().expect("suggestion renders");
        assert!(supplement.contains("[suggestion]"));
    }

    #[test]
    fn varied_requests_do_not_form_a_routine() {
        let engine = engine();
        for text in [
            "please run the full test suite now",
            "run the full test suite please",
            "check the full test suite output",
        ] {
            engine.observe_turn_forced(text);
        }
        assert!(
            !engine
                .insights()
                .iter()
                .any(|i| i.kind == InsightKind::Suggestion),
            "near-misses are not the same request"
        );
    }

    #[test]
    fn vague_corrections_do_not_create_topics() {
        assert_eq!(correction_topic("no, that's wrong"), None);
        assert_eq!(
            correction_topic("no, use pnpm not npm"),
            Some("pnpm-npm".into())
        );
    }
}
