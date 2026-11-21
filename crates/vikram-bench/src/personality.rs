//! Personality-consistency harness: does the assistant's voice stay stable
//! across turns?
//!
//! Fully deterministic and offline — no model calls, no network. A fixed
//! corpus of seeded assistant outputs ([`BASELINE_TURNS`]) defines the
//! persona's style fingerprint; observed turn texts are compared against it
//! with a hand-rolled similarity score (token-set overlap plus per-feature
//! deltas — no ML model, no learned weights).
//!
//! Drift is reported as a number and as a `WARN` flag when a turn scores
//! below the threshold. Nothing here fails the run: the harness reports
//! consistency, it does not judge it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Default drift threshold. A turn scoring below this is flagged WARN.
/// Calibrated so persona-consistent turns (including list-heavy ones) stay
/// above it while clearly off-voice text (e.g. legal boilerplate, ~0.40)
/// falls below.
pub const DEFAULT_THRESHOLD: f64 = 0.5;

/// Hedging / uncertainty markers, matched as whole words.
const HEDGES: &[&str] = &[
    "maybe",
    "perhaps",
    "possibly",
    "probably",
    "might",
    "could",
    "likely",
    "seems",
    "seem",
    "guess",
    "i think",
    "it seems",
    "i believe",
    "not sure",
];

/// Formal register markers.
const FORMAL: &[&str] = &[
    "furthermore",
    "moreover",
    "therefore",
    "regarding",
    "pursuant",
    "kindly",
    "shall",
    "hereby",
    "accordingly",
    "consequently",
];

/// Casual register markers.
const CASUAL: &[&str] = &[
    "gonna", "wanna", "kinda", "yeah", "yep", "nope", "lol", "btw", "stuff", "cool", "awesome",
    "dude", "cheers",
];

const GREETINGS: &[&str] = &[
    "hi",
    "hello",
    "hey",
    "good morning",
    "good afternoon",
    "good evening",
];

const SIGNOFFS: &[&str] = &[
    "let me know",
    "feel free",
    "hope this helps",
    "thanks for",
    "thank you",
    "cheers",
    "regards",
    "best regards",
    "happy to help",
];

/// Style fingerprint of one assistant output (or an aggregate of several).
#[derive(Debug, Clone, PartialEq)]
pub struct Fingerprint {
    /// Normalised content tokens (lowercase alphanumeric runs, len >= 2).
    pub tokens: BTreeSet<String>,
    /// First content tokens of the text — the typical opener vocabulary.
    pub openers: Vec<String>,
    /// Last content tokens of the text — the typical closer vocabulary.
    pub closers: Vec<String>,
    /// Non-ASCII symbol characters per character of text.
    pub emoji_rate: f64,
    /// Lines that look like list items per non-empty line.
    pub list_ratio: f64,
    /// Hedging marker hits per token.
    pub hedge_rate: f64,
    /// (formal - casual) marker hits per token.
    pub tone: f64,
    /// Fraction of the source text(s) opening with a greeting (0.0 or 1.0
    /// for a single text, a corpus fraction for a merged fingerprint).
    pub greeting: f64,
    /// Fraction of the source text(s) closing with a sign-off phrase.
    pub signoff: f64,
}

fn tokens_of(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .map(|t| t.to_lowercase())
        .collect()
}

fn count_phrase(text: &str, phrases: &[&str]) -> usize {
    let lower = text.to_lowercase();
    phrases.iter().map(|p| lower.match_indices(p).count()).sum()
}

fn is_list_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("• ")
        || (t.len() > 2
            && t.chars().next().is_some_and(|c| c.is_ascii_digit())
            && t[1..].trim_start().starts_with('.')
            && t[1..].trim_start().len() > 1)
}

/// Extracts the deterministic style fingerprint of one text.
pub fn fingerprint(text: &str) -> Fingerprint {
    let tokens = tokens_of(text);
    let token_set: BTreeSet<String> = tokens.iter().cloned().collect();

    let char_count = text.chars().count().max(1) as f64;
    let emoji_count = text
        .chars()
        .filter(|c| !c.is_ascii() && !c.is_alphanumeric())
        .count();

    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let list_lines = lines.iter().filter(|l| is_list_line(l)).count();
    let list_ratio = if lines.is_empty() {
        0.0
    } else {
        list_lines as f64 / lines.len() as f64
    };

    let lower = text.to_lowercase();
    let first_line = lower.lines().next().unwrap_or("").trim_start();
    let greeting = GREETINGS.iter().any(|g| {
        first_line.starts_with(g)
            && first_line[g.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric())
    });
    let signoff = SIGNOFFS.iter().any(|s| lower.contains(s));

    let token_count = tokens.len().max(1) as f64;
    let formal = count_phrase(text, FORMAL) as f64;
    let casual = count_phrase(text, CASUAL) as f64;

    Fingerprint {
        tokens: token_set,
        openers: tokens.iter().take(4).cloned().collect(),
        closers: tokens
            .iter()
            .rev()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect(),
        emoji_rate: emoji_count as f64 / char_count,
        list_ratio,
        hedge_rate: count_phrase(text, HEDGES) as f64 / token_count,
        tone: (formal - casual) / token_count,
        greeting: f64::from(greeting),
        signoff: f64::from(signoff),
    }
}

