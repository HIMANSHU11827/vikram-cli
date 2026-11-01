//! The execution loop. One architecture for chat, coding, research and background work: operations
//! run in registration order over a shared [`RunContext`], each proposing effects, and the kernel
//! is the only thing that may act on them.
//!
//! Three properties carry the rest of the design:
//! - **Resumable.** Every effect is journalled before it is applied, and an effect id already
//!   present means the action is skipped rather than repeated.
//! - **Auditable.** The journal is the record of what happened and what policy said, not the logs.
//! - **Safe by construction.** Permission is evaluated in the kernel against the trust of the
//!   content that produced the request; an operation has no path to the grant set.

use vikram_contracts::policy::PermissionPolicy;
use vikram_contracts::trust::Trust;
use vikram_contracts::{
    Appended, Decision, EffectId, JournalSink, Message, NewRecord, PermissionMode,
    PermissionRequest, Posture, RecordKind, SessionId, ToolCall, TurnId, VikramError,
};

use crate::effects::{Effect, EffectHandler, Handled};
use crate::grants::{GrantSet, GrantSnapshot};
use crate::run::{CancellationToken, Clock, TokenMeter};

pub enum Outcome {
    /// Nothing to do on this pass; the loop advances without side effects.
    NotApplicable,
    Applied(Vec<Effect>),
}

/// A step of work. Operations are the extension seam for everything above the kernel: planning,
/// context assembly, provider calls, verification and memory consolidation are each one.
pub trait Operation {
    fn name(&self) -> &'static str;
    fn run(&mut self, ctx: &mut RunContext) -> Result<Outcome, VikramError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCall {
    pub call: ToolCall,
    pub request: PermissionRequest,
}

pub struct RunContext {
    pub session: SessionId,
    pub turn: TurnId,
    pub messages: Vec<Message>,
    pub pending: Vec<PendingCall>,
    /// Read-only. There is no setter, and the live [`GrantSet`] is not reachable from here.
    pub grants: GrantSnapshot,
    pub meter: TokenMeter,
    pub state: serde_json::Value,
    pub cancel: CancellationToken,
    /// Set by an operation to end the turn cleanly.
    pub finished: bool,
    /// The request a human must answer before the turn may continue.
    pub awaiting: Option<PermissionRequest>,
    /// What this run does with a request the rules only prompted for. `Ask` — nothing — is the
    /// default, because a run that wants a looser posture has to say so.
    pub mode: PermissionMode,
}

impl RunContext {
    pub fn new(session: SessionId, turn: TurnId, grants: GrantSnapshot) -> Self {
        Self {
            session,
            turn,
            messages: Vec::new(),
            pending: Vec::new(),
            grants,
            meter: TokenMeter::default(),
            state: serde_json::Value::Null,
            cancel: CancellationToken::new(),
            finished: false,
            awaiting: None,
            mode: PermissionMode::Ask,
        }
    }

    pub fn push_text(&mut self, message: Message) {
        self.meter.add_input(vikram_estimate(&message));
        self.messages.push(message);
    }
}

fn vikram_estimate(message: &Message) -> u64 {
    TokenMeter::estimate(&message.text_body())
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_steps: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_steps: 64 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunReport {
    pub steps: u32,
    pub effects_applied: usize,
    pub effects_skipped: usize,
    pub effects_denied: usize,
    pub effects_deferred: usize,
    pub converged: bool,
}

pub struct Machine<'a> {
    ops: Vec<Box<dyn Operation>>,
    journal: &'a dyn JournalSink,
    policy: &'a dyn PermissionPolicy,
    handler: &'a mut dyn EffectHandler,
    grants: &'a mut GrantSet,
    clock: &'a dyn Clock,
    limits: Limits,
}

impl<'a> Machine<'a> {
    pub fn new(
        ops: Vec<Box<dyn Operation>>,
        journal: &'a dyn JournalSink,
        policy: &'a dyn PermissionPolicy,
        handler: &'a mut dyn EffectHandler,
        grants: &'a mut GrantSet,
        clock: &'a dyn Clock,
        limits: Limits,
    ) -> Self {
        Self {
            ops,
            journal,
            policy,
            handler,
            grants,
            clock,
            limits,
        }
    }

