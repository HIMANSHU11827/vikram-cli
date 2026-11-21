//! `vikram evolution` — inspection and operator-gated control of the
//! self-evolution engine.
//!
//! The engine's durable state lives in `~/.vikram/evolution.db` (SQLite) with
//! evolution-tracked skills under `~/.vikram/skills`. This command opens that
//! store directly — no runtime, no daemon. Most subcommands only read:
//! counts, listings, and the store-backed analyses the self-* engines already
//! implement. Four subcommands mutate durable state — `cycle`, `benchmark`,
//! `approve`, `reject` — and each requires `--yes` or an interactive
//! confirmation before writing. The conversational handlers answer from
//! the real store; self-recovery persists state fingerprints and reports
//! drift. Proposal review and approval stay in `vikram learning`.

use std::path::Path;

use clap::Subcommand;
use vikram_evolution::{
    EvaluationBenchmark, EvolutionEngine, ExperienceOutcome, ProposalStatus,
    autonomy::ApprovalStatus, metrics::DEFAULT_WINDOW_MS, now_ms,
};

#[derive(Debug, Subcommand)]
pub enum EvolutionAction {
    /// Engine health: store counts, pending proposals, last activity.
    Status,
    /// List recent experiences.
    Experiences {
        /// How many to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only experiences recorded by this agent id.
        #[arg(long)]
        agent: Option<String>,
    },
    /// List reflection proposals, optionally filtered by status.
    Proposals {
        /// pending | approved | rejected | applied.
        #[arg(long)]
        status: Option<String>,
        /// How many to show.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// List layered memory entries.
    Memory {
        /// Only entries in this scope (e.g. `global`, `agent:<id>`).
        #[arg(long)]
        scope: Option<String>,
        /// Include superseded and expired entries (hidden by default).
        #[arg(long)]
        all: bool,
    },
    /// List evolution-tracked skill versions.
    Skills {
        /// Show the version history of one skill instead of the summary.
        name: Option<String>,
    },
    /// Analyze recorded evidence and report improvement opportunities.
    Evaluate,
    /// Run one MAPE-K monitoring pass and report subsystem health.
    Mapek,
    /// Natural-language command handled by the conversational engine.
    Ask {
        /// Free text, e.g. `"analyze yourself"` or `"what can you improve"`.
        text: String,
    },
    /// List pending autonomy approval requests.
    Approvals,
    /// Report self-recovery state and run the restart-recovery pass.
    Recover,
    /// Run one MAPE-K cycle: monitor → analyze → plan → execute.
    ///
    /// Mutates the store: bumps the durable cycle count, records decisions and
    /// outcomes in the knowledge base, re-queues failed reflection jobs it can
    /// remediate, and persists `record_issue` proposals for what it cannot.
    Cycle {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Run a self-evaluation benchmark, or list available benchmarks.
    ///
    /// A run records an `EvaluationResult` in the store; listing is read-only.
    Benchmark {
        /// Benchmark id or name. Lists benchmarks when omitted.
        name: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Approve a pending autonomy request (durable decision).
    Approve {
        /// Approval request id (see `vikram evolution approvals`).
        id: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Reject a pending autonomy request (durable decision).
    Reject {
        /// Approval request id (see `vikram evolution approvals`).
        id: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

pub fn run(action: Option<EvolutionAction>) -> Result<(), String> {
    let action = action.unwrap_or(EvolutionAction::Status);
    let home = vikram_runtime::config::vikram_home();
    run_at(action, &home.join("evolution.db"), &home.join("skills"))
}

fn run_at(action: EvolutionAction, db: &Path, skills_root: &Path) -> Result<(), String> {
    if !db.exists() {
        return Err(format!(
            "no evolution store at {} — the engine has not recorded anything yet",
            db.display()
        ));
    }
    let mut engine = open_engine(db, skills_root)?;
    for line in dispatch(&mut engine, &action)? {
        println!("{line}");
    }
    Ok(())
}

fn open_engine(db: &Path, skills_root: &Path) -> Result<EvolutionEngine, String> {
    EvolutionEngine::open(db.to_path_buf(), skills_root.to_path_buf()).map_err(|e| e.to_string())
}

fn dispatch(engine: &mut EvolutionEngine, action: &EvolutionAction) -> Result<Vec<String>, String> {
    match action {
        EvolutionAction::Status => status_lines(engine),
        EvolutionAction::Experiences { limit, agent } => {
            experiences_lines(engine, *limit, agent.as_deref())
        }
        EvolutionAction::Proposals { status, limit } => {
            proposals_lines(engine, status.as_deref(), *limit)
        }
        EvolutionAction::Memory { scope, all } => memory_lines(engine, scope.as_deref(), *all),
        EvolutionAction::Skills { name } => skills_lines(engine, name.as_deref()),
        EvolutionAction::Evaluate => evaluate_lines(engine),
        EvolutionAction::Mapek => mapek_lines(engine),
        EvolutionAction::Ask { text } => ask_lines(engine, text),
        EvolutionAction::Approvals => approvals_lines(engine),
        EvolutionAction::Recover => recover_lines(engine),
        EvolutionAction::Cycle { yes } => cycle_lines(engine, *yes),
        EvolutionAction::Benchmark { name, yes } => benchmark_lines(engine, name.as_deref(), *yes),
        EvolutionAction::Approve { id, yes } => approval_decision_lines(engine, id, true, *yes),
        EvolutionAction::Reject { id, yes } => approval_decision_lines(engine, id, false, *yes),
    }
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.replace('\n', " ");
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn status_lines(engine: &EvolutionEngine) -> Result<Vec<String>, String> {
    let store = &engine.store;
    let dash = engine.dashboard.snapshot().map_err(|e| e.to_string())?;
    let exp = store
        .experience_stats(vikram_evolution::metrics::DEFAULT_WINDOW_MS)
        .map_err(|e| e.to_string())?;
    let props = store.proposal_stats().map_err(|e| e.to_string())?;
    let refl = store.reflection_stats().map_err(|e| e.to_string())?;
    let footprint = store.store_footprint_bytes().map_err(|e| e.to_string())?;
    let (first, last) = store.experience_time_bounds().map_err(|e| e.to_string())?;
    let now = now_ms();

    let mut out = vec![
        format!("store: evolution.db ({:.1} KiB)", footprint as f64 / 1024.0),
        format!(
            "experiences: {} total ({} in last {}h window)",
            exp.all_time.total,
            exp.recent.total,
            DEFAULT_WINDOW_MS / 3_600_000
        ),
    ];
    if let Some(sample) = exp.authoritative_sample() {
        out.push(format!(
            "  outcomes: {} success, {} failure, {} partial, {} with errors, {} with corrections",
            sample.success,
            sample.failure,
            sample.partial,
            sample.with_errors,
            sample.with_corrections
        ));
        if let Some(rate) = sample.failure_rate() {
            out.push(format!("  failure rate: {:.0}%", rate * 100.0));
        }
    } else {
        out.push("  outcomes: no sample — fewer than 5 experiences recorded".to_string());
    }
    match (first, last) {
        (Some(_), Some(ts)) => {
            out.push(format!("  last experience: {}", crate::format_age(now, ts)))
        }
        _ => out.push("  last experience: none recorded".to_string()),
    }
    out.push(format!("memories: {} live entries", dash.memories));
    out.push(format!(
        "proposals: {} total — {} pending, {} approved, {} rejected, {} applied",
        props.total, props.pending, props.approved, props.rejected, props.applied
    ));
    if props.pending_record_issue > 0 {
        out.push(format!(
            "  {} pending record_issue proposal(s) awaiting review",
            props.pending_record_issue
        ));
    }
    out.push(format!(
        "reflection jobs: {} total — {} pending, {} running, {} done, {} failed",
        refl.total, refl.pending, refl.running, refl.done, refl.failed
    ));
    out.push(format!(
        "learning events: {}, optimization candidates: {}, hive lessons: {}",
        dash.learning_events, dash.candidates, dash.hive_lessons
    ));
    Ok(out)
}

fn experiences_lines(
    engine: &EvolutionEngine,
    limit: usize,
    agent: Option<&str>,
) -> Result<Vec<String>, String> {
    let exps = engine
        .experience
        .list(agent, limit)
        .map_err(|e| e.to_string())?;
    if exps.is_empty() {
        return Ok(vec![match agent {
            Some(a) => format!("no experiences recorded for agent `{a}`"),
            None => "no experiences recorded".to_string(),
        }]);
    }
    let now = now_ms();
    let mut out = Vec::with_capacity(exps.len());
    for e in &exps {
        let outcome = match e.outcome {
            ExperienceOutcome::Success => "success",
            ExperienceOutcome::Failure => "failure",
            ExperienceOutcome::Partial => "partial",
        };
        let mut line = format!(
            "{}  {} ago  {}  {}  {}",
            e.id,
            crate::format_age(now, e.timestamp_ms),
            e.agent_id,
            e.task_category,
            outcome
        );
        if !e.user_request.is_empty() {
            line.push_str(&format!("  {}", truncate(&e.user_request, 80)));
        }
        out.push(line);
    }
    Ok(out)
}

fn proposals_lines(
    engine: &EvolutionEngine,
    status: Option<&str>,
    limit: usize,
) -> Result<Vec<String>, String> {
    let status_filter = match status {
        Some(s) => {
            if ProposalStatus::parse(s).is_none() {
                return Err(format!(
                    "unknown proposal status `{s}` (pending | approved | rejected | applied)"
                ));
            }
            Some(s.to_string())
        }
        None => None,
    };
    let props = engine
        .store
        .list_proposals(status_filter.as_deref())
        .map_err(|e| e.to_string())?;
    if props.is_empty() {
        return Ok(vec![match status {
            Some(s) => format!("no {s} proposals"),
            None => "no proposals recorded".to_string(),
        }]);
    }
    let now = now_ms();
    let mut out = Vec::new();
    for p in props.iter().take(limit) {
        let approval = if p.requires_approval {
            " [needs approval]"
        } else {
            ""
        };
        out.push(format!(
            "{}  {}  {}  {}  conf {:.2}{}  {}",
            p.id,
            p.status.as_str(),
            p.kind.as_str(),
            crate::format_age(now, p.created_ms),
            p.confidence,
            approval,
            truncate(&p.summary, 90)
        ));
    }
    if props.len() > limit {
        out.push(format!("… {} more (use --limit)", props.len() - limit));
    }
    Ok(out)
}

fn memory_lines(
    engine: &EvolutionEngine,
    scope: Option<&str>,
    all: bool,
) -> Result<Vec<String>, String> {
    let entries = if all {
        engine.memory.list_all(scope)
    } else {
        engine.memory.list(scope)
    }
    .map_err(|e| e.to_string())?;
    if entries.is_empty() {
        let live = if all { "" } else { "live " };
        return Ok(vec![match scope {
            Some(s) => format!("no {live}memory entries in scope `{s}`"),
            None => format!("no {live}memory entries"),
        }]);
    }
    let now = now_ms();
    let mut out = Vec::with_capacity(entries.len());
    for m in &entries {
        let flags = if m.is_superseded() {
            " [superseded]"
        } else if m.is_expired(now) {
            " [expired]"
        } else {
            ""
        };
        out.push(format!(
            "{}  {}/{}  {}  {}  conf {:.2}{}  {}",
            m.id,
            m.layer.as_str(),
            m.scope,
            m.key,
            m.status.as_str(),
            m.confidence,
            flags,
            truncate(&m.value, 80)
        ));
    }
    Ok(out)
}

fn skills_lines(engine: &EvolutionEngine, name: Option<&str>) -> Result<Vec<String>, String> {
    match name {
        Some(name) => {
            let versions = engine
                .skills
                .list_versions(name)
                .map_err(|e| e.to_string())?;
            if versions.is_empty() {
                return Ok(vec![format!("no recorded versions for skill `{name}`")]);
            }
            let mut out = Vec::with_capacity(versions.len());
            for v in &versions {
                let parent = v
                    .parent_version
                    .map(|p| format!("v{p}"))
                    .unwrap_or_else(|| "-".to_string());
                out.push(format!(
                    "v{}  parent={}  {}  {}",
                    v.version,
                    parent,
                    crate::format_age(now_ms(), v.created_ms),
                    truncate(&v.change_description, 90)
                ));
            }
            Ok(out)
        }
        None => {
            let names = engine.skills.discover();
            if names.is_empty() {
                return Ok(vec![format!(
                    "no evolution-tracked skills under {}",
                    engine.skills.skills_root().display()
                )]);
            }
            let mut out = vec![format!(
                "skills root: {}",
                engine.skills.skills_root().display()
            )];
            for name in names {
                let versions = engine
                    .skills
                    .list_versions(&name)
                    .map_err(|e| e.to_string())?;
                match versions.last() {
                    Some(latest) => out.push(format!(
                        "{name}  v{} ({} version{})  {}",
                        latest.version,
                        versions.len(),
                        if versions.len() == 1 { "" } else { "s" },
                        truncate(&latest.change_description, 70)
                    )),
                    None => out.push(format!("{name}  (no recorded versions)")),
                }
            }
            Ok(out)
        }
    }
}

fn evaluate_lines(engine: &mut EvolutionEngine) -> Result<Vec<String>, String> {
    let opportunities = engine
        .self_improvement
        .identify_improvement_opportunities()
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(note) = engine.self_improvement.last_opportunity_note() {
        out.push(format!("scan: {note}"));
    }
    if opportunities.is_empty() {
        out.push("no improvement opportunities".to_string());
        return Ok(out);
    }
    out.push(format!(
        "{} improvement opportunit(y/ies):",
        opportunities.len()
    ));
    for o in &opportunities {
        out.push(format!(
            "  p{}  {:?}  expected value {:.2}  {}",
            o.priority, o.area, o.expected_value, o.description
        ));
        for evidence in &o.evidence {
            out.push(format!("    - {evidence}"));
        }
    }
    Ok(out)
}

fn mapek_lines(engine: &mut EvolutionEngine) -> Result<Vec<String>, String> {
    // `monitor` only reads the store and probes the host; it persists nothing.
    let data = engine.mapek.monitor().map_err(|e| e.to_string())?;
    let mut out = vec![format!(
        "overall: {:?} ({} recorded outcome(s), {} failure record(s))",
        data.runtime_health.overall_status,
        data.execution_outcomes.len(),
        data.failures.len()
    )];
    for s in &data.runtime_health.subsystems {
        out.push(format!("  {}: {:?} — {}", s.name, s.status, s.details));
    }
    out.push(format!(
        "provider: {} ({}% error rate over {} sample(s), {} ms mean latency)",
        if data.provider_status.available {
            "available"
        } else {
            "unavailable"
        },
        data.provider_status.error_rate * 100.0,
        data.provider_status.sample_count,
        data.provider_status.latency_ms
    ));
    for note in &data.provider_status.notes {
        out.push(format!("  note: {note}"));
    }
    for note in &data.resource_metrics.notes {
        out.push(format!("  resources: {note}"));
    }
    Ok(out)
}

fn ask_lines(engine: &mut EvolutionEngine, text: &str) -> Result<Vec<String>, String> {
    let Some(command) = engine.conversational.parse_command(text) else {
        return Err(format!(
            "unrecognized command `{text}` — the conversational engine only understands phrases like \
             \"analyze yourself\", \"what can you improve\", \"find your weaknesses\", \
             \"what have you improved recently\""
        ));
    };
    let response = engine
        .conversational
        .handle_command(command)
        .map_err(|e| e.to_string())?;
    let mut out = vec![
        "note: conversational handlers answer from the real store; commands \
         with no honest implementation return success:false with the reason"
            .to_string(),
        format!("success: {}", response.success),
        response.message.clone(),
    ];
    if let Some(data) = &response.data {
        out.push(format!(
            "data: {}",
            serde_json::to_string(data).unwrap_or_else(|_| "<unserializable>".to_string())
        ));
    }
    Ok(out)
}

fn approvals_lines(engine: &EvolutionEngine) -> Result<Vec<String>, String> {
    let all = engine.autonomy.get_pending_approvals();
    let pending: Vec<_> = all
        .iter()
        .filter(|r| r.status == ApprovalStatus::Pending)
        .collect();
    let resolved = all.len() - pending.len();
    let mut out = vec![
        "note: autonomy approvals are durable — requests and decisions persist in \
         engine_records and survive restarts; an approval authorizes exactly one \
         controlled execution"
            .to_string(),
    ];
    if pending.is_empty() {
        out.push("no pending approval requests".to_string());
    } else {
        for r in &pending {
            out.push(format!(
                "{}  {:?}  {:?}  {}  {}",
                r.id,
                r.risk_level,
                r.action_type,
                crate::format_age(now_ms(), r.timestamp_ms),
                truncate(&r.description, 90)
            ));
        }
    }
    if resolved > 0 {
        out.push(format!(
            "{resolved} request(s) already decided (approved/rejected/executed)"
        ));
    }
    Ok(out)
}

fn recover_lines(engine: &mut EvolutionEngine) -> Result<Vec<String>, String> {
    let mut out = vec![
        "note: self-recovery snapshots and state fingerprints persist in \
         engine_records; drift is detected against the live store"
            .to_string(),
    ];
    let operations = engine
        .self_recovery
        .recover_after_restart()
        .map_err(|e| e.to_string())?;
    out.push(format!(
        "restart recovery pass: {} operation(s) queued",
        operations.len()
    ));
    for op in &operations {
        out.push(format!(
            "  {}  {:?}  target={}  {}",
            op.id,
            op.recovery_type,
            op.target,
            truncate(&op.reason, 70)
        ));
    }
    out.push(format!(
        "state: {} snapshot(s), {} failed change(s), {} recovery operation(s)",
        engine.self_recovery.get_snapshots().len(),
        engine.self_recovery.get_failed_changes().len(),
        engine.self_recovery.get_recovery_history().len()
    ));
    Ok(out)
}

fn confirm_or_abort(yes: bool, question: &str) -> Result<(), String> {
    if yes {
        Ok(())
    } else {
        crate::dist::confirm(question)
    }
}

fn cycle_lines(engine: &mut EvolutionEngine, yes: bool) -> Result<Vec<String>, String> {
    // monitor/analyze are documented pure reads; running them first lets the
    // report name the issues the cycle is about to act on. Nothing is written
    // here — the only mutation is `run_cycle` below.
    let data = engine.mapek.monitor().map_err(|e| e.to_string())?;
    let analysis = engine.mapek.analyze(&data).map_err(|e| e.to_string())?;

    confirm_or_abort(
        yes,
        "run one MAPE-K cycle? This writes to the evolution store \
         (knowledge base, cycle count, proposals, events). [y/N] ",
    )?;

    let result = engine.mapek.run_cycle().map_err(|e| e.to_string())?;
    let mut out = vec![
        format!(
            "pre-cycle analysis: {} issue(s), {} opportunit(y/ies), {} anomal(y/ies)",
            analysis.issues.len(),
            analysis.opportunities.len(),
            analysis.anomalies.len()
        ),
        format!(
            "cycle {} complete (plan {})",
            result.cycle_number, result.plan_id
        ),
    ];
    for issue in &analysis.issues {
        out.push(format!(
            "  issue: {:?} on `{}` — {}",
            issue.severity,
            issue.component,
            truncate(&issue.description, 90)
        ));
    }
    out.push(format!(
        "actions: {} planned, {} remediated, {} recorded for operator review",
        result.planned_actions, result.remediated_actions, result.recorded_for_review
    ));
    out.push(format!(
        "execution: {}",
        if result.execution_success {
            "every planned action remediated".to_string()
        } else {
            format!(
                "not fully remediated — {}",
                result
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| "no reason recorded".to_string())
            )
        }
    ));
    out.push(
        "state: cycle count and knowledge base persisted; unremediated actions leave \
         pending record_issue proposals and a mapek_execution_incomplete event"
            .to_string(),
    );
    Ok(out)
}

/// `engine_records` namespace the self-evaluation engine persists benchmarks
/// under (mirrors `self_evaluation::BENCHMARK_NAMESPACE`, which is private).
const BENCHMARK_NAMESPACE: &str = "self_evaluation.benchmark";

fn list_benchmarks(engine: &EvolutionEngine) -> Result<Vec<EvaluationBenchmark>, String> {
    engine
        .store
        .list_engine_records(BENCHMARK_NAMESPACE)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(_, payload)| {
            serde_json::from_value::<EvaluationBenchmark>(payload)
                .map_err(|e| format!("corrupt benchmark record in store: {e}"))
        })
        .collect()
}

fn resolve_benchmark<'a>(
    benchmarks: &'a [EvaluationBenchmark],
    name: &str,
) -> Result<&'a EvaluationBenchmark, String> {
    if let Some(b) = benchmarks.iter().find(|b| b.id == name) {
        return Ok(b);
    }
    let by_name: Vec<_> = benchmarks.iter().filter(|b| b.name == name).collect();
    match by_name.as_slice() {
        [b] => Ok(b),
        [] => Err(format!(
            "no benchmark with id or name `{name}` ({} recorded)",
            benchmarks.len()
        )),
        _ => Err(format!("benchmark name `{name}` is ambiguous")),
    }
}

fn benchmark_lines(
    engine: &mut EvolutionEngine,
    name: Option<&str>,
    yes: bool,
) -> Result<Vec<String>, String> {
    let benchmarks = list_benchmarks(engine)?;
    let Some(name) = name else {
        if benchmarks.is_empty() {
            return Ok(vec!["no evaluation benchmarks recorded".to_string()]);
        }
        let mut out = vec![format!("{} benchmark(s):", benchmarks.len())];
        for b in &benchmarks {
            out.push(format!(
                "  {}  {}  task_type={}  {}",
                b.id,
                b.name,
                b.task_type,
                truncate(&b.description, 70)
            ));
        }
        out.push("run one with `vikram evolution benchmark <id-or-name> --yes`".to_string());
        return Ok(out);
    };

    let benchmark = resolve_benchmark(&benchmarks, name)?;
    confirm_or_abort(
        yes,
        &format!(
            "run benchmark `{}`? This records an evaluation result in the store. [y/N] ",
            benchmark.name
        ),
    )?;

    let result = engine
        .self_evaluation
        .run_benchmark(&benchmark.id)
        .map_err(|e| e.to_string())?;
    let summary = engine
        .self_evaluation
        .aggregate_results()
        .map_err(|e| e.to_string())?;
    let out = vec![
        format!("benchmark: {} ({})", benchmark.name, benchmark.id),
        format!(
            "result: {}  score {:.2}  data_points={}  {}",
            if result.passed { "passed" } else { "failed" },
            result.score,
            result.data_points,
            if result.sufficient_data {
                "sufficient data"
            } else {
                "insufficient data"
            }
        ),
        format!("notes: {}", result.notes),
        format!(
            "state: {} evaluation(s) recorded — {} passed, {} failed, avg score {:.2}",
            summary.total_evaluations,
            summary.passed_evaluations,
            summary.failed_evaluations,
            summary.average_score
        ),
    ];
    Ok(out)
}

fn approval_decision_lines(
    engine: &mut EvolutionEngine,
    id: &str,
    approve: bool,
    yes: bool,
) -> Result<Vec<String>, String> {
    let request = engine
        .autonomy
        .get_pending_approvals()
        .iter()
        .find(|r| r.id == id)
        .cloned()
        .ok_or_else(|| format!("approval request `{id}` not found"))?;

    let verb = if approve { "approve" } else { "reject" };
    confirm_or_abort(
        yes,
        &format!(
            "{verb} request {} ({:?} {:?}) — \"{}\"? This decision is durable. [y/N] ",
            request.id, request.risk_level, request.action_type, request.description
        ),
    )?;

    if approve {
        let approver = std::env::var("USER").unwrap_or_else(|_| "operator".to_string());
        engine
            .autonomy
            .approve_request(id, &approver)
            .map_err(|e| e.to_string())?;
    } else {
        engine
            .autonomy
            .reject_request(id)
            .map_err(|e| e.to_string())?;
    }

    let status = engine
        .autonomy
        .get_pending_approvals()
        .iter()
        .find(|r| r.id == id)
        .map(|r| format!("{:?}", r.status))
        .unwrap_or_else(|| "unknown".to_string());
    Ok(vec![
        format!(
            "request {} {}: {:?} {:?} — {}",
            request.id,
            verb,
            request.risk_level,
            request.action_type,
            truncate(&request.description, 90)
        ),
        format!("reason: {}", truncate(&request.reason, 90)),
        format!("state: request {id} is now {status}"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_evolution::types::{
        Experience, KnowledgeStatus, LearningProposal, MemoryLayer, ProposalKind, Sensitivity,
    };

    fn engine_at(tmp: &tempfile::TempDir) -> EvolutionEngine {
        let db = tmp.path().join("evolution.db");
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        EvolutionEngine::open(db, skills).unwrap()
    }

    fn seed_experience(engine: &EvolutionEngine, outcome: ExperienceOutcome, category: &str) {
        engine
            .experience
            .record(Experience {
                id: String::new(),
                timestamp_ms: 0,
                agent_id: "agent-1".to_string(),
                project_id: None,
                task_category: category.to_string(),
                user_request: "do the thing".to_string(),
                outcome,
                tools_used: vec![],
                errors: if outcome == ExperienceOutcome::Failure {
                    vec!["boom".to_string()]
                } else {
                    vec![]
                },
                corrections: vec![],
                feedback: None,
                reusable: false,
                sensitivity: Sensitivity::Private,
                metadata: serde_json::json!({}),
            })
            .unwrap();
    }

    fn seed_proposal(engine: &EvolutionEngine, status: ProposalStatus, kind: ProposalKind) {
        engine
            .store
            .save_proposal(&LearningProposal {
                id: format!("prop-{}", kind.as_str()),
                experience_id: None,
                agent_id: "agent-1".to_string(),
                kind,
                summary: "a proposal".to_string(),
                details: serde_json::json!({}),
                confidence: 0.75,
                requires_approval: kind == ProposalKind::CreateSkill,
                created_ms: now_ms(),
                status,
            })
            .unwrap();
    }

    #[test]
    fn empty_store_reports_honest_empty_states() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        for (action, expected) in [
            (EvolutionAction::Status, "experiences: 0 total"),
            (
                EvolutionAction::Experiences {
                    limit: 5,
                    agent: None,
                },
                "no experiences",
            ),
            (
                EvolutionAction::Proposals {
                    status: None,
                    limit: 5,
                },
                "no proposals",
            ),
            (
                EvolutionAction::Memory {
                    scope: None,
                    all: false,
                },
                "no live memory",
            ),
            (
                EvolutionAction::Skills { name: None },
                "no evolution-tracked skills",
            ),
        ] {
            let lines = dispatch(&mut engine, &action).unwrap();
            assert!(
                lines.iter().any(|l| l.contains(expected)),
                "{action:?} missing `{expected}` in {lines:?}"
            );
        }
        let eval = dispatch(&mut engine, &EvolutionAction::Evaluate).unwrap();
        assert!(eval.iter().any(|l| l.contains("scan:")), "{eval:?}");
        let mapek = dispatch(&mut engine, &EvolutionAction::Mapek).unwrap();
        assert!(mapek.iter().any(|l| l.starts_with("overall:")), "{mapek:?}");
    }

    #[test]
    fn experiences_lists_seeded_rows_and_filters_by_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        seed_experience(&engine, ExperienceOutcome::Success, "chat");
        seed_experience(&engine, ExperienceOutcome::Failure, "tools");

        let lines = dispatch(
            &mut engine,
            &EvolutionAction::Experiences {
                limit: 10,
                agent: None,
            },
        )
        .unwrap();
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("agent-1") && l.contains("failure"))
        );
        assert!(lines.iter().any(|l| l.contains("success")));

        let none = dispatch(
            &mut engine,
            &EvolutionAction::Experiences {
                limit: 10,
                agent: Some("nobody".into()),
            },
        )
        .unwrap();
        assert_eq!(
            none,
            vec!["no experiences recorded for agent `nobody`".to_string()]
        );
    }

    #[test]
    fn proposals_filters_by_status() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        seed_proposal(&engine, ProposalStatus::Pending, ProposalKind::RecordIssue);
        seed_proposal(&engine, ProposalStatus::Applied, ProposalKind::AddMemory);

        let all = dispatch(
            &mut engine,
            &EvolutionAction::Proposals {
                status: None,
                limit: 50,
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2);

        let pending = dispatch(
            &mut engine,
            &EvolutionAction::Proposals {
                status: Some("pending".into()),
                limit: 50,
            },
        )
        .unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].contains("record_issue"));

        let err = dispatch(
            &mut engine,
            &EvolutionAction::Proposals {
                status: Some("bogus".into()),
                limit: 50,
            },
        )
        .unwrap_err();
        assert!(err.contains("unknown proposal status"), "{err}");
    }