/// Merges fingerprints (used to build the baseline over the whole corpus).
/// Scalar rates are averaged over corpus size; opener/closer vocabulary is
/// the union across turns, since different turns open and close differently.
fn merge(fps: &[Fingerprint]) -> Fingerprint {
    let mut tokens = BTreeSet::new();
    let mut openers: Vec<String> = Vec::new();
    let mut closers: Vec<String> = Vec::new();
    let push_unique = |v: &mut Vec<String>, w: &str| {
        if !v.iter().any(|x| x == w) {
            v.push(w.to_string());
        }
    };
    let mut emoji_rate = 0.0;
    let mut list_ratio = 0.0;
    let mut hedge_rate = 0.0;
    let mut tone = 0.0;
    let mut greeting = 0.0;
    let mut signoff = 0.0;
    for fp in fps {
        tokens.extend(fp.tokens.iter().cloned());
        for w in &fp.openers {
            push_unique(&mut openers, w);
        }
        for w in &fp.closers {
            push_unique(&mut closers, w);
        }
        emoji_rate += fp.emoji_rate;
        list_ratio += fp.list_ratio;
        hedge_rate += fp.hedge_rate;
        tone += fp.tone;
        greeting += fp.greeting;
        signoff += fp.signoff;
    }
    let n = fps.len().max(1) as f64;
    Fingerprint {
        tokens,
        openers,
        closers,
        emoji_rate: emoji_rate / n,
        list_ratio: list_ratio / n,
        hedge_rate: hedge_rate / n,
        tone: tone / n,
        greeting: greeting / n,
        signoff: signoff / n,
    }
}

/// Overlap coefficient |A∩B| / min(|A|,|B|): 1 when one set contains the other,
/// which is the right shape for "does this turn use the persona's vocabulary"
/// — a short turn need not cover the whole baseline union to be on-voice.
fn vocab_overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return if a.is_empty() && b.is_empty() {
            1.0
        } else {
            0.0
        };
    }
    a.intersection(b).count() as f64 / a.len().min(b.len()) as f64
}

fn overlap(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return if a.is_empty() && b.is_empty() {
            1.0
        } else {
            0.0
        };
    }
    let sa: BTreeSet<&String> = a.iter().collect();
    let sb: BTreeSet<&String> = b.iter().collect();
    sa.intersection(&sb).count() as f64 / sa.len().min(sb.len()) as f64
}

/// 1 - |a-b| / max(|a|,|b|,1): 1 when equal, 0 when maximally different.
fn rate_sim(a: f64, b: f64) -> f64 {
    (1.0 - (a - b).abs() / a.abs().max(b.abs()).max(1.0)).clamp(0.0, 1.0)
}

/// Deterministic similarity in [0, 1] between two fingerprints:
/// 45% token-set overlap coefficient, 35% scalar feature agreement (emoji
/// rate, list ratio, hedge rate, tone, greeting, sign-off rates), 20%
/// opener/closer vocabulary overlap.
pub fn similarity(a: &Fingerprint, b: &Fingerprint) -> f64 {
    let token_score = vocab_overlap(&a.tokens, &b.tokens);

    let scalar_score = (rate_sim(a.emoji_rate, b.emoji_rate)
        + rate_sim(a.list_ratio, b.list_ratio)
        + rate_sim(a.hedge_rate, b.hedge_rate)
        + rate_sim(a.tone, b.tone)
        + rate_sim(a.greeting, b.greeting)
        + rate_sim(a.signoff, b.signoff))
        / 6.0;

    let vocab_score = (overlap(&a.openers, &b.openers) + overlap(&a.closers, &b.closers)) / 2.0;

    0.45 * token_score + 0.35 * scalar_score + 0.20 * vocab_score
}