    pub fn run(&mut self, ctx: &mut RunContext) -> Result<RunReport, VikramError> {
        let mut report = RunReport::default();

        loop {
            ctx.grants = self.grants.snapshot();

            if ctx.cancel.is_cancelled() {
                self.finalize_cancellation(ctx)?;
                return Err(VikramError::Cancelled);
            }

            report.steps += 1;
            if report.steps > self.limits.max_steps {
                return Err(VikramError::BudgetExhausted(self.limits.max_steps));
            }

            let mut progressed = false;
            let op_count = self.ops.len();
            for idx in 0..op_count {
                if ctx.cancel.is_cancelled() {
                    self.finalize_cancellation(ctx)?;
                    return Err(VikramError::Cancelled);
                }
                let outcome = self.ops[idx].run(ctx)?;
                match outcome {
                    Outcome::NotApplicable => {}
                    Outcome::Applied(effects) => {
                        progressed = true;
                        for effect in &effects {
                            self.dispatch(effect, ctx, &mut report)?;
                        }
                    }
                }
            }

            if ctx.cancel.is_cancelled() {
                self.finalize_cancellation(ctx)?;
                return Err(VikramError::Cancelled);
            }

            if ctx.finished {
                report.converged = true;
                return Ok(report);
            }
            if !progressed {
                report.converged = true;
                return Ok(report);
            }
        }
    }

    /// Journal first, then act. If the process dies between the two, the replay finds the record
    /// and skips the action instead of performing it twice.
    fn dispatch(
        &mut self,
        effect: &Effect,
        ctx: &mut RunContext,
        report: &mut RunReport,
    ) -> Result<(), VikramError> {
        let mut record = NewRecord::new(
            ctx.session.clone(),
            Self::record_kind(effect),
            serde_json::to_value(effect)
                .map_err(|e| VikramError::InvalidState(format!("effect not serialisable: {e}")))?,
        )
        .at(self.clock.now_ms());

        if let Some(effect_id) = effect.effect_id() {
            record = record.with_effect(effect_id.clone());
        }

        match self.journal.append(&record)? {
            Appended::AlreadyApplied(_seq) => {
                report.effects_skipped += 1;
                return Ok(());
            }
            Appended::Written(_) => {}
        }

        if let Some(request) = effect.permission_request(Trust::Model) {
            let raw = self
                .policy
                .evaluate_at_depth(&request, ctx.grants.delegation_depth());
            // The posture is applied here, by the kernel, not by the evaluator: a replaced policy
            // cannot loosen `plan`, `allow-list`, or the prompting of `ask` by answering differently.
            let decision = ctx.mode.resolve(
                raw.clone(),
                &request,
                Posture {
                    workspace_root: self.policy.workspace_root().as_deref(),
                    listed: self.policy.is_specially_allowed(&request),
                },
            );
            self.record_decision(ctx, &request, &decision)?;

            let covered = ctx.grants.permits(&request);
            match decision {
                Decision::Allow if covered => {}
                Decision::Allow => {
                    let reason = format!(
                        "`{}` is permitted by policy but not by this agent's grants",
                        request.verb
                    );
                    self.record_denial(ctx, effect, &reason)?;
                    report.effects_denied += 1;
                    return Ok(());
                }
                Decision::Deny { reason } => {
                    self.record_denial(ctx, effect, &reason)?;
                    report.effects_denied += 1;
                    return Ok(());
                }
                Decision::RequireApproval { reason, amendment } => {
                    ctx.pending.push(PendingCall {
                        call: tool_call_of(effect),
                        request: request.clone(),
                    });
                    ctx.awaiting = Some(request.clone());
                    self.record_approval(ctx, &request, &reason, amendment.as_deref())?;
                    report.effects_deferred += 1;
                    return Ok(());
                }
            }
        }

        if let Effect::RequestTool { call, .. } = effect {
            ctx.pending.push(PendingCall {
                call: call.clone(),
                request: effect
                    .permission_request(Trust::Model)
                    .expect("tool effects always describe their own request"),
            });
        }

        match self.handler.apply(effect)? {
            Handled::Applied(payload) => {
                if let Effect::RequestTool { call, .. } = effect {
                    let result = Message::tool_result(call.id.clone(), payload.to_string(), true);
                    self.append(
                        ctx,
                        RecordKind::ToolResult,
                        serde_json::to_value(&result)
                            .map_err(|e| VikramError::InvalidState(e.to_string()))?,
                        None,
                    )?;
                    ctx.messages.push(result);
                    ctx.pending.retain(|p| p.call.id != call.id);
                }
                report.effects_applied += 1;
            }
            Handled::Deferred => {
                report.effects_deferred += 1;
            }
        }
        Ok(())
    }

