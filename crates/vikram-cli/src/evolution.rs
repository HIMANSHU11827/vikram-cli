//! `vikram evolution` — read-only inspection of the self-evolution engine.
//!
//! The engine's durable state lives in `~/.vikram/evolution.db` (SQLite) with
//! evolution-tracked skills under `~/.vikram/skills`. This command opens that
//! store directly — no runtime, no daemon — and only reads: counts, listings,
//! and the store-backed analyses the self-* engines already implement.
//! Approvals and every other mutation stay in `vikram learning`.

use std::path::Path;

use clap::Subcommand;
use vikram_evolution::{
    EvolutionEngine, ExperienceOutcome, ProposalStatus, metrics::DEFAULT_WINDOW_MS, now_ms,
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
}
