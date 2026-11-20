//! Learning-system benchmarks: measurable-improvement checks over the real
//! stores, not model calls.
//!
//! Each benchmark seeds a fresh temp directory, runs the same code paths the
//! runtime's learning loop uses, and reports the counts it actually observed —
//! recall hit rates, list ordering, version/rollback state, proposal statuses,
//! consolidation dedup/supersession. Nothing is estimated and nothing touches
//! the network or a provider credential, so a run is deterministic apart from
//! wall-clock timestamps the stores stamp themselves.
//!
//! A benchmark "passes" only when the store did what the learning loop claims
//! it does; the metrics are reported either way so a regression is visible as
//! a number, not just a red row.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use vikram_contracts::SessionId;
use vikram_evolution::{
    EvolutionStore, Experience, ExperienceCollector, ExperienceOutcome, HeuristicReflector,
    ProposalKind, ProposalStatus, ReflectionEngine, Sensitivity,
};
use vikram_runtime::MemoryStore;
use vikram_runtime::consolidation::{ConsolidationPolicy, ConsolidationStore, ObservationStatus};
use vikram_runtime::learning::{proposal_from_evolution, scope_of};
use vikram_skills::{History, Manager, Manifest, SkillId, hash_dir};

/// One benchmark's outcome: a pass/fail verdict plus every metric it measured.
pub struct Report {
    pub name: &'static str,
    pub passed: bool,
    pub metrics: Vec<(String, f64)>,
    pub notes: Vec<String>,
}

impl Report {
    fn new(name: &'static str, passed: bool, metrics: Vec<(&str, f64)>) -> Self {
        Self {
            name,
            passed,
            metrics: metrics
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            notes: Vec::new(),
        }
    }
}

/// Runs every learning benchmark on fresh temp state. The temp directory stays
/// alive for the whole suite and is removed on return.
pub fn run_all() -> Result<Vec<Report>, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let reports = vec![
        memory_recall(&dir.path().join("memory"))?,
        experience_recall(&dir.path().join("experience"))?,
        skill_lifecycle(&dir.path().join("skill"))?,
        correction_pipeline(&dir.path().join("correction"))?,
        consolidation(&dir.path().join("consolidation"))?,
    ];
    Ok(reports)
}

// ---------------------------------------------------------------- memory_recall

const MEMORY_SEED: usize = 40;
const MEMORY_QUERIES: usize = 20;

/// Seeds `MEMORY_SEED` memories into a file-backed `MemoryStore`, then asks
/// `recall_terms` — the same entry point the context engine uses — for each of
/// `MEMORY_QUERIES` unique tokens. A hit is the seeded key appearing in the
/// top-8; the deny-first scope guard is checked by repeating one query under a
/// scope nothing was written to.
fn memory_recall(dir: &Path) -> Result<Report, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let store = MemoryStore::open(dir.join("memory.db")).map_err(|e| e.to_string())?;
    for i in 0..MEMORY_SEED {
        store
            .save(
                format!("note-{i:03}"),
                format!("Project note {i}: the unique token quixotic-{i:03} marks this memory."),
                "project",
            )
            .map_err(|e| e.to_string())?;
    }

    let scopes = vec!["project".to_string()];
    let mut hits = 0usize;
    let mut returned = 0usize;
    let mut scope_violations = 0usize;
    for i in 0..MEMORY_QUERIES {
        let terms = vec![format!("quixotic-{i:03}")];
        let rows = store
            .recall_terms(&terms, &scopes, 8)
            .map_err(|e| e.to_string())?;
        returned += rows.len();
        if rows
            .iter()
            .any(|(_, key, _)| key == &format!("note-{i:03}"))
        {
            hits += 1;
        }
        scope_violations += rows
            .iter()
            .filter(|(scope, _, _)| scope != "project")
            .count();
    }

    // The same token under a scope that was never written must return nothing:
    // recall is deny-first, and a store that ignored scopes would leak here.
    let foreign = store
        .recall_terms(
            &["quixotic-000".to_string()],
            &["other-scope".to_string()],
            8,
        )
        .map_err(|e| e.to_string())?;

    let hit_rate = hits as f64 / MEMORY_QUERIES as f64;
    let passed = hits == MEMORY_QUERIES && foreign.is_empty() && scope_violations == 0;
    Ok(Report::new(
        "memory_recall",
        passed,
        vec![
            ("memories_seeded", MEMORY_SEED as f64),
            ("queries", MEMORY_QUERIES as f64),
            ("hits", hits as f64),
            ("hit_rate", hit_rate),
            ("mean_returned", returned as f64 / MEMORY_QUERIES as f64),
            ("scope_violations", scope_violations as f64),
            ("foreign_scope_hits", foreign.len() as f64),
        ],
    ))
}