    fn finalize_cancellation(&mut self, ctx: &mut RunContext) -> Result<(), VikramError> {
        self.append(
            ctx,
            RecordKind::Cancellation,
            serde_json::json!({ "pending": ctx.pending.len() }),
            None,
        )?;
        // A dangling tool call with no result is what makes a resumed session unreadable, so every
        // cancelled turn synthesises one.
        for pending in std::mem::take(&mut ctx.pending) {
            let result = Message::tool_result(
                pending.call.id.clone(),
                "cancelled before completion",
                false,
            );
            self.append(
                ctx,
                RecordKind::ToolResult,
                serde_json::to_value(&result)
                    .map_err(|e| VikramError::InvalidState(e.to_string()))?,
                None,
            )?;
            ctx.messages.push(result);
        }
        Ok(())
    }

    fn record_decision(
        &mut self,
        ctx: &mut RunContext,
        request: &PermissionRequest,
        decision: &Decision,
    ) -> Result<(), VikramError> {
        self.append(
            ctx,
            RecordKind::Policy,
            serde_json::json!({
                "verb": request.verb,
                "action": request.action,
                "resource": request.resource,
                "trust": request.originating_trust,
                "decision": decision,
                "mode": ctx.mode.as_str(),
            }),
            None,
        )
    }

    fn record_denial(
        &mut self,
        ctx: &mut RunContext,
        effect: &Effect,
        reason: &str,
    ) -> Result<(), VikramError> {
        let call = tool_call_of(effect);
        let message = Message::tool_result(call.id.clone(), format!("denied: {reason}"), false);
        self.append(
            ctx,
            RecordKind::ToolResult,
            serde_json::to_value(&message).map_err(|e| VikramError::InvalidState(e.to_string()))?,
            None,
        )?;
        ctx.messages.push(message);
        Ok(())
    }

    fn record_approval(
        &mut self,
        ctx: &mut RunContext,
        request: &PermissionRequest,
        reason: &str,
        amendment: Option<&str>,
    ) -> Result<(), VikramError> {
        self.append(
            ctx,
            RecordKind::Approval,
            serde_json::json!({
                "verb": request.verb,
                "resource": request.resource,
                "reason": reason,
                "amendment": amendment,
                "state": "pending",
            }),
            None,
        )
    }

    fn append(
        &mut self,
        ctx: &mut RunContext,
        kind: RecordKind,
        payload: serde_json::Value,
        effect: Option<EffectId>,
    ) -> Result<(), VikramError> {
        let record = NewRecord {
            session: ctx.session.clone(),
            ts_ms: self.clock.now_ms(),
            kind,
            effect,
            payload,
        };
        self.journal.append(&record)?;
        Ok(())
    }