/// Similarity of an observed text to the baseline corpus fingerprint.
pub fn score_text(text: &str) -> f64 {
    similarity(&fingerprint(text), &baseline())
}

/// Aggregate fingerprint of the seeded baseline corpus.
pub fn baseline() -> Fingerprint {
    merge(
        &BASELINE_TURNS
            .iter()
            .map(|t| fingerprint(t))
            .collect::<Vec<_>>(),
    )
}

// ---------------------------------------------------------------- baseline corpus

/// Seeded assistant outputs representative of the persona: concise, warm,
/// mostly prose with occasional lists, rare emoji, light hedging, greetings
/// and sign-offs. These strings are the fixed reference the harness scores
/// against — they are fixtures, not model output.
pub const BASELINE_TURNS: &[&str] = &[
    "Hi! I took a look at the failing test and found the issue. The assertion was comparing a timestamp in milliseconds against one in seconds, so it only failed around midnight UTC. I fixed the conversion in `clock.rs` and the suite is green now. Let me know if you want me to add a regression test for the boundary case.",
    "Sure, here's the summary:\n\n- The gateway retries delivery up to 3 times with exponential backoff.\n- Failed deliveries land in the `outbox` table with status `dead` after the last attempt.\n- The worker drains the outbox every 30 seconds.\n\nOne thing worth noting: the retry budget is per-attempt, not per-message, so a flaky platform can still burn through it quickly. Happy to walk through the state machine if that helps.",
    "Good question. The short answer is that we could support it, but it would need a small migration to store the preference per workspace rather than per user. I'd probably start with the per-user path since it covers most cases, then generalise once we see real demand. Want me to sketch the schema change?",
    "Done — pushed the fix to `main`. The root cause was a missing await in the spawn path, so the task was being dropped before it could register its handle. I also tightened the test so it fails if the handle is ever dropped again. CI should be green in a few minutes.",
    "Hey! I checked the logs and nothing is on fire. There were a couple of slow queries around 14:02 UTC but they resolved on their own — probably a cold cache after the deploy. If it happens again I can add an index on the `created_at` column, which should cut the scan roughly in half.",
    "Thanks for the context, that makes sense. I'll update the docs to mention that the flag is deprecated and point people at `--strict` instead. Might also be worth a note in the changelog since it changes default behaviour for anyone still passing the old flag.",
    "Here's what I'd suggest: keep the retry logic as-is, but make the backoff cap configurable so operators can tune it for their platform. It seems like a small change — maybe 20 lines — and it avoids the one-size-fits-all problem we hit last quarter. Let me know what you think!",
];

/// Default observed corpus used when no `--turns`/`--text` input is given:
/// persona-consistent turns so the harness demonstrates a clean run.
pub const OBSERVED_TURNS: &[&str] = &[
    "Hi! I found the bug — it was an off-by-one in the pagination cursor. Fixed it and added a test covering the last page edge case. Let me know if anything else looks off.",
    "Sure, quick summary:\n\n- Moved the config loader behind a trait so tests can inject fixtures.\n- Deleted the duplicated parsing code in `cli.rs`.\n- Updated the two call sites that were reaching into the old struct.\n\nBuild is green and clippy is clean. Happy to explain any of the pieces if useful.",
    "Good catch — yes, we could probably make that configurable, but I'd want to check how often people actually hit the limit first. Maybe start with a warning in the logs so we get real data before adding a flag. Sound reasonable?",
];

// ---------------------------------------------------------------- scoring + report

/// One observed turn's score.
#[derive(Debug, Clone)]
pub struct TurnScore {
    pub name: String,
    pub score: f64,
    pub warned: bool,
}

/// The full consistency report: per-turn scores plus the aggregate.
#[derive(Debug)]
pub struct Report {
    pub turns: Vec<TurnScore>,
    pub mean: f64,
    pub min: f64,
    pub threshold: f64,
    pub warnings: usize,
}

/// Scores each observed text against the baseline fingerprint.
pub fn score_all(texts: &[(String, String)], threshold: f64) -> Report {
    let base = baseline();
    let mut turns = Vec::with_capacity(texts.len());
    let mut warnings = 0usize;
    for (name, text) in texts {
        let score = similarity(&fingerprint(text), &base);
        let warned = score < threshold;
        warnings += usize::from(warned);
        turns.push(TurnScore {
            name: name.clone(),
            score,
            warned,
        });
    }
    let mean = if turns.is_empty() {
        0.0
    } else {
        turns.iter().map(|t| t.score).sum::<f64>() / turns.len() as f64
    };
    let min = turns.iter().map(|t| t.score).fold(f64::INFINITY, f64::min);
    let empty = turns.is_empty();
    Report {
        turns,
        mean,
        min: if empty { 0.0 } else { min },
        threshold,
        warnings,
    }
}