// ---------------------------------------------------------------- experience_recall

/// Records experiences through `ExperienceCollector` (the sanitising write
/// path), then checks `list` ordering, the agent filter, `list_experiences_since`,
/// and point lookup by id.
fn experience_recall(dir: &Path) -> Result<Report, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let store = EvolutionStore::open(dir.join("evolution.db")).map_err(|e| e.to_string())?;
    let collector = ExperienceCollector::new(store.clone());

    let base = 1_700_000_000_000u64;
    let bench_count = 12u64;
    let other_count = 3u64;
    for i in 0..bench_count {
        collector
            .record(Experience {
                id: String::new(),
                timestamp_ms: base + i * 1_000,
                agent_id: "bench-agent".to_string(),
                project_id: None,
                task_category: "conversation".to_string(),
                user_request: format!("bench task {i}"),
                outcome: if i % 3 == 0 {
                    ExperienceOutcome::Failure
                } else {
                    ExperienceOutcome::Success
                },
                tools_used: vec!["fs.read".to_string()],
                errors: Vec::new(),
                corrections: Vec::new(),
                feedback: None,
                reusable: false,
                sensitivity: Sensitivity::Private,
                metadata: json!({}),
            })
            .map_err(|e| e.to_string())?;
    }
    for i in 0..other_count {
        collector
            .record(Experience {
                id: String::new(),
                timestamp_ms: base + 500 + i * 1_000,
                agent_id: "other-agent".to_string(),
                project_id: None,
                task_category: "conversation".to_string(),
                user_request: format!("other task {i}"),
                outcome: ExperienceOutcome::Success,
                tools_used: Vec::new(),
                errors: Vec::new(),
                corrections: Vec::new(),
                feedback: None,
                reusable: false,
                sensitivity: Sensitivity::Private,
                metadata: json!({}),
            })
            .map_err(|e| e.to_string())?;
    }

    let recent = collector
        .list(Some("bench-agent"), 5)
        .map_err(|e| e.to_string())?;
    let bench_all = collector
        .list(Some("bench-agent"), 100)
        .map_err(|e| e.to_string())?;
    let since = store
        .list_experiences_since(base, 100)
        .map_err(|e| e.to_string())?;

    let ordering_ok = recent.len() == 5
        && recent
            .windows(2)
            .all(|w| w[0].timestamp_ms >= w[1].timestamp_ms)
        && recent
            .first()
            .is_some_and(|e| e.timestamp_ms == base + (bench_count - 1) * 1_000);
    let agent_filter_ok = bench_all.len() == bench_count as usize;
    let roundtrip_ok = recent
        .first()
        .and_then(|e| collector.get(&e.id).ok().flatten())
        .is_some_and(|e| e.user_request == format!("bench task {}", bench_count - 1));

    let passed = ordering_ok && agent_filter_ok && roundtrip_ok && since.len() == 15;
    Ok(Report::new(
        "experience_recall",
        passed,
        vec![
            ("recorded", (bench_count + other_count) as f64),
            ("recent_len", recent.len() as f64),
            ("bench_agent_total", bench_all.len() as f64),
            ("since_count", since.len() as f64),
            ("ordering_ok", f64::from(ordering_ok)),
            ("agent_filter_ok", f64::from(agent_filter_ok)),
            ("get_roundtrip_ok", f64::from(roundtrip_ok)),
        ],
    ))
}

// ---------------------------------------------------------------- skill_lifecycle