    fn record_kind(effect: &Effect) -> RecordKind {
        match effect {
            Effect::EmitMessage(_) => RecordKind::Message,
            Effect::RequestTool { .. } => RecordKind::ToolCall,
            Effect::StreamDelta { .. } => RecordKind::Message,
            Effect::Status { .. } => RecordKind::Status,
            Effect::Activity { .. } => RecordKind::Activity,
            Effect::Compaction { .. } => RecordKind::Compaction,
        }
    }
}

fn tool_call_of(effect: &Effect) -> ToolCall {
    match effect {
        Effect::RequestTool { call, .. } => call.clone(),
        _ => ToolCall::new("kernel.noop", serde_json::Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::Grant;
    use vikram_contracts::{
        ActionKind, ContractDecl, ContractDecl as Decl, InMemoryJournal, PermissionMode,
        SideEffect, Versioned, version::EFFECT_HANDLER,
    };
    use vikram_policy::{MandatoryFloor, Policy, ResourcePattern, Rule};

    struct Fixed;
    impl Clock for Fixed {
        fn now_ms(&self) -> u64 {
            1_700_000_000_000
        }
    }

    #[derive(Default)]
    struct CountingHandler {
        calls: Vec<String>,
        defer_next: bool,
    }

    impl Versioned for CountingHandler {
        fn contract(&self) -> ContractDecl {
            ContractDecl::new(EFFECT_HANDLER, 1, 0)
        }
    }

    impl EffectHandler for CountingHandler {
        fn apply(&mut self, effect: &Effect) -> Result<Handled, VikramError> {
            if self.defer_next {
                self.defer_next = false;
                return Ok(Handled::Deferred);
            }
            self.calls.push(effect_name(effect));
            Ok(Handled::Applied(
                serde_json::json!({ "ran": effect_name(effect) }),
            ))
        }
    }

    fn effect_kind_name(effect: &Effect) -> &'static str {
        match effect {
            Effect::EmitMessage(_) => "emit_message",
            Effect::RequestTool { .. } => "request_tool",
            Effect::StreamDelta { .. } => "stream_delta",
            Effect::Status { .. } => "status",
            Effect::Activity { .. } => "activity",
            Effect::Compaction { .. } => "compaction",
        }
    }

    fn effect_name(effect: &Effect) -> String {
        match effect {
            Effect::RequestTool { call, .. } => call.name.clone(),
            other => effect_kind_name(other).to_string(),
        }
    }

    struct EmitOnce {
        fired: bool,
        effect: Effect,
    }

    impl Operation for EmitOnce {
        fn name(&self) -> &'static str {
            "emit_once"
        }
        fn run(&mut self, _ctx: &mut RunContext) -> Result<Outcome, VikramError> {
            if self.fired {
                return Ok(Outcome::NotApplicable);
            }
            self.fired = true;
            Ok(Outcome::Applied(vec![self.effect.clone()]))
        }
    }

    struct AlwaysBusy;
    impl Operation for AlwaysBusy {
        fn name(&self) -> &'static str {
            "always_busy"
        }
        fn run(&mut self, _ctx: &mut RunContext) -> Result<Outcome, VikramError> {
            Ok(Outcome::Applied(vec![Effect::Status {
                status: "working".into(),
            }]))
        }
    }