fn fmt_score(v: f64) -> String {
    format!("{v:.3}")
}

/// Prints the report table and warnings. Deterministic, stdout only.
pub fn print_report(report: &Report) {
    println!("personality consistency — deterministic style fingerprint, offline");
    println!("{:<28} | {:>8} | {:<6}", "turn", "score", "status");
    println!("{}-+-{}-+-{}", "-".repeat(28), "-".repeat(8), "-".repeat(6));
    for turn in &report.turns {
        println!(
            "{:<28} | {:>8} | {:<6}",
            turn.name,
            fmt_score(turn.score),
            if turn.warned { "WARN" } else { "ok" }
        );
    }
    println!(
        "{:<28} | {:>8} |",
        "aggregate (mean)",
        fmt_score(report.mean)
    );
    println!("{:<28} | {:>8} |", "aggregate (min)", fmt_score(report.min));
    for turn in report.turns.iter().filter(|t| t.warned) {
        println!(
            "note: `{}` scored {:.3}, below threshold {:.3} — style drift, not a failure",
            turn.name, turn.score, report.threshold
        );
    }
    if report.warnings > 0 {
        println!(
            "{} turn(s) flagged as drift warnings (threshold {:.2})",
            report.warnings, report.threshold
        );
    } else {
        println!("no drift warnings (threshold {:.2})", report.threshold);
    }
}

/// Loads observed turns: `*.txt` files under `dir` (file stem as the turn
/// name) plus any inline `--text` values named `text-N`. Returns an error on
/// unreadable files or an empty corpus.
pub fn load_turns(dir: Option<&Path>, texts: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (i, text) in texts.iter().enumerate() {
        out.push((format!("text-{}", i + 1), text.clone()));
    }
    if let Some(dir) = dir {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "txt"))
            .collect();
        entries.sort();
        if entries.is_empty() {
            return Err(format!("no .txt turn files in {}", dir.display()));
        }
        for path in entries {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            out.push((name, text));
        }
    }
    if out.is_empty() {
        return Err("no observed turns: pass --turns DIR, --text TEXT, or omit both to use the seeded corpus".to_string());
    }
    Ok(out)
}

/// Runs the harness end to end and prints the report. Never fails on drift;
/// only on bad arguments or unreadable input.
pub fn personality(
    turns_dir: Option<&Path>,
    texts: &[String],
    threshold: Option<f64>,
) -> Result<(), String> {
    let threshold = threshold.unwrap_or(DEFAULT_THRESHOLD);
    if !(0.0..=1.0).contains(&threshold) {
        return Err(format!("threshold must be in [0, 1], got {threshold}"));
    }
    let observed = if turns_dir.is_none() && texts.is_empty() {
        OBSERVED_TURNS
            .iter()
            .enumerate()
            .map(|(i, t)| (format!("seeded-{}", i + 1), (*t).to_string()))
            .collect()
    } else {
        load_turns(turns_dir, texts)?
    };
    let report = score_all(&observed, threshold);
    print_report(&report);
    Ok(())
}