/// create -> snapshot/record -> edit -> snapshot/record -> rollback, all on a
/// temp skills root and temp history state dir. Proves the skill versioning
/// path the learning loop's skill proposals rely on.
fn skill_lifecycle(dir: &Path) -> Result<Report, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let root = dir.join("skills");
    let manager = Manager::at(root.clone());
    manager
        .create("bench-skill", "Bench lifecycle skill", "version one body")
        .map_err(|e| e.to_string())?;
    let skill_dir = root.join("bench-skill");
    let id = SkillId::new("bench-skill").map_err(|e| e.to_string())?;
    let state = dir.join("state");

    let history = History::load_in(&state, &id).map_err(|e| e.to_string())?;
    let files = history
        .snapshot(&skill_dir, 1_000)
        .map_err(|e| e.to_string())?;
    history
        .record(Manifest {
            name: id.clone(),
            version: None,
            content_hash: hash_dir(&skill_dir).map_err(|e| e.to_string())?,
            source: None,
            rev: None,
            installed_ms: 1_000,
            files,
        })
        .map_err(|e| e.to_string())?;

    manager
        .edit("bench-skill", "version two body")
        .map_err(|e| e.to_string())?;

    let history = History::load_in(&state, &id).map_err(|e| e.to_string())?;
    let files = history
        .snapshot(&skill_dir, 2_000)
        .map_err(|e| e.to_string())?;
    history
        .record(Manifest {
            name: id.clone(),
            version: None,
            content_hash: hash_dir(&skill_dir).map_err(|e| e.to_string())?,
            source: None,
            rev: None,
            installed_ms: 2_000,
            files,
        })
        .map_err(|e| e.to_string())?;

    let history = History::load_in(&state, &id).map_err(|e| e.to_string())?;
    let versions = history.entries().len();
    let restored = history.rollback(&skill_dir).map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(skill_dir.join("SKILL.md")).map_err(|e| e.to_string())?;
    let restored_ok = restored.installed_ms == 1_000
        && text.contains("version one body")
        && !text.contains("version two body");

    let passed = versions == 2 && restored_ok;
    Ok(Report::new(
        "skill_lifecycle",
        passed,
        vec![
            ("versions_recorded", versions as f64),
            ("rollback_restored", f64::from(restored_ok)),
        ],
    ))
}

// ---------------------------------------------------------------- correction_pipeline

/// The correction path end to end, minus the turn loop: an experience carrying
/// a user correction goes through `ExperienceCollector::record` (the
/// sanitising write), `ReflectionEngine::run_reflection` with the heuristic
/// reflector (no model), the auto-apply memory write, and then the runtime's
/// own `proposal_from_evolution` + `scope_of` mapping, proving the proposal
/// would land in a scope `MemoryStore::recall_terms` actually reads.
///
/// `learning::detect_corrections` itself is `pub(crate)` to vikram-runtime, so
/// the benchmark feeds the correction note directly — the same string the
/// detector would have produced — rather than re-implementing the heuristic.
fn correction_pipeline(dir: &Path) -> Result<Report, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let store = EvolutionStore::open(dir.join("evolution.db")).map_err(|e| e.to_string())?;
    let collector = ExperienceCollector::new(store.clone());
    let reflection = ReflectionEngine::new(store.clone(), Arc::new(HeuristicReflector));

    let exp = collector
        .record(Experience {
            id: String::new(),
            timestamp_ms: 0,
            agent_id: "bench-agent".to_string(),
            project_id: None,
            task_category: "conversation".to_string(),
            user_request: "deploy the service".to_string(),
            outcome: ExperienceOutcome::Partial,
            tools_used: vec!["db.migrate".to_string()],
            errors: vec!["migration failed".to_string()],
            corrections: vec!["No, use the staging database instead of production.".to_string()],
            feedback: None,
            reusable: false,
            sensitivity: Sensitivity::Private,
            metadata: json!({}),
        })
        .map_err(|e| e.to_string())?;

    let proposals = reflection
        .run_reflection(&exp.id, true)
        .map_err(|e| e.to_string())?;
    let memory_props: Vec<_> = proposals
        .iter()
        .filter(|p| p.kind == ProposalKind::AddMemory)
        .collect();
    let applied = store
        .list_proposals(Some(ProposalStatus::Applied.as_str()))
        .map_err(|e| e.to_string())?;
    let applied_ok = memory_props
        .first()
        .is_some_and(|p| applied.iter().any(|a| a.id == p.id));

    let entries = reflection
        .memory()
        .recall(Some("agent:bench-agent"), "staging", 5)
        .map_err(|e| e.to_string())?;
    let evolution_hit = entries.iter().any(|m| m.value.contains("staging database"));

    // Runtime-side mapping: the same proposal the approval surface would stage,
    // written at `scope_of`'s scope and read back through `recall_terms`.
    let session = SessionId::new("ses-bench-correction");
    let mut runtime_ok = false;
    let mut mapped_ok = false;
    if let Some(prop) = memory_props.first()
        && let Some(mapped) = proposal_from_evolution(prop, &session)
    {
        mapped_ok = mapped.kind == "memory";
        let scope = scope_of(&mapped.target, &session);
        let mem = MemoryStore::open_in_memory().map_err(|e| e.to_string())?;
        mem.save(&mapped.key, &mapped.content, &scope)
            .map_err(|e| e.to_string())?;
        let hits = mem
            .recall_terms(&["staging".to_string()], &[scope], 5)
            .map_err(|e| e.to_string())?;
        runtime_ok = hits.iter().any(|(_, key, _)| key == &mapped.key);
    }

    let passed = memory_props.len() == 1 && applied_ok && evolution_hit && mapped_ok && runtime_ok;
    Ok(Report::new(
        "correction_pipeline",
        passed,
        vec![
            ("proposals", proposals.len() as f64),
            ("memory_proposals", memory_props.len() as f64),
            ("applied_proposals", applied.len() as f64),
            ("evolution_memory_hits", entries.len() as f64),
            ("runtime_mapped", f64::from(mapped_ok)),
            ("runtime_scope_recall", f64::from(runtime_ok)),
        ],
    ))
}

