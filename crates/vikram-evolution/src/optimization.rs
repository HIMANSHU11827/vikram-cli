use crate::error::Result;
use crate::store::EvolutionStore;
use crate::types::{EvaluationResult, OptimizationCandidate, gen_id, now_ms};

pub struct OptimizationEngine {
    store: EvolutionStore,
}

impl OptimizationEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    pub fn propose_candidate(
        &self,
        target: &str,
        parent: Option<&str>,
        change_desc: &str,
        content: &str,
    ) -> Result<OptimizationCandidate> {
        crate::redact::scan_injection(content)?;
        let c = OptimizationCandidate {
            id: gen_id("cand"),
            target: target.to_string(),
            parent_version: parent.map(|s| s.to_string()),
            change_description: change_desc.to_string(),
            content: content.to_string(),
            created_ms: now_ms(),
            status: crate::types::CandidateStatus::Candidate,
            rejection_reason: None,
        };
        self.store.save_candidate(&c)?;
        Ok(c)
    }

    pub fn evaluate(
        &self,
        candidate_id: &str,
        dataset: &str,
        metrics: serde_json::Value,
        threshold: f64,
    ) -> Result<EvaluationResult> {
        // compute aggregate score from metrics if not provided
        let score = metrics
            .get("score")
            .and_then(|v| v.as_f64())
            .unwrap_or_else(|| {
                // default: weighted success+correctness
                let s = metrics
                    .get("success")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let c = metrics
                    .get("correctness")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                (s + c) / 2.0
            });
        let passed = score >= threshold;
        let er = EvaluationResult {
            id: gen_id("eval"),
            candidate_id: candidate_id.to_string(),
            dataset: dataset.to_string(),
            metrics,
            score,
            passed,
            notes: if passed {
                "passed threshold".to_string()
            } else {
                "below threshold".to_string()
            },
            created_ms: now_ms(),
        };
        self.store.save_evaluation(&er)?;
        Ok(er)
    }

    /// Pareto selection: keep candidates not dominated on (score, regression_rate).
    /// Here regression_rate = 1 - score of holdout; lower better. We approximate with score only + passed.
    pub fn pareto_frontier(&self, target: &str) -> Result<Vec<OptimizationCandidate>> {
        let cands = self.store.list_candidates(Some(target))?;
        // gather best eval per candidate
        let mut scored: Vec<(OptimizationCandidate, f64, bool)> = Vec::new();
        for c in cands {
            let evals = self.store.list_evaluations(&c.id)?;
            let best = evals.iter().map(|e| e.score).fold(f64::MIN, f64::max);
            let passed = evals.iter().any(|e| e.passed);
            scored.push((c, if evals.is_empty() { 0.0 } else { best }, passed));
        }
        // frontier: not dominated (higher score, passed)
        let mut frontier = Vec::new();
        for (c, s, p) in &scored {
            if !p {
                continue;
            }
            let dominated = scored
                .iter()
                .any(|(c2, s2, p2)| c2.id != c.id && *p2 && *s2 > *s + 1e-9);
            if !dominated {
                frontier.push(c.clone());
            }
        }
        if frontier.is_empty() {
            // fall back to best candidate
            let mut best: Option<&(OptimizationCandidate, f64, bool)> = None;
            for item in &scored {
                match best {
                    None => best = Some(item),
                    Some(b) if item.1 > b.1 => best = Some(item),
                    _ => {}
                }
            }
            if let Some((c, _, _)) = best {
                frontier.push(c.clone());
            }
        }
        Ok(frontier)
    }

    pub fn promote(&self, candidate_id: &str) -> Result<()> {
        let mut c = self
            .store
            .get_candidate(candidate_id)?
            .ok_or_else(|| crate::error::EvolutionError::NotFound(candidate_id.to_string()))?;
        c.status = crate::types::CandidateStatus::Promoted;
        c.rejection_reason = None;
        self.store.save_candidate(&c)
    }

    /// Reject a candidate, persisting the reason so the decision is
    /// auditable. Previously `reason` was explicitly discarded.
    pub fn reject(&self, candidate_id: &str, reason: &str) -> Result<()> {
        let mut c = self
            .store
            .get_candidate(candidate_id)?
            .ok_or_else(|| crate::error::EvolutionError::NotFound(candidate_id.to_string()))?;
        c.status = crate::types::CandidateStatus::Rejected;
        c.rejection_reason = if reason.trim().is_empty() {
            None
        } else {
            Some(crate::redact::sanitize_for_storage(reason)?)
        };
        self.store.save_candidate(&c)
    }

    /// Roll back a candidate.
    ///
    /// Marks the candidate `rolled_back` and, when it has a
    /// `parent_version`, restores the parent's content by recording a
    /// successor candidate whose content is the parent's content and
    /// whose status is `candidate` (the caller applies it). The rolled-
    /// back row itself keeps its own content for audit — previously the
    /// parent content was "restored" only in a comment.
    pub fn rollback(&self, candidate_id: &str) -> Result<Option<OptimizationCandidate>> {
        let mut c = self
            .store
            .get_candidate(candidate_id)?
            .ok_or_else(|| crate::error::EvolutionError::NotFound(candidate_id.to_string()))?;
        c.status = crate::types::CandidateStatus::RolledBack;
        self.store.save_candidate(&c)?;
        if let Some(parent_id) = c.parent_version.clone()
            && let Some(parent) = self.store.get_candidate(&parent_id)?
        {
            let restored = OptimizationCandidate {
                id: gen_id("cand"),
                target: c.target.clone(),
                parent_version: Some(c.id.clone()),
                change_description: format!("restore parent {} (rollback of {})", parent.id, c.id),
                content: parent.content.clone(),
                created_ms: now_ms(),
                status: crate::types::CandidateStatus::Candidate,
                rejection_reason: None,
            };
            self.store.save_candidate(&restored)?;
            return Ok(Some(restored));
        }
        Ok(None)
    }
}