/// `vikram-bench personality [--turns DIR] [--text TEXT]... [--threshold F]`.
pub fn cli(args: &[String]) -> Result<(), String> {
    let mut turns_dir: Option<PathBuf> = None;
    let mut texts: Vec<String> = Vec::new();
    let mut threshold: Option<f64> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--turns" => {
                i += 1;
                turns_dir = Some(PathBuf::from(
                    args.get(i).ok_or("--turns needs a directory path")?.clone(),
                ));
            }
            "--text" => {
                i += 1;
                texts.push(args.get(i).ok_or("--text needs a string")?.clone());
            }
            "--threshold" => {
                i += 1;
                let raw = args.get(i).ok_or("--threshold needs a number")?;
                threshold = Some(
                    raw.parse::<f64>()
                        .map_err(|_| format!("--threshold must be a number, got `{raw}`"))?,
                );
            }
            "-h" | "--help" => {
                println!("vikram-bench personality [--turns DIR] [--text TEXT]... [--threshold F]");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
        i += 1;
    }
    personality(turns_dir.as_deref(), &texts, threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_extraction_is_deterministic() {
        let text =
            "Hi! Here's the plan:\n\n- fix the parser\n- run tests\n\nLet me know if this helps!";
        let a = fingerprint(text);
        let b = fingerprint(text);
        assert_eq!(a, b);
        assert_eq!(a.greeting, 1.0);
        assert_eq!(a.signoff, 1.0);
        assert!(a.list_ratio > 0.0);
        assert!(a.tokens.contains("parser"));
        assert!(a.tokens.contains("tests"));
    }

    #[test]
    fn fingerprint_catches_emoji_and_casual_markers() {
        let casual = fingerprint("yo lol this is awesome, gonna fix it rn");
        let formal = fingerprint("Furthermore, the implementation shall proceed accordingly.");
        assert!(casual.tone < formal.tone);
        let emoji = fingerprint("done ✅ all good 👍");
        assert!(emoji.emoji_rate > 0.0);
        assert_eq!(fingerprint("plain ascii text").emoji_rate, 0.0);
    }

    #[test]
    fn baseline_is_stable_across_calls() {
        assert_eq!(baseline(), baseline());
        assert!(baseline().tokens.len() > 50);
    }

    #[test]
    fn matching_fixture_scores_high() {
        // A turn in the persona's voice scores well above the drift threshold.
        let matching = "Hi! I found the issue in the retry loop — it was double-counting attempts. Fixed it and the flaky test now passes consistently. Let me know if you'd like me to look at the backoff curve too.";
        let score = score_text(matching);
        assert!(score > DEFAULT_THRESHOLD, "matching score {score}");
    }

    #[test]
    fn drifting_fixture_scores_low_and_warns() {
        // Corporate-legal boilerplate with none of the persona's markers.
        let drifting = "Pursuant to the aforementioned agreement, the undersigned party shall hereby acknowledge receipt of the deliverables. Consequently, further correspondence regarding this matter is discouraged.";
        let score = score_text(drifting);
        assert!(score < DEFAULT_THRESHOLD, "drifting score {score}");
        let report = score_all(
            &[("drift".to_string(), drifting.to_string())],
            DEFAULT_THRESHOLD,
        );
        assert_eq!(report.warnings, 1);
        assert!(report.turns[0].warned);
    }

    #[test]
    fn identical_text_scores_near_one() {
        let text = BASELINE_TURNS[0];
        let fp = fingerprint(text);
        let score = similarity(&fp, &baseline());
        assert!(score > 0.8, "self-similarity {score}");
    }

    #[test]
    fn empty_inputs_are_handled() {
        let fp = fingerprint("");
        assert!(fp.tokens.is_empty());
        let report = score_all(&[], DEFAULT_THRESHOLD);
        assert_eq!(report.mean, 0.0);
        assert_eq!(report.warnings, 0);
    }

    #[test]
    fn load_turns_reads_txt_files_and_inline_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("turn_one.txt"), "hello there").unwrap();
        std::fs::write(dir.path().join("ignore.md"), "not a turn").unwrap();
        let turns = load_turns(Some(dir.path()), &["inline text".to_string()]).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].0, "text-1");
        assert_eq!(turns[1].0, "turn_one");
        assert!(load_turns(Some(dir.path()), &[]).is_ok());
        assert!(load_turns(Some(dir.path().join("missing").as_path()), &[]).is_err());
        assert!(load_turns(None, &[]).is_err());
    }

    #[test]
    fn cli_rejects_bad_args_and_thresholds() {
        assert!(cli(&["--bogus".to_string()]).is_err());
        assert!(cli(&["--text".to_string()]).is_err());
        assert!(cli(&["--threshold".to_string()]).is_err());
        assert!(cli(&["--threshold".to_string(), "abc".to_string()]).is_err());
        assert!(
            cli(&[
                "--threshold".to_string(),
                "1.5".to_string(),
                "--text".to_string(),
                "hi".to_string(),
            ])
            .is_err()
        );
        // A normal run succeeds and reports, warnings included.
        assert!(cli(&["--text".to_string(), "hello, this is a test".to_string()]).is_ok());
    }

    #[test]
    fn seeded_observed_corpus_passes_clean() {
        let turns: Vec<(String, String)> = OBSERVED_TURNS
            .iter()
            .enumerate()
            .map(|(i, t)| (format!("seeded-{i}"), (*t).to_string()))
            .collect();
        let report = score_all(&turns, DEFAULT_THRESHOLD);
        assert_eq!(report.warnings, 0, "unexpected drift: {:?}", report.turns);
        assert!(report.mean > 0.55, "mean {}", report.mean);
    }
}