// ---------------------------------------------------------------- consolidation

/// Seeds duplicate, contradictory and unique observations into a file-backed
/// `ConsolidationStore`, runs `run_consolidation`, and checks the report's
/// dedup/supersession counts plus the superseded row's lineage pointer.
fn consolidation(dir: &Path) -> Result<Report, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let store =
        ConsolidationStore::open(dir.join("consolidation.db")).map_err(|e| e.to_string())?;

    for _ in 0..3 {
        store
            .capture("deploy-target", "staging", "project", "bench", 0.9, 0.8)
            .map_err(|e| e.to_string())?;
    }
    store
        .capture("db-backend", "postgres", "project", "bench", 0.9, 0.8)
        .map_err(|e| e.to_string())?;
    store
        .capture("db-backend", "mysql", "project", "bench", 0.4, 0.5)
        .map_err(|e| e.to_string())?;
    for i in 0..4 {
        store
            .capture(
                &format!("fact-{i}"),
                &format!("value {i}"),
                "project",
                "bench",
                0.7,
                0.6,
            )
            .map_err(|e| e.to_string())?;
    }

    let (report, rows) = store
        .run_consolidation(None, &ConsolidationPolicy::default())
        .map_err(|e| e.to_string())?;

    let superseded_rows = rows
        .iter()
        .filter(|o| o.status == ObservationStatus::Superseded)
        .count();
    let lineage_ok = rows
        .iter()
        .filter(|o| o.status == ObservationStatus::Superseded)
        .all(|o| o.superseded_by.is_some());
    let postgres_active = rows.iter().any(|o| {
        o.key == "db-backend" && o.value == "postgres" && o.status == ObservationStatus::Active
    });

    let passed = report.captured == 9
        && report.deduplicated == 2
        && report.contradictions_detected == 1
        && report.superseded == 1
        && report.active_after == 6
        && superseded_rows == 1
        && lineage_ok
        && postgres_active;
    Ok(Report::new(
        "consolidation",
        passed,
        vec![
            ("captured", report.captured as f64),
            ("deduplicated", report.deduplicated as f64),
            (
                "contradictions_detected",
                report.contradictions_detected as f64,
            ),
            ("superseded", report.superseded as f64),
            ("expired", report.expired as f64),
            ("active_after", report.active_after as f64),
            ("superseded_rows", superseded_rows as f64),
            ("lineage_ok", f64::from(lineage_ok)),
            ("winner_active", f64::from(postgres_active)),
        ],
    ))
}

// ---------------------------------------------------------------- baseline