    #[test]
    fn memory_lists_live_entries_and_all_includes_superseded() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        engine
            .memory
            .remember(
                MemoryLayer::Semantic,
                "global",
                "pref",
                "likes rust",
                KnowledgeStatus::Confirmed,
                0.9,
                None,
                "test",
            )
            .unwrap();
        // A stronger claim supersedes the first entry.
        engine
            .memory
            .remember(
                MemoryLayer::Semantic,
                "global",
                "pref",
                "loves rust",
                KnowledgeStatus::Confirmed,
                0.99,
                None,
                "test",
            )
            .unwrap();

        let live = dispatch(
            &mut engine,
            &EvolutionAction::Memory {
                scope: None,
                all: false,
            },
        )
        .unwrap();
        assert_eq!(live.len(), 1);
        assert!(live[0].contains("loves rust"));

        let all = dispatch(
            &mut engine,
            &EvolutionAction::Memory {
                scope: None,
                all: true,
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|l| l.contains("[superseded]")));

        let scoped = dispatch(
            &mut engine,
            &EvolutionAction::Memory {
                scope: Some("agent:x".into()),
                all: false,
            },
        )
        .unwrap();
        assert!(scoped[0].contains("no live memory entries"), "{scoped:?}");
    }

    #[test]
    fn skills_lists_versions_and_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        engine
            .skills
            .create_skill("demo-skill", "# demo\nbody", "initial")
            .unwrap();
        engine
            .skills
            .improve_skill("demo-skill", "# demo\nbetter", "tweak")
            .unwrap();

        let summary = dispatch(&mut engine, &EvolutionAction::Skills { name: None }).unwrap();
        assert!(
            summary
                .iter()
                .any(|l| l.contains("demo-skill") && l.contains("v2"))
        );

        let history = dispatch(
            &mut engine,
            &EvolutionAction::Skills {
                name: Some("demo-skill".into()),
            },
        )
        .unwrap();
        assert_eq!(history.len(), 2);
        assert!(history[1].contains("tweak"));

        let missing = dispatch(
            &mut engine,
            &EvolutionAction::Skills {
                name: Some("ghost".into()),
            },
        )
        .unwrap();
        assert_eq!(
            missing,
            vec!["no recorded versions for skill `ghost`".to_string()]
        );
    }

    #[test]
    fn evaluate_reports_opportunity_from_failure_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        seed_experience(&engine, ExperienceOutcome::Failure, "tools");
        let lines = dispatch(&mut engine, &EvolutionAction::Evaluate).unwrap();
        assert!(lines.iter().any(|l| l.contains("opportunit")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("tools")), "{lines:?}");
    }

    #[test]
    fn missing_db_is_an_honest_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run_at(
            EvolutionAction::Status,
            &tmp.path().join("nope").join("evolution.db"),
            &tmp.path().join("skills"),
        )
        .unwrap_err();
        assert!(err.contains("no evolution store"), "{err}");
    }

    #[test]
    fn ask_parses_known_phrases_and_rejects_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);

        let lines = dispatch(
            &mut engine,
            &EvolutionAction::Ask {
                text: "analyze yourself".into(),
            },
        )
        .unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("real store") && l.contains("success:false")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("success: true")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Self-analysis complete")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("\"runtime_health\"")),
            "{lines:?}"
        );

        let err = dispatch(
            &mut engine,
            &EvolutionAction::Ask {
                text: "do my laundry".into(),
            },
        )
        .unwrap_err();
        assert!(err.contains("unrecognized command"), "{err}");
        assert!(err.contains("analyze yourself"), "{err}");
    }

    #[test]
    fn ask_covers_every_parseable_phrase() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        for text in [
            "analyze yourself",
            "self-analysis",
            "what can you improve",
            "find your weaknesses",
            "improve your memory",
            "optimize your model",
            "review your agent",
            "learn coding workflow",
            "what have you improved",
            "why did you change the timeout",
            "undo last change",
            "stop autonomous improvements",
            "resume autonomous management",
            "explain action act-1",
            "report improvements",
        ] {
            assert!(
                engine.conversational.parse_command(text).is_some(),
                "phrase not parsed: {text}"
            );
            let lines = dispatch(&mut engine, &EvolutionAction::Ask { text: text.into() }).unwrap();
            assert!(
                lines.iter().any(|l| l.starts_with("success:")),
                "{text}: {lines:?}"
            );
        }
        assert!(
            engine.conversational.parse_command("status").is_none(),
            "`status` is not a conversational phrase — it is a subcommand"
        );
    }

    #[test]
    fn approvals_reports_durable_empty_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        let lines = dispatch(&mut engine, &EvolutionAction::Approvals).unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("durable") && l.contains("survive restarts")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "no pending approval requests"),
            "{lines:?}"
        );
    }

    fn seed_approval(engine: &mut EvolutionEngine) -> String {
        engine
            .autonomy
            .request_approval(
                vikram_evolution::ActionType::ChangeSecurityPolicy,
                "loosen the egress policy",
                "operator asked",
            )
            .unwrap()
            .id
    }

    #[test]
    fn cycle_runs_once_and_reports_durable_result() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);

        let lines = dispatch(&mut engine, &EvolutionAction::Cycle { yes: true }).unwrap();
        assert!(
            lines.iter().any(|l| l.contains("cycle 1 complete")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("pre-cycle analysis") && l.contains("issue")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("knowledge base persisted")),
            "{lines:?}"
        );

        // The cycle count is durable: a second run continues at 2, and a
        // reopened engine sees the same state.
        let lines = dispatch(&mut engine, &EvolutionAction::Cycle { yes: true }).unwrap();
        assert!(
            lines.iter().any(|l| l.contains("cycle 2 complete")),
            "{lines:?}"
        );
        drop(engine);
        let mut reopened =
            EvolutionEngine::open(tmp.path().join("evolution.db"), tmp.path().join("skills"))
                .unwrap();
        let lines = dispatch(&mut reopened, &EvolutionAction::Cycle { yes: true }).unwrap();
        assert!(
            lines.iter().any(|l| l.contains("cycle 3 complete")),
            "{lines:?}"
        );
    }

    #[test]
    fn cycle_without_yes_refuses_on_non_terminal_stdin() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        let err = dispatch(&mut engine, &EvolutionAction::Cycle { yes: false }).unwrap_err();
        assert!(err.contains("--yes"), "{err}");
    }

    #[test]
    fn benchmark_lists_then_runs_and_reports_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);

        let empty = dispatch(
            &mut engine,
            &EvolutionAction::Benchmark {
                name: None,
                yes: true,
            },
        )
        .unwrap();
        assert_eq!(empty, vec!["no evaluation benchmarks recorded".to_string()]);

        let benchmark = engine
            .self_evaluation
            .create_benchmark(
                "chat-quality",
                "chat task outcomes",
                "chat",
                vikram_evolution::PerformanceMetrics {
                    task_completion_rate: 0.9,
                    task_correctness: 0.9,
                    tool_success_rate: 0.9,
                    model_performance: 0.0,
                    agent_performance: 0.0,
                    execution_time_ms: 60_000,
                    token_consumption: 100_000,
                    resource_consumption: 0.0,
                    failure_rate: 0.0,
                    recovery_rate: 0.0,
                    regression_rate: 0.0,
                    user_corrections: 0,
                    skill_effectiveness: 0.0,
                },
            )
            .unwrap();

        let listed = dispatch(
            &mut engine,
            &EvolutionAction::Benchmark {
                name: None,
                yes: true,
            },
        )
        .unwrap();
        assert!(
            listed
                .iter()
                .any(|l| l.contains(&benchmark.id) && l.contains("chat-quality")),
            "{listed:?}"
        );

        let ran = dispatch(
            &mut engine,
            &EvolutionAction::Benchmark {
                name: Some("chat-quality".into()),
                yes: true,
            },
        )
        .unwrap();
        assert!(
            ran.iter()
                .any(|l| l.contains("result:") && l.contains("data_points=0")),
            "{ran:?}"
        );
        assert!(
            ran.iter().any(|l| l.contains("1 evaluation(s) recorded")),
            "{ran:?}"
        );

        // Resolution also accepts the benchmark id.
        let by_id = dispatch(
            &mut engine,
            &EvolutionAction::Benchmark {
                name: Some(benchmark.id.clone()),
                yes: true,
            },
        )
        .unwrap();
        assert!(
            by_id.iter().any(|l| l.contains("2 evaluation(s)")),
            "{by_id:?}"
        );

        let missing = dispatch(
            &mut engine,
            &EvolutionAction::Benchmark {
                name: Some("ghost".into()),
                yes: true,
            },
        )
        .unwrap_err();
        assert!(missing.contains("no benchmark"), "{missing}");
    }

    #[test]
    fn benchmark_run_without_yes_refuses_on_non_terminal_stdin() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        engine
            .self_evaluation
            .create_benchmark(
                "b",
                "d",
                "chat",
                vikram_evolution::PerformanceMetrics {
                    task_completion_rate: 0.0,
                    task_correctness: 0.0,
                    tool_success_rate: 0.0,
                    model_performance: 0.0,
                    agent_performance: 0.0,
                    execution_time_ms: 0,
                    token_consumption: 0,
                    resource_consumption: 0.0,
                    failure_rate: 0.0,
                    recovery_rate: 0.0,
                    regression_rate: 0.0,
                    user_corrections: 0,
                    skill_effectiveness: 0.0,
                },
            )
            .unwrap();
        let err = dispatch(
            &mut engine,
            &EvolutionAction::Benchmark {
                name: Some("b".into()),
                yes: false,
            },
        )
        .unwrap_err();
        assert!(err.contains("--yes"), "{err}");
    }

    #[test]
    fn approve_and_reject_decide_durably() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        let approved_id = seed_approval(&mut engine);
        let rejected_id = seed_approval(&mut engine);

        let lines = dispatch(
            &mut engine,
            &EvolutionAction::Approve {
                id: approved_id.clone(),
                yes: true,
            },
        )
        .unwrap();
        assert!(
            lines.iter().any(|l| l.contains("now Approved")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("ChangeSecurityPolicy")),
            "{lines:?}"
        );

        let lines = dispatch(
            &mut engine,
            &EvolutionAction::Reject {
                id: rejected_id.clone(),
                yes: true,
            },
        )
        .unwrap();
        assert!(
            lines.iter().any(|l| l.contains("now Rejected")),
            "{lines:?}"
        );

        // Decisions are durable: a reopened engine still sees them resolved.
        drop(engine);
        let mut reopened =
            EvolutionEngine::open(tmp.path().join("evolution.db"), tmp.path().join("skills"))
                .unwrap();
        let approvals = dispatch(&mut reopened, &EvolutionAction::Approvals).unwrap();
        assert!(
            approvals
                .iter()
                .any(|l| l.contains("2 request(s) already decided")),
            "{approvals:?}"
        );
        assert!(
            approvals
                .iter()
                .any(|l| l == "no pending approval requests"),
            "{approvals:?}"
        );
        // A decided request cannot be decided again.
        let err = dispatch(
            &mut reopened,
            &EvolutionAction::Approve {
                id: approved_id,
                yes: true,
            },
        )
        .unwrap_err();
        assert!(err.contains("already"), "{err}");
    }

    #[test]
    fn approval_decision_unknown_id_and_missing_yes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);

        let err = dispatch(
            &mut engine,
            &EvolutionAction::Approve {
                id: "approval-nope".into(),
                yes: true,
            },
        )
        .unwrap_err();
        assert!(err.contains("not found"), "{err}");

        let id = seed_approval(&mut engine);
        let err = dispatch(
            &mut engine,
            &EvolutionAction::Reject {
                id: id.clone(),
                yes: false,
            },
        )
        .unwrap_err();
        assert!(err.contains("--yes"), "{err}");
        // The refused decision changed nothing.
        let approvals = dispatch(&mut engine, &EvolutionAction::Approvals).unwrap();
        assert!(approvals.iter().any(|l| l.contains(&id)), "{approvals:?}");
    }

    #[test]
    fn recover_reports_process_local_state_and_empty_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_at(&tmp);
        let lines = dispatch(&mut engine, &EvolutionAction::Recover).unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("fingerprints") && l.contains("drift")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("0 operation(s) queued")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("0 snapshot(s)") && l.contains("0 failed change(s)")),
            "{lines:?}"
        );
    }
}