    struct Finisher;
    impl Operation for Finisher {
        fn name(&self) -> &'static str {
            "finisher"
        }
        fn run(&mut self, ctx: &mut RunContext) -> Result<Outcome, VikramError> {
            ctx.finished = true;
            Ok(Outcome::NotApplicable)
        }
    }

    struct Canceller {
        token: CancellationToken,
    }
    impl Operation for Canceller {
        fn name(&self) -> &'static str {
            "canceller"
        }
        fn run(&mut self, ctx: &mut RunContext) -> Result<Outcome, VikramError> {
            self.token.cancel();
            ctx.cancel = self.token.clone();
            Ok(Outcome::NotApplicable)
        }
    }

    struct Setter {
        token: CancellationToken,
    }
    impl Operation for Setter {
        fn name(&self) -> &'static str {
            "setter"
        }
        fn run(&mut self, ctx: &mut RunContext) -> Result<Outcome, VikramError> {
            ctx.cancel = self.token.clone();
            ctx.pending.push(PendingCall {
                call: ToolCall::new("shell.run", serde_json::json!({"cmd": "sleep 100"})),
                request: PermissionRequest::new(
                    ActionKind::Execute,
                    "shell.run",
                    "*",
                    SideEffect::LocalMutation,
                    Trust::Model,
                ),
            });
            Ok(Outcome::NotApplicable)
        }
    }

    fn write_effect(resource: &str) -> Effect {
        Effect::RequestTool {
            call: ToolCall::new("fs.write", serde_json::json!({"path": resource})),
            effect_id: EffectId::generate(),
            action: ActionKind::Write,
            resource: resource.into(),
            side_effect: SideEffect::LocalMutation,
        }
    }

    fn permissive_policy() -> Policy {
        Policy::new(
            vec![Rule::allow(ActionKind::Write, ResourcePattern::Any)],
            MandatoryFloor::default(),
        )
    }

    fn context(grants: GrantSet) -> RunContext {
        RunContext::new(SessionId::generate(), TurnId::generate(), grants.snapshot())
    }

    /// Run one write through the loop under a posture, and report what the kernel decided and what
    /// the handler actually executed.
    fn dispatch(policy: &Policy, mode: PermissionMode, resource: &str) -> (RunReport, Vec<String>) {
        let journal = InMemoryJournal::new();
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(vec![Grant::new(ActionKind::Write, "*")], 0);
        let mut machine = Machine::new(
            vec![Box::new(EmitOnce {
                fired: false,
                effect: write_effect(resource),
            })],
            &journal,
            policy,
            &mut handler,
            &mut grants,
            &Fixed,
            Limits::default(),
        );
        let mut ctx = context(GrantSet::new(vec![Grant::new(ActionKind::Write, "*")], 0));
        ctx.mode = mode;
        let report = machine.run(&mut ctx).expect("the loop runs");
        drop(machine);
        (report, handler.calls)
    }

    #[test]
    fn a_posture_is_applied_by_the_kernel_not_by_the_evaluator() {
        // The evaluator allows this write; `plan` still refuses it, because the posture is applied
        // after the policy answers and a replaced policy cannot loosen it from the inside.
        let policy = permissive_policy();
        let (permissive, applied) = dispatch(&policy, PermissionMode::Ask, "/work/a.txt");
        assert_eq!(applied, vec!["fs.write"], "ask is not a refusal");
        assert_eq!(permissive.effects_denied, 0);

        let (report, calls) = dispatch(&policy, PermissionMode::Plan, "/work/a.txt");
        assert!(calls.is_empty(), "plan must not execute the write");
        assert_eq!(report.effects_denied, 1, "and must record why");
    }

    #[test]
    fn yolo_stops_prompting_without_reaching_past_a_floor_refusal() {
        let asks = Policy::new(
            vec![Rule::ask(
                ActionKind::Write,
                ResourcePattern::Any,
                "a human reads this",
            )],
            MandatoryFloor::default(),
        );
        let (report, calls) = dispatch(&asks, PermissionMode::Ask, "/work/a.txt");
        assert!(calls.is_empty());
        assert_eq!(report.effects_deferred, 1, "the request waits for a human");

        let (report, calls) = dispatch(&asks, PermissionMode::Yolo, "/work/a.txt");
        assert_eq!(calls, vec!["fs.write"], "yolo ends the prompt");
        assert_eq!(report.effects_deferred, 0);

        // A floor that refuses the path: there is nothing to prompt about, in any posture.
        let rooted = Policy::new(
            vec![Rule::ask(
                ActionKind::Write,
                ResourcePattern::Any,
                "a human reads this",
            )],
            MandatoryFloor {
                workspace_root: Some("/elsewhere".into()),
                ..MandatoryFloor::default()
            },
        );
        let (report, calls) = dispatch(&rooted, PermissionMode::Yolo, "/work/a.txt");
        assert!(
            calls.is_empty(),
            "a bypass posture is not a bypass of the floor"
        );
        assert_eq!(report.effects_denied, 1);

        // Inside the root an auto-approved edit needs no human; outside it, it does.
        let (report, calls) = dispatch(&rooted, PermissionMode::AcceptEdits, "/elsewhere/b.txt");
        assert_eq!(calls, vec!["fs.write"], "a write in the workspace is the point");
        assert_eq!(report.effects_deferred, 0);
        let (report, calls) = dispatch(&rooted, PermissionMode::AcceptEdits, "/work/a.txt");
        assert!(
            calls.is_empty(),
            "the floor refused it before any posture could approve it"
        );
        assert_eq!(report.effects_denied, 1);
    }

    #[test]
    fn unattended_refuses_a_prompt_and_says_do_not_retry() {
        let asks = Policy::new(
            vec![Rule::ask(
                ActionKind::Write,
                ResourcePattern::Any,
                "a human reads this",
            )],
            MandatoryFloor::default(),
        );
        let (report, calls) = dispatch(&asks, PermissionMode::Unattended, "/work/a.txt");
        assert!(calls.is_empty());
        assert_eq!(report.effects_deferred, 0, "nothing is left waiting");
        assert_eq!(report.effects_denied, 1);
    }

    #[test]
    fn the_loop_converges_when_no_operation_can_progress() {
        let journal = InMemoryJournal::new();
        let policy = permissive_policy();
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(vec![crate::grants::Grant::new(ActionKind::Write, "*")], 0);
        let clock = Fixed;
        let mut machine = Machine::new(
            vec![Box::new(EmitOnce {
                fired: false,
                effect: write_effect("/work/a.txt"),
            })],
            &journal,
            &policy,
            &mut handler,
            &mut grants,
            &clock,
            Limits::default(),
        );
        let mut ctx = context(GrantSet::new(
            vec![crate::grants::Grant::new(ActionKind::Write, "*")],
            0,
        ));

        let report = machine.run(&mut ctx).unwrap();
        assert!(report.converged);
        assert_eq!(report.effects_applied, 1);
        assert_eq!(handler.calls, vec!["fs.write".to_string()]);
        assert_eq!(report.steps, 2, "second pass finds nothing to do");
    }

    #[test]
    fn a_step_budget_stops_a_loop_that_never_settles() {
        let journal = InMemoryJournal::new();
        let policy = permissive_policy();
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(vec![], 0);
        let clock = Fixed;
        let mut machine = Machine::new(
            vec![Box::new(AlwaysBusy)],
            &journal,
            &policy,
            &mut handler,
            &mut grants,
            &clock,
            Limits { max_steps: 5 },
        );
        let mut ctx = context(GrantSet::new(vec![], 0));
        let err = machine.run(&mut ctx).unwrap_err();
        assert!(matches!(err, VikramError::BudgetExhausted(5)), "{err:?}");
    }

    #[test]
    fn an_operation_may_end_the_turn() {
        let journal = InMemoryJournal::new();
        let policy = permissive_policy();
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(vec![], 0);
        let clock = Fixed;
        let mut machine = Machine::new(
            vec![Box::new(AlwaysBusy), Box::new(Finisher)],
            &journal,
            &policy,
            &mut handler,
            &mut grants,
            &clock,
            Limits::default(),
        );
        let mut ctx = context(GrantSet::new(vec![], 0));
        let report = machine.run(&mut ctx).unwrap();
        assert!(report.converged);
        assert_eq!(report.steps, 1);
    }

    #[test]
    fn every_effect_is_journalled_before_the_handler_sees_it() {
        let journal = InMemoryJournal::new();
        let policy = permissive_policy();
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(vec![crate::grants::Grant::new(ActionKind::Write, "*")], 0);
        let clock = Fixed;
        let effect = write_effect("/work/a.txt");
        let effect_id = effect.effect_id().cloned().unwrap();
        let mut machine = Machine::new(
            vec![Box::new(EmitOnce {
                fired: false,
                effect: effect.clone(),
            })],
            &journal,
            &policy,
            &mut handler,
            &mut grants,
            &clock,
            Limits::default(),
        );
        let mut ctx = context(GrantSet::new(
            vec![crate::grants::Grant::new(ActionKind::Write, "*")],
            0,
        ));
        machine.run(&mut ctx).unwrap();

        let records = journal.read_after(&ctx.session, 0, 50).unwrap();
        let kinds: Vec<RecordKind> = records.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RecordKind::ToolCall,
                RecordKind::Policy,
                RecordKind::ToolResult
            ]
        );
        let call = records
            .iter()
            .find(|r| r.kind == RecordKind::ToolCall)
            .unwrap();
        assert_eq!(call.effect, Some(effect_id));
        assert_eq!(call.ts_ms, 1_700_000_000_000);
    }

    #[test]
    fn a_replayed_effect_is_skipped_instead_of_acting_twice() {
        let journal = InMemoryJournal::new();
        let policy = permissive_policy();
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(vec![crate::grants::Grant::new(ActionKind::Write, "*")], 0);
        let clock = Fixed;

        let mut effect = write_effect("/work/a.txt");
        let effect_id = effect.effect_id().cloned().unwrap();
        // Simulate the crash: the decision was journalled before the process died.
        journal
            .append(
                &NewRecord::new(
                    SessionId::new("pre-existing"),
                    RecordKind::ToolCall,
                    serde_json::json!({}),
                )
                .with_effect(EffectId::new("unrelated")),
            )
            .unwrap();
        let preexisting_session = match &mut effect {
            Effect::RequestTool { .. } => SessionId::generate(),
            _ => unreachable!(),
        };
        journal
            .append(
                &NewRecord::new(
                    preexisting_session.clone(),
                    RecordKind::ToolCall,
                    serde_json::json!({}),
                )
                .with_effect(effect_id.clone()),
            )
            .unwrap();

        let mut ctx = RunContext::new(preexisting_session, TurnId::generate(), grants.snapshot());
        let mut machine = Machine::new(
            vec![Box::new(EmitOnce {
                fired: false,
                effect,
            })],
            &journal,
            &policy,
            &mut handler,
            &mut grants,
            &clock,
            Limits::default(),
        );

        let report = machine.run(&mut ctx).unwrap();
        assert_eq!(report.effects_skipped, 1);
        assert_eq!(report.effects_applied, 0);
        assert!(
            handler.calls.is_empty(),
            "the outside action must not run a second time"
        );
    }

    #[test]
    fn policy_denies_a_model_authored_permission_grant_even_when_a_rule_allows_it() {
        let journal = InMemoryJournal::new();
        // A config that (wrongly) allows everything, to prove the floor still holds.
        let policy = Policy::new(
            vec![Rule::allow(ActionKind::Admin, ResourcePattern::Any)],
            MandatoryFloor::default(),
        );
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(vec![crate::grants::Grant::new(ActionKind::Admin, "*")], 0);
        let clock = Fixed;
        let effect = Effect::RequestTool {
            call: ToolCall::new("grant.self", serde_json::json!({"scope": "*"})),
            effect_id: EffectId::generate(),
            action: ActionKind::Admin,
            resource: "*".into(),
            side_effect: SideEffect::ExternalChange,
        };
        let mut machine = Machine::new(
            vec![Box::new(EmitOnce {
                fired: false,
                effect,
            })],
            &journal,
            &policy,
            &mut handler,
            &mut grants,
            &clock,
            Limits::default(),
        );
        let mut ctx = context(GrantSet::new(
            vec![crate::grants::Grant::new(ActionKind::Admin, "*")],
            0,
        ));
        let report = machine.run(&mut ctx).unwrap();

        assert_eq!(report.effects_denied, 1);
        assert!(
            handler.calls.is_empty(),
            "denied effects never reach the host"
        );
        let denial = ctx.messages.last().unwrap().text_body();
        assert!(denial.contains("denied"), "{denial}");
        assert!(
            denial.contains("model-sourced"),
            "the reason must name the trust that caused the refusal: {denial}"
        );
    }

    #[test]
    fn a_grant_narrower_than_policy_still_blocks_the_effect() {
        let journal = InMemoryJournal::new();
        let policy = permissive_policy();
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(
            vec![crate::grants::Grant::new(ActionKind::Write, "/other")],
            0,
        );
        let clock = Fixed;
        let mut ctx = context(GrantSet::new(
            vec![crate::grants::Grant::new(ActionKind::Write, "/other")],
            0,
        ));
        ctx.grants = grants.snapshot();
        let mut machine = Machine::new(
            vec![Box::new(EmitOnce {
                fired: false,
                effect: write_effect("/work/a.txt"),
            })],
            &journal,
            &policy,
            &mut handler,
            &mut grants,
            &clock,
            Limits::default(),
        );
        let report = machine.run(&mut ctx).unwrap();
        assert_eq!(report.effects_denied, 1);
        assert!(handler.calls.is_empty());
        assert!(
            ctx.messages
                .last()
                .unwrap()
                .text_body()
                .contains("not by this agent's grants")
        );
    }

    #[test]
    fn a_deferred_effect_parks_the_turn_on_an_approval_instead_of_running() {
        let journal = InMemoryJournal::new();
        let policy = Policy::new(
            vec![Rule::ask(
                ActionKind::Write,
                ResourcePattern::Subtree("/work".into()),
                "review this mutation",
            )],
            MandatoryFloor::default(),
        );
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(vec![crate::grants::Grant::new(ActionKind::Write, "*")], 0);
        let clock = Fixed;
        let mut ctx = context(GrantSet::new(
            vec![crate::grants::Grant::new(ActionKind::Write, "*")],
            0,
        ));
        let mut machine = Machine::new(
            vec![Box::new(EmitOnce {
                fired: false,
                effect: write_effect("/work/a.txt"),
            })],
            &journal,
            &policy,
            &mut handler,
            &mut grants,
            &clock,
            Limits::default(),
        );
        let report = machine.run(&mut ctx).unwrap();

        assert_eq!(report.effects_deferred, 1);
        assert_eq!(report.effects_applied, 0);
        assert!(handler.calls.is_empty());
        assert!(
            ctx.awaiting.is_some(),
            "the turn must expose what it is waiting on"
        );
        assert_eq!(ctx.pending.len(), 1, "the call stays pending for approval");
        let approvals = journal
            .read_after(&ctx.session, 0, 50)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == RecordKind::Approval)
            .collect::<Vec<_>>();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].payload["state"], "pending");
    }

    #[test]
    fn cancellation_synthesises_results_for_every_pending_call() {
        let journal = InMemoryJournal::new();
        let policy = permissive_policy();
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(vec![], 0);
        let clock = Fixed;
        let token = CancellationToken::new();
        let setter_token = token.clone();
        let cancel_op_token = token.clone();

        let mut ctx = context(GrantSet::new(vec![], 0));
        ctx.cancel = token.clone();
        {
            let mut machine = Machine::new(
                vec![
                    Box::new(Setter {
                        token: setter_token,
                    }),
                    Box::new(Canceller {
                        token: cancel_op_token,
                    }),
                ],
                &journal,
                &policy,
                &mut handler,
                &mut grants,
                &clock,
                Limits::default(),
            );
            let err = machine.run(&mut ctx).unwrap_err();
            assert!(matches!(err, VikramError::Cancelled), "{err:?}");
        }

        assert!(ctx.pending.is_empty(), "pending must be drained on cancel");
        let results = ctx
            .messages
            .iter()
            .filter(|m| m.text_body().contains("cancelled before completion"))
            .count();
        assert_eq!(results, 1, "one synthesised result per pending call");
        let kinds: Vec<RecordKind> = journal
            .read_after(&ctx.session, 0, 50)
            .unwrap()
            .into_iter()
            .map(|r| r.kind)
            .collect();
        assert!(kinds.contains(&RecordKind::Cancellation));
        assert!(kinds.contains(&RecordKind::ToolResult));
    }

    #[test]
    fn non_tool_effects_reach_the_handler_without_a_policy_check() {
        let journal = InMemoryJournal::new();
        let policy = Policy::empty();
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(vec![], 0);
        let clock = Fixed;
        let mut machine = Machine::new(
            vec![Box::new(EmitOnce {
                fired: false,
                effect: Effect::Status {
                    status: "indexing".into(),
                },
            })],
            &journal,
            &policy,
            &mut handler,
            &mut grants,
            &clock,
            Limits::default(),
        );
        let mut ctx = context(GrantSet::new(vec![], 0));
        let report = machine.run(&mut ctx).unwrap();
        assert_eq!(report.effects_applied, 1);
        assert_eq!(report.effects_denied, 0);
        assert_eq!(handler.calls, vec!["status".to_string()]);
    }

    #[test]
    fn the_widened_grant_set_survives_a_run_and_the_next_run_sees_it() {
        let journal = InMemoryJournal::new();
        let policy = permissive_policy();
        let mut handler = CountingHandler::default();
        let mut grants = GrantSet::new(vec![], 0);
        let clock = Fixed;

        let mut ctx = context(grants.clone());
        {
            let mut machine = Machine::new(
                vec![Box::new(EmitOnce {
                    fired: false,
                    effect: write_effect("/work/a.txt"),
                })],
                &journal,
                &policy,
                &mut handler,
                &mut grants,
                &clock,
                Limits::default(),
            );
            machine.run(&mut ctx).unwrap();
        }
        assert_eq!(
            handler.calls.len(),
            0,
            "no grant means the write is denied even though policy allows it"
        );

        // The host grants the scope after a human approves, then the next turn runs.
        let mut broker = crate::grants::ApprovalBroker::new();
        let request = PermissionRequest::new(
            ActionKind::Write,
            "fs.write",
            "/work",
            SideEffect::LocalMutation,
            Trust::User,
        );
        let ticket = broker
            .issue(request, crate::grants::ApprovalSource::User)
            .unwrap();
        let grant = ticket.grant_within(ActionKind::Write, "/work").unwrap();
        grants.widen(ticket, grant);

        let mut handler2 = CountingHandler::default();
        let mut ctx2 = context(grants.clone());
        let mut machine = Machine::new(
            vec![Box::new(EmitOnce {
                fired: false,
                effect: write_effect("/work/b.txt"),
            })],
            &journal,
            &policy,
            &mut handler2,
            &mut grants,
            &clock,
            Limits::default(),
        );
        machine.run(&mut ctx2).unwrap();
        assert_eq!(handler2.calls, vec!["fs.write".to_string()]);
    }

    #[test]
    fn an_operation_cannot_reach_the_live_grant_set() {
        // Compile-time guarantee: RunContext hands out a snapshot only.
        let grants = GrantSet::new(vec![], 0);
        let ctx = context(grants.clone());
        let _: GrantSnapshot = ctx.grants.clone();
        let _: Decl = ContractDecl::new(EFFECT_HANDLER, 1, 0);
        assert!(ctx.grants.grants().is_empty());
    }
}