/// Metrics baseline: `{version, created_ms, benchmarks: {name: {metric: value}}}`.
/// Only metric numbers are stored — pass/fail is recomputed every run, so a
/// baseline can never freeze a stale verdict.
pub fn save_baseline(path: &Path, reports: &[Report]) -> Result<(), String> {
    let mut benchmarks = serde_json::Map::new();
    for report in reports {
        let mut metrics = serde_json::Map::new();
        for (key, value) in &report.metrics {
            metrics.insert(key.clone(), json!(value));
        }
        benchmarks.insert(report.name.to_string(), Value::Object(metrics));
    }
    let doc = json!({
        "version": 1,
        "created_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "benchmarks": benchmarks,
    });
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    std::fs::write(path, format!("{text}\n")).map_err(|e| e.to_string())
}

type Baseline = BTreeMap<String, BTreeMap<String, f64>>;

fn load_baseline(path: &Path) -> Result<Baseline, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read baseline {}: {e}", path.display()))?;
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| format!("baseline {} is not valid JSON: {e}", path.display()))?;
    let benches = doc
        .get("benchmarks")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("baseline {} has no `benchmarks` object", path.display()))?;
    let mut out = Baseline::new();
    for (name, metrics) in benches {
        let obj = metrics.as_object().ok_or_else(|| {
            format!(
                "baseline {name} metrics are not an object in {}",
                path.display()
            )
        })?;
        let mut row = BTreeMap::new();
        for (key, value) in obj {
            let number = value.as_f64().ok_or_else(|| {
                format!(
                    "baseline metric {name}.{key} is not a number in {}",
                    path.display()
                )
            })?;
            row.insert(key.clone(), number);
        }
        out.insert(name.clone(), row);
    }
    Ok(out)
}

/// One printed comparison row.
struct DeltaRow {
    benchmark: String,
    metric: String,
    baseline: Option<f64>,
    current: f64,
}

fn compare_baseline(reports: &[Report], baseline: &Baseline) -> (Vec<DeltaRow>, Vec<String>) {
    let mut rows = Vec::new();
    let mut notes = Vec::new();
    for report in reports {
        let base_metrics = baseline.get(report.name);
        for (metric, value) in &report.metrics {
            let base = base_metrics.and_then(|m| m.get(metric)).copied();
            if base.is_none() {
                notes.push(format!(
                    "{}.{metric}: no baseline value (new metric or benchmark)",
                    report.name
                ));
            }
            rows.push(DeltaRow {
                benchmark: report.name.to_string(),
                metric: metric.clone(),
                baseline: base,
                current: *value,
            });
        }
        if let Some(base_metrics) = base_metrics {
            for metric in base_metrics.keys() {
                if !report.metrics.iter().any(|(k, _)| k == metric) {
                    notes.push(format!(
                        "{}.{}: in baseline but not in this run",
                        report.name, metric
                    ));
                }
            }
        } else {
            notes.push(format!("{}: no baseline entry", report.name));
        }
    }
    for name in baseline.keys() {
        if !reports.iter().any(|r| r.name == name) {
            notes.push(format!("{name}: in baseline but not in this run"));
        }
    }
    (rows, notes)
}

// ---------------------------------------------------------------- output + CLI

fn fmt_value(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}

fn fmt_delta(d: f64) -> String {
    if d == 0.0 {
        "+0".to_string()
    } else {
        format!("{d:+.3}")
    }
}

fn print_reports(reports: &[Report], deltas: Option<&[DeltaRow]>, notes: &[String]) {
    println!("learning benchmarks — real store state, deterministic, offline");
    println!(
        "{:<22} | {:<6} | {:<26} | {:>10} | {:>10} | {:>10}",
        "benchmark", "result", "metric", "value", "baseline", "delta"
    );
    println!(
        "{}-+-{}-+-{}-+-{}-+-{}-+-{}",
        "-".repeat(22),
        "-".repeat(6),
        "-".repeat(26),
        "-".repeat(10),
        "-".repeat(10),
        "-".repeat(10)
    );
    for report in reports {
        for (index, (metric, value)) in report.metrics.iter().enumerate() {
            let row = deltas.and_then(|rows| {
                rows.iter()
                    .find(|r| r.benchmark == report.name && r.metric == *metric)
            });
            println!(
                "{:<22} | {:<6} | {:<26} | {:>10} | {:>10} | {:>10}",
                if index == 0 { report.name } else { "" },
                if index == 0 {
                    if report.passed { "PASS" } else { "FAIL" }
                } else {
                    ""
                },
                metric,
                fmt_value(*value),
                row.and_then(|r| r.baseline)
                    .map(fmt_value)
                    .unwrap_or_else(|| "-".into()),
                row.and_then(|r| r.baseline.map(|b| fmt_delta(r.current - b)))
                    .unwrap_or_else(|| "-".into())
            );
        }
    }
    for note in notes {
        println!("note: {note}");
    }
}

/// `vikram-bench learning [--baseline PATH] [--save-baseline PATH]`.
pub fn cli(args: &[String]) -> Result<(), String> {
    let mut baseline: Option<PathBuf> = None;
    let mut save: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--baseline" => {
                i += 1;
                baseline = Some(PathBuf::from(
                    args.get(i).ok_or("--baseline needs a path")?.clone(),
                ));
            }
            "--save-baseline" => {
                i += 1;
                save = Some(PathBuf::from(
                    args.get(i).ok_or("--save-baseline needs a path")?.clone(),
                ));
            }
            "-h" | "--help" => {
                println!("vikram-bench learning [--baseline PATH] [--save-baseline PATH]");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
        i += 1;
    }
    learning(baseline.as_deref(), save.as_deref())
}

/// Runs the suite, prints the table, optionally compares against and writes a
/// baseline. Fails if any benchmark failed or the baseline could not be read.
pub fn learning(baseline: Option<&Path>, save: Option<&Path>) -> Result<(), String> {
    // A corrupt baseline is refused before any work: comparing against garbage
    // would print deltas that mean nothing.
    let loaded = baseline.map(load_baseline).transpose()?;
    let reports = run_all()?;
    let (rows, notes) = match &loaded {
        Some(base) => compare_baseline(&reports, base),
        None => (Vec::new(), Vec::new()),
    };
    print_reports(&reports, loaded.as_ref().map(|_| rows.as_slice()), &notes);
    if let Some(path) = save {
        save_baseline(path, &reports)?;
        println!("baseline saved to {}", path.display());
    }
    let failed = reports.iter().filter(|r| !r.passed).count();
    if failed > 0 {
        return Err(format!("{failed} learning benchmark(s) failed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_learning_benchmark_passes_on_clean_temp_state() {
        let dir = tempfile::tempdir().unwrap();
        let reports = vec![
            memory_recall(&dir.path().join("memory")).unwrap(),
            experience_recall(&dir.path().join("experience")).unwrap(),
            skill_lifecycle(&dir.path().join("skill")).unwrap(),
            correction_pipeline(&dir.path().join("correction")).unwrap(),
            consolidation(&dir.path().join("consolidation")).unwrap(),
        ];
        for report in &reports {
            assert!(
                report.passed,
                "{} failed: {:?}",
                report.name, report.metrics
            );
        }
        assert_eq!(run_all().unwrap().len(), 5);
    }

    #[test]
    fn baseline_save_and_compare_roundtrip() {
        let reports = run_all().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        save_baseline(&path, &reports).unwrap();
        let loaded = load_baseline(&path).unwrap();
        let (rows, notes) = compare_baseline(&reports, &loaded);
        assert!(notes.is_empty(), "unexpected notes: {notes:?}");
        assert_eq!(
            rows.len(),
            reports.iter().map(|r| r.metrics.len()).sum::<usize>()
        );
        for row in &rows {
            assert_eq!(row.baseline, Some(row.current), "{}", row.metric);
        }
    }

    #[test]
    fn corrupt_baselines_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let bad_json = dir.path().join("bad.json");
        std::fs::write(&bad_json, "{not json").unwrap();
        assert!(load_baseline(&bad_json).is_err());

        let no_benchmarks = dir.path().join("empty.json");
        std::fs::write(&no_benchmarks, "{}").unwrap();
        assert!(load_baseline(&no_benchmarks).is_err());

        let bad_metric = dir.path().join("metric.json");
        std::fs::write(
            &bad_metric,
            r#"{"benchmarks":{"memory_recall":{"hit_rate":"high"}}}"#,
        )
        .unwrap();
        assert!(load_baseline(&bad_metric).is_err());

        assert!(load_baseline(&dir.path().join("missing.json")).is_err());
    }

    #[test]
    fn learning_cli_reports_usage_errors() {
        assert!(cli(&["--bogus".to_string()]).is_err());
        assert!(cli(&["--baseline".to_string()]).is_err());
    }
}
