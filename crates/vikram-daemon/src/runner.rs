//! Execution: what a scheduled job actually does when its slot comes up.
//!
//! Two shapes of work share one outcome type. An agent run starts the same runtime the terminal
//! interface starts and drives the same turn loop the headless `chat` command drives, in a session
//! of its own, so a scheduled prompt can never land inside a conversation somebody is having. A
//! script run never touches the model at all, because a job that only has to check a port should
//! not cost a token.

use std::path::{Path, PathBuf};
use std::time::Duration;

use std::sync::Arc;
use vikram_contracts::{Block, BotId, Message, PermissionMode, Role, SessionId};
use vikram_runtime::config::Config;
use vikram_runtime::types::{RuntimeEvent, RuntimeStatus, TurnState};
use vikram_runtime::{BotControl, Runtime, RuntimeHandle};

use crate::presence::{DISK_REASON, Liveness, Presence};
use crate::store::ScheduleStore;
use crate::time::{Millis, now_ms};

/// How much of a run's own words are kept. A job that prints a gigabyte must not fill the database.
const OUTPUT_CAP: usize = 64 * 1024;

/// The disk share at which maintenance dozes the daemon, and the share it must come back below to
/// wake it. The evolution loop measures the same band; the hysteresis lives here with the actuator.
const DISK_DOZE_SHARE: f32 = 0.95;
const DISK_WAKE_SHARE: f32 = 0.90;

/// What one execution produced, whether it was an agent turn or a script.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOutcome {
    pub ok: bool,
    /// What the work said, clipped to `OUTPUT_CAP`.
    pub output: String,
    /// The session the turn ran in, so the operator can go and read it.
    pub session: Option<String>,
    pub error: Option<String>,
}

impl RunOutcome {
    pub fn finished(output: impl Into<String>, session: Option<String>) -> Self {
        Self {
            ok: true,
            output: clip(output.into()),
            session,
            error: None,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: String::new(),
            session: None,
            error: Some(error.into()),
        }
    }

    /// The one line a run history needs.
    pub fn summary(&self) -> String {
        if self.ok {
            let first = self.output.lines().next().unwrap_or("").trim();
            if first.is_empty() {
                "finished".to_string()
            } else {
                clip_line(first)
            }
        } else {
            self.error
                .as_deref()
                .map(clip_line)
                .unwrap_or_else(|| "failed".to_string())
        }
    }
}

/// What to ask the agent to do.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub prompt: String,
    /// The session title. Derived from the prompt when absent, so a job named "nightly report"
    /// reads as that in the session list rather than "Session 4".
    pub title: Option<String>,
    pub model: Option<String>,
    /// Refuse, rather than wait for, anything that wants approval. A scheduled job has nobody to
    /// approve it, and silently granting it would be the runtime authorising itself.
    pub unattended: bool,
}

impl AgentRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            title: None,
            model: None,
            unattended: true,
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn waiting_for_approval(mut self) -> Self {
        self.unattended = false;
        self
    }
}

/// Run one agent turn in a fresh session and wait for it to settle.
///
/// The turn runs under a deadline: a model that never answers must not hold a worker forever, and
/// a cancelled turn is reported as a timeout rather than as a success with no output.
pub async fn run_agent(config: Config, request: AgentRequest, timeout: Duration) -> RunOutcome {
    let title = request
        .title
        .clone()
        .unwrap_or_else(|| title_from(&request.prompt));
    let work = turn(config, request, title);

    match tokio::time::timeout(timeout, work).await {
        Ok(outcome) => outcome,
        Err(_) => RunOutcome::failed(format!(
            "timed out after {}",
            crate::time::humanize_ms(timeout.as_millis() as i64)
        )),
    }
}

async fn turn(config: Config, request: AgentRequest, title: String) -> RunOutcome {
    let runtime = match Runtime::start(config).await {
        Ok(runtime) => runtime,
        Err(err) => return RunOutcome::failed(format!("could not start the runtime: {err}")),
    };
    let handle = runtime.handle();
    let snapshot = handle.snapshot();
    if let RuntimeStatus::Error(reason) = &snapshot.status {
        return RunOutcome::failed(format!(
            "{reason}; run `vikram config set model <id>` and export the key"
        ));
    }

    let session = match handle.create_session(Some(title)) {
        Ok(session) => session,
        Err(err) => return RunOutcome::failed(format!("could not open a session: {err}")),
    };
    if let Some(model) = &request.model
        && let Err(err) = handle.set_model(&session, model.clone())
    {
        return RunOutcome::failed(format!("could not use model {model}: {err}"));
    }
    if request.unattended
        && handle.mode() == PermissionMode::Ask
        && handle
            .set_mode(&session, PermissionMode::Unattended)
            .is_err()
    {
        return RunOutcome::failed("could not switch the session to unattended");
    }

    let mut events = handle.events();
    if let Err(err) = handle.submit(&session, request.prompt) {
        return RunOutcome::failed(format!("could not submit the job: {err}"));
    }

    let mut output = String::new();
    let mut denied = 0_usize;
    let mut failure: Option<String> = None;
    loop {
        match events.recv().await {
            Ok(RuntimeEvent::StreamDelta {
                session: sid, text, ..
            }) if sid == session => output.push_str(&text),
            Ok(RuntimeEvent::ApprovalRequested(prompt)) if prompt.session == session => {
                denied += 1;
                failure.get_or_insert_with(|| {
                    format!(
                        "{} {} was refused: nobody to ask",
                        prompt.verb, prompt.resource
                    )
                });
                let _ = handle.answer_approval(&prompt.approval, false, false);
            }
            Ok(RuntimeEvent::Error { text }) => {
                failure.get_or_insert(text);
            }
            Ok(RuntimeEvent::TurnFinished {
                session: sid,
                state,
                ..
            }) if sid == session => {
                if !matches!(state, TurnState::Idle | TurnState::Cancelled) {
                    failure.get_or_insert_with(|| format!("the turn ended {state:?}"));
                }
                break;
            }
            Ok(RuntimeEvent::Shutdown) => {
                failure.get_or_insert_with(|| "the runtime shut down mid-turn".to_string());
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Fell behind the bus — skipped events are deltas, not terminal state.
                continue;
            }
            Err(err) => {
                failure.get_or_insert_with(|| format!("event stream closed: {err}"));
                break;
            }
        }
    }

    // The learning pass is a side effect of finishing a turn; letting it finish keeps a burst of
    // scheduled jobs from piling up reviews behind them. It is never worth failing a job over.
    if handle.learning_busy() {
        let _ = handle.wait_for_learning(Duration::from_secs(20)).await;
    }

    let session_id = Some(session.to_string());
    if let Some(error) = failure {
        let mut outcome = RunOutcome::failed(error);
        outcome.output = clip(std::mem::take(&mut output));
        outcome.session = session_id;
        if denied > 0 {
            outcome.error = Some(format!(
                "{} ({} request(s) refused)",
                outcome.error.unwrap_or_default(),
                denied
            ));
        }
        outcome
    } else {
        RunOutcome::finished(output, session_id)
    }
}

/// A job that runs as a named agent.
///
/// A scheduled delegation is not an anonymous turn: the work happens in a session that agent owns,
/// carrying its name, its parent link and its depth, so two bots working the same instruction never
/// look like one session and the operator can go and read what that bot actually did.
#[derive(Debug, Clone)]
pub struct DelegateRequest {
    pub agent: String,
    pub prompt: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub unattended: bool,
}

impl DelegateRequest {
    pub fn new(agent: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            prompt: prompt.into(),
            title: None,
            model: None,
            unattended: true,
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn waiting_for_approval(mut self) -> Self {
        self.unattended = false;
        self
    }
}

/// Asked, between events, whether one member of a running hive should be stopped. The daemon
/// answers from the scheduler database, so the request survives the process boundary that
/// separates `vikram jobs cancel` from the runtime doing the work.
pub type StopProbe = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// A job that runs as a team: one delegated turn per member, all at once.
pub struct HiveRosterRequest {
    pub hive: String,
    pub prompt: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub unattended: bool,
    pub order: HiveRosterOrder,
    pub stop: Option<StopProbe>,
}

/// Whether a team's members work at the same time or one after another.
///
/// Parallel is the default because the members do not depend on each other; sequential exists for a
/// team whose members share something outside vikram — a rate limit, a single test database, a
/// deploy lock — where two turns at once would interfere.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HiveRosterOrder {
    #[default]
    Parallel,
    Sequential,
}

impl HiveRosterOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            HiveRosterOrder::Parallel => "parallel",
            HiveRosterOrder::Sequential => "sequential",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "parallel" | "together" | "at-once" | "all" => Some(HiveRosterOrder::Parallel),
            "sequential" | "serial" | "one" | "in-order" => Some(HiveRosterOrder::Sequential),
            _ => None,
        }
    }
}

impl std::fmt::Display for HiveRosterOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl HiveRosterRequest {
    pub fn new(hive: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            hive: hive.into(),
            prompt: prompt.into(),
            title: None,
            model: None,
            unattended: true,
            order: HiveRosterOrder::Parallel,
            stop: None,
        }
    }

    /// Ask this probe, while the team runs, which members the operator has stopped.
    pub fn stopping_when(mut self, probe: Option<StopProbe>) -> Self {
        self.stop = probe;
        self
    }

    pub fn in_order(mut self, order: HiveRosterOrder) -> Self {
        self.order = order;
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn waiting_for_approval(mut self) -> Self {
        self.unattended = false;
        self
    }
}

/// How many members one scheduled team run will start. A schedule is unattended: eight parallel
/// model turns is already a lot of somebody's credit to spend without being asked.
const MAX_HIVE_ROSTER_MEMBERS: usize = 8;

/// Run the job as the bot it was assigned to.
pub async fn run_delegate(
    config: Config,
    request: DelegateRequest,
    timeout: Duration,
) -> RunOutcome {
    let name = request.agent.clone();
    let title = request
        .title
        .clone()
        .unwrap_or_else(|| title_from(&request.prompt));
    let work = async move {
        let agent = BotId::new(&request.agent);
        let fanout = delegated(
            config,
            vec![agent],
            request.prompt,
            title,
            request.model,
            request.unattended,
            HiveRosterOrder::Parallel,
            None,
        )
        .await;
        fanout
            .results
            .into_iter()
            .next()
            .map(|(_, outcome)| outcome)
            .unwrap_or_else(|| RunOutcome::failed(format!("{name} was given no work")))
    };

    match tokio::time::timeout(timeout, work).await {
        Ok(outcome) => outcome,
        Err(_) => RunOutcome::failed(format!(
            "timed out after {}",
            crate::time::humanize_ms(timeout.as_millis() as i64)
        )),
    }
}

/// Run the job as every member of a team, in parallel, and report each one separately.
pub async fn run_hive_roster(
    config: Config,
    request: HiveRosterRequest,
    timeout: Duration,
) -> RunOutcome {
    let members = match hive_roster_members(&request.hive) {
        Ok(members) => members,
        Err(err) => return RunOutcome::failed(err),
    };
    let title = request
        .title
        .clone()
        .unwrap_or_else(|| title_from(&request.prompt));
    let work = async move {
        let fanout = delegated(
            config,
            members,
            request.prompt,
            title,
            request.model,
            request.unattended,
            request.order,
            request.stop,
        )
        .await;
        hive_roster_report(fanout)
    };

    match tokio::time::timeout(timeout, work).await {
        Ok(outcome) => outcome,
        Err(_) => RunOutcome::failed(format!(
            "timed out after {}",
            crate::time::humanize_ms(timeout.as_millis() as i64)
        )),
    }
}

/// The members of a saved team, or a reason a scheduled run cannot use it.
fn hive_roster_members(name: &str) -> Result<Vec<BotId>, String> {
    members_of(
        &vikram_runtime::hive_roster::HiveRosterManager::load(),
        name,
    )
}

fn members_of(
    rosters: &vikram_runtime::hive_roster::HiveRosterManager,
    name: &str,
) -> Result<Vec<BotId>, String> {
    let Some(roster) = rosters.get(name) else {
        return Err(format!("no hive called `{name}`"));
    };
    if roster.members.is_empty() {
        return Err(format!("hive `{name}` has no members"));
    }
    if roster.members.len() > MAX_HIVE_ROSTER_MEMBERS {
        return Err(format!(
            "hive `{name}` has {} members; a scheduled hive run stops at {MAX_HIVE_ROSTER_MEMBERS}",
            roster.members.len()
        ));
    }
    Ok(roster.members)
}

struct Fanout {
    parent: Option<String>,
    results: Vec<(BotId, RunOutcome)>,
}

/// Start one delegated turn per agent inside a single runtime and wait for all of them.
///
/// One runtime rather than one per bot, because they share a provider, a store and a permission
/// posture — a second runtime would be a second set of credentials to keep in step. Each agent
/// still gets its own session, which is what makes the identities separable.
#[allow(clippy::too_many_arguments)]
async fn delegated(
    config: Config,
    agents: Vec<BotId>,
    prompt: String,
    title: String,
    model: Option<String>,
    unattended: bool,
    order: HiveRosterOrder,
    stop: Option<StopProbe>,
) -> Fanout {
    let mut fanout = Fanout {
        parent: None,
        results: Vec::new(),
    };

    let runtime = match Runtime::start(config).await {
        Ok(runtime) => runtime,
        Err(err) => {
            let why = format!("could not start the runtime: {err}");
            for agent in agents {
                fanout
                    .results
                    .push((agent, RunOutcome::failed(why.clone())));
            }
            return fanout;
        }
    };
    let handle = runtime.handle();
    if let RuntimeStatus::Error(reason) = &handle.snapshot().status {
        let why = format!("{reason}; run `vikram config set model <id>` and export the key");
        for agent in agents {
            fanout
                .results
                .push((agent, RunOutcome::failed(why.clone())));
        }
        return fanout;
    }

    let parent = match handle.create_session(Some(title)) {
        Ok(session) => session,
        Err(err) => {
            let why = format!("could not open a session: {err}");
            for agent in agents {
                fanout
                    .results
                    .push((agent, RunOutcome::failed(why.clone())));
            }
            return fanout;
        }
    };
    fanout.parent = Some(parent.to_string());
    if let Some(model) = &model
        && let Err(err) = handle.set_model(&parent, model.clone())
    {
        let why = format!("could not use model {model}: {err}");
        for agent in agents {
            fanout
                .results
                .push((agent, RunOutcome::failed(why.clone())));
        }
        return fanout;
    }
    if unattended
        && handle.mode() == PermissionMode::Ask
        && handle
            .set_mode(&parent, PermissionMode::Unattended)
            .is_err()
    {
        let why = "could not switch the session to unattended".to_string();
        for agent in agents {
            fanout
                .results
                .push((agent, RunOutcome::failed(why.clone())));
        }
        return fanout;
    }

    let mut events = handle.events();
    let mut denied = 0_usize;
    match order {
        HiveRosterOrder::Parallel => {
            let started = spawn_into(&handle, &agents, &prompt, &parent, &mut fanout.results);
            let children = children_of(&handle, &started, &mut fanout.results);
            let (finished, failure, stopped) =
                wait_for(&mut events, &handle, &children, &mut denied, stop.as_ref()).await;
            pause_learning(&handle).await;
            for (sid, agent) in children {
                let state = finished
                    .iter()
                    .find(|(session, _)| *session == sid)
                    .map(|(_, state)| *state);
                let outcome = if stopped.contains(&agent) {
                    RunOutcome::failed(format!("{} was stopped by the operator", agent.as_str()))
                } else {
                    outcome_of(&handle, sid, state, &failure, denied)
                };
                fanout.results.push((agent, outcome));
            }
        }
        HiveRosterOrder::Sequential => {
            // One member at a time: the next turn does not start until the previous one has
            // finished, so a team sharing a rate limit or a single test database cannot collide.
            // Each member still gets its own session; sequential means ordered, not shared.
            for agent in &agents {
                // A member stopped before its turn began never starts, so the queue behind it moves
                // on instead of waiting for work nobody wants any more.
                if stop.as_ref().is_some_and(|probe| probe(agent.as_str())) {
                    fanout.results.push((
                        agent.clone(),
                        RunOutcome::failed(format!(
                            "{} was stopped by the operator",
                            agent.as_str()
                        )),
                    ));
                    continue;
                }
                let started = spawn_into(
                    &handle,
                    std::slice::from_ref(agent),
                    &prompt,
                    &parent,
                    &mut fanout.results,
                );
                let children = children_of(&handle, &started, &mut fanout.results);
                if children.is_empty() {
                    continue;
                }
                let (finished, failure, stopped) =
                    wait_for(&mut events, &handle, &children, &mut denied, stop.as_ref()).await;
                for (sid, child) in children {
                    let state = finished
                        .iter()
                        .find(|(session, _)| *session == sid)
                        .map(|(_, state)| *state);
                    let outcome = if stopped.contains(&child) {
                        RunOutcome::failed(format!(
                            "{} was stopped by the operator",
                            child.as_str()
                        ))
                    } else {
                        outcome_of(&handle, sid, state, &failure, denied)
                    };
                    fanout.results.push((child, outcome));
                }
            }
            pause_learning(&handle).await;
        }
    }

    fanout
}

/// Start every named agent on the same request, recording the ones that would not start.
fn spawn_into(
    handle: &RuntimeHandle,
    agents: &[BotId],
    prompt: &str,
    parent: &SessionId,
    results: &mut Vec<(BotId, RunOutcome)>,
) -> Vec<BotId> {
    let mut started = Vec::new();
    for agent in agents {
        match handle.spawn_agent(agent.clone(), prompt.to_string(), parent.clone()) {
            Ok(()) => started.push(agent.clone()),
            Err(err) => results.push((
                agent.clone(),
                RunOutcome::failed(format!("could not start {agent}: {err}")),
            )),
        }
    }
    started
}

/// The child sessions the runtime opened for the agents that started. A runtime that accepted the
/// delegation but opened no session leaves the agent without an answer, so it is recorded here
/// rather than silently dropped from the report.
fn children_of(
    handle: &RuntimeHandle,
    started: &[BotId],
    results: &mut Vec<(BotId, RunOutcome)>,
) -> Vec<(SessionId, BotId)> {
    let mut children: Vec<(SessionId, BotId)> = Vec::new();
    for snapshot in handle.bot_snapshots() {
        if let (Some(session), true) = (
            snapshot.session.clone(),
            started.contains(&snapshot.profile.id),
        ) {
            children.push((session, snapshot.profile.id.clone()));
        }
    }
    let missing: Vec<BotId> = started
        .iter()
        .filter(|agent| !children.iter().any(|(_, child)| child == *agent))
        .cloned()
        .collect();
    for agent in missing {
        results.push((
            agent.clone(),
            RunOutcome::failed(format!("the runtime opened no session for {agent}")),
        ));
    }
    children
}

/// Wait until every child session has finished its turn. A turn the runtime refuses approval for
/// could otherwise wait forever, so a refusal is answered immediately and counted.
async fn wait_for(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    handle: &RuntimeHandle,
    children: &[(SessionId, BotId)],
    denied: &mut usize,
    stop: Option<&StopProbe>,
) -> (Vec<(SessionId, TurnState)>, Option<String>, Vec<BotId>) {
    let mut finished: Vec<(SessionId, TurnState)> = Vec::new();
    let mut failure: Option<String> = None;
    let mut stopped: Vec<BotId> = Vec::new();
    while finished.len() < children.len() {
        // Wait for an event, but not forever: a stop request arrives through the database, not
        // through the event stream, so the loop has to surface every so often to look.
        let next = match tokio::time::timeout(Duration::from_millis(400), events.recv()).await {
            Ok(event) => event,
            Err(_) => {
                if let Some(probe) = stop {
                    for (_, agent) in children {
                        if stopped.contains(agent) || !probe(agent.as_str()) {
                            continue;
                        }
                        // The runtime owns the turn; asking it to stop is the only way to end one
                        // member without ending the team.
                        let _ = handle.control_bot(agent, BotControl::Stop);
                        stopped.push(agent.clone());
                    }
                }
                continue;
            }
        };
        match next {
            Ok(RuntimeEvent::ApprovalRequested(prompt)) => {
                if children.iter().any(|(child, _)| *child == prompt.session) {
                    *denied += 1;
                    let _ = handle.answer_approval(&prompt.approval, false, false);
                }
            }
            Ok(RuntimeEvent::Error { text }) => {
                failure.get_or_insert(text);
            }
            Ok(RuntimeEvent::TurnFinished {
                session: sid,
                state,
                ..
            }) if children.iter().any(|(child, _)| *child == sid) => {
                finished.push((sid, state));
            }
            Ok(RuntimeEvent::Shutdown) => {
                failure.get_or_insert_with(|| "the runtime shut down mid-turn".to_string());
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                continue;
            }
            Err(err) => {
                failure.get_or_insert_with(|| format!("event stream closed: {err}"));
                break;
            }
        }
    }
    (finished, failure, stopped)
}

/// Let the runtime finish whatever it learned from the turn before the process is dropped.
async fn pause_learning(handle: &RuntimeHandle) {
    if handle.learning_busy() {
        let _ = handle.wait_for_learning(Duration::from_secs(20)).await;
    }
}

/// What one member's turn amounts to, read back from the snapshot because the handle exposes no
/// session accessor: the answer is the last thing that member said.
fn outcome_of(
    handle: &RuntimeHandle,
    sid: SessionId,
    state: Option<TurnState>,
    failure: &Option<String>,
    denied: usize,
) -> RunOutcome {
    let answer = handle
        .snapshot()
        .sessions
        .iter()
        .find(|session| session.id == sid)
        .and_then(|session| answer_text(&session.messages));
    let mut outcome = match state {
        Some(TurnState::Idle) | Some(TurnState::Cancelled) => match answer {
            Some(text) if !text.trim().is_empty() => {
                RunOutcome::finished(text, Some(sid.to_string()))
            }
            _ => RunOutcome::failed("the agent finished without saying anything"),
        },
        Some(other) => RunOutcome::failed(format!("the turn ended {other:?}")),
        None => RunOutcome::failed(
            failure
                .clone()
                .unwrap_or_else(|| "the turn never finished".to_string()),
        ),
    };
    if denied > 0 && !outcome.ok {
        outcome.error = Some(format!(
            "{} ({} request(s) refused)",
            outcome.error.unwrap_or_default(),
            denied
        ));
    }
    outcome
}

/// Join a fan-out into one report a notification can carry.
fn hive_roster_report(fanout: Fanout) -> RunOutcome {
    let parent = fanout.parent.clone();
    let total = fanout.results.len();
    let mut lines = Vec::new();
    let mut broken = Vec::new();
    for (agent, outcome) in &fanout.results {
        if outcome.ok {
            lines.push(format!("{}: {}", agent, outcome.output.trim()));
        } else {
            let why = outcome.error.clone().unwrap_or_else(|| outcome.summary());
            broken.push(agent.to_string());
            lines.push(format!("{}: failed — {why}", agent));
        }
    }
    let text = lines.join("\n");
    if broken.is_empty() {
        return RunOutcome::finished(text, parent);
    }
    let mut outcome = RunOutcome::failed(format!(
        "{} of {total} members failed: {}",
        broken.len(),
        broken.join(", ")
    ));
    outcome.output = clip(text);
    outcome.session = parent;
    outcome
}

/// What an agent said last. Its transcript is not handed over, only the answer.
fn answer_text(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        if message.role != Role::Assistant {
            return None;
        }
        let text = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        (!text.trim().is_empty()).then_some(text)
    })
}

/// A command line handed to the host shell: `cmd /C` on Windows, `$SHELL -c` elsewhere.
fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let comspec = std::env::var("COMSPEC")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "cmd".to_owned());
        let mut cmd = tokio::process::Command::new(comspec);
        cmd.args(["/C", command]);
        cmd
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sh".to_owned());
        let mut cmd = tokio::process::Command::new(shell);
        cmd.args(["-c", command]);
        cmd
    }
}

/// Run one shell command. No model, no session, no tokens.
///
/// A shell string rather than a parsed argv, because the operator writes the command the way they
/// would type it, and a scheduled command that cannot use a pipe or a redirect is not worth
/// scheduling. The shell is `$SHELL`/`sh` on unix and `%COMSPEC%`/`cmd` on Windows.
/// One unattended pass of the evolution loop, with no model and no session: drain the reflection
/// queue, run one MAPE-K cycle, and report what each step did.
///
/// This is the only scheduled work that improves Vikram itself rather than doing something for the
/// operator. It opens the same store the runtime opens, so a cycle run here is the cycle the operator
/// would have run by hand — it just no longer needs somebody to be at the keyboard for it to happen.
pub async fn run_maintenance(home: PathBuf, timeout: Duration) -> RunOutcome {
    let work = tokio::task::spawn_blocking(move || maintenance(&home));
    match tokio::time::timeout(timeout, work).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) => RunOutcome::failed(format!("maintenance task failed: {err}")),
        Err(_) => RunOutcome::failed(format!(
            "timed out after {}",
            crate::time::humanize_ms(timeout.as_millis() as i64)
        )),
    }
}

fn maintenance(home: &Path) -> RunOutcome {
    let skills = home.join("skills");
    if let Err(err) = std::fs::create_dir_all(&skills) {
        return RunOutcome::failed(format!("could not create {}: {err}", skills.display()));
    }
    let db = home.join("evolution.db");
    let mut engine = match vikram_evolution::EvolutionEngine::open(db.clone(), skills) {
        Ok(engine) => engine,
        Err(err) => {
            return RunOutcome::failed(format!(
                "could not open the evolution store at {}: {err}",
                db.display()
            ));
        }
    };

    let mut lines: Vec<String> = Vec::new();
    let mut ok = true;

    match engine.reflection.recover_jobs() {
        Ok(recovered) if recovered > 0 => lines.push(format!(
            "reflection: recovered {recovered} interrupted job(s)"
        )),
        Ok(_) => {}
        Err(err) => {
            ok = false;
            lines.push(format!("reflection: could not recover jobs — {err}"));
        }
    }
    match engine.reflection.process_pending() {
        Ok(done) => lines.push(format!("reflection: {done} job(s) processed")),
        Err(err) => {
            ok = false;
            lines.push(format!("reflection: failed — {err}"));
        }
    }
    match engine.mapek.run_cycle() {
        Ok(result) => lines.push(format!(
            "cycle {}: {} planned, {} remediated, {} for review ({})",
            result.cycle_number,
            result.planned_actions,
            result.remediated_actions,
            result.recorded_for_review,
            if result.execution_success {
                "fully remediated"
            } else {
                result
                    .failure_reason
                    .as_deref()
                    .unwrap_or("not fully remediated")
            }
        )),
        Err(err) => {
            ok = false;
            lines.push(format!("cycle: failed — {err}"));
        }
    }

    // The cycle reports no resource reading of its own, so the disk evidence is taken from the
    // monitoring pass. Only a known share may move presence: an unreadable probe is not a full disk
    // and is not an empty one either.
    let disk = match engine.mapek.monitor() {
        Ok(data) if data.resource_metrics.disk_usage_known => {
            Some(data.resource_metrics.disk_usage)
        }
        Ok(_) => None,
        Err(err) => {
            lines.push(format!("disk: could not be read — {err}"));
            None
        }
    };
    if disk.is_some() {
        match ScheduleStore::open(&home.join("vikram.db")) {
            Ok(store) => lines.extend(react_to_disk(&store, disk, now_ms())),
            Err(err) => lines.push(format!("presence: could not be recorded — {err}")),
        }
    }

    // Night-shift forgetting: what stopped being used sinks out of the prompt, and the ledger
    // keeps every transition undoable. Runs here so an idle machine still tidies itself.
    match engine.curator.sweep(vikram_evolution::now_ms(), None) {
        Ok(sweep) => lines.push(format!(
            "curator: {} staled, {} archived, {} reactivated",
            sweep.staled, sweep.archived, sweep.reactivated
        )),
        Err(err) => {
            ok = false;
            lines.push(format!("curator: failed — {err}"));
        }
    }

    let text = lines.join("\n");
    if ok {
        RunOutcome::finished(text, None)
    } else {
        RunOutcome {
            ok: false,
            output: clip(text),
            session: None,
            error: Some("one or more maintenance steps failed".to_string()),
        }
    }
}

/// Let a known disk reading move presence, and say what it did.
///
/// A full disk dozes once: the daemon keeps working, but ordinary reports wait and urgent ones
/// still land. A recovered disk wakes only the doze this actuator set, because the reason is
/// recorded separately and an operator's doze carries no disk reason. Every store failure is a line
/// in the report, never a panic or a silent discard.
fn react_to_disk(store: &ScheduleStore, disk: Option<f32>, now: Millis) -> Vec<String> {
    let Some(share) = disk else {
        return Vec::new();
    };
    let presence = match Presence::load(store) {
        Ok(presence) => presence,
        Err(err) => return vec![format!("presence: could not be recorded — {err}")],
    };
    let full = percent(share);

    if share >= DISK_DOZE_SHARE {
        if presence.state != Liveness::Awake {
            return Vec::new();
        }
        let line = format!(
            "disk {full}% full: dozing until it comes back below {}%",
            percent(DISK_WAKE_SHARE)
        );
        if let Err(err) = presence.moved(Liveness::Dozing, now).apply(store, now) {
            return vec![format!("presence: could not be recorded — {err}")];
        }
        if let Err(err) = Presence::record_reason(store, DISK_REASON) {
            return vec![format!("presence: could not be recorded — {err}")];
        }
        return vec![line];
    }

    if share < DISK_WAKE_SHARE && presence.state == Liveness::Dozing {
        match Presence::reason(store) {
            Ok(reason) if reason.as_deref() == Some(DISK_REASON) => {}
            Ok(_) => return Vec::new(),
            Err(err) => return vec![format!("presence: could not be recorded — {err}")],
        }
        if let Err(err) = presence.moved(Liveness::Awake, now).apply(store, now) {
            return vec![format!("presence: could not be recorded — {err}")];
        }
        if let Err(err) = Presence::clear_reason(store) {
            return vec![format!("presence: could not be recorded — {err}")];
        }
        return vec![format!("disk {full}% full: awake again")];
    }

    Vec::new()
}

fn percent(share: f32) -> i64 {
    (share * 100.0).round() as i64
}

pub async fn run_script(command: &str, cwd: Option<&Path>, timeout: Duration) -> RunOutcome {
    let mut child = shell_command(command);
    child.kill_on_drop(true);
    child
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(cwd) = cwd {
        child.current_dir(cwd);
    }
    let mut child = match child.spawn() {
        Ok(child) => child,
        Err(err) => return RunOutcome::failed(format!("could not run the script: {err}")),
    };

    // `output()` buffers everything the job prints before OUTPUT_CAP ever applies, so a runaway
    // job could grow the daemon without bound. Reading the pipes directly keeps at most the cap
    // per stream and drains the rest.
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let finished = tokio::time::timeout(timeout, async {
        let (status, out, err) = tokio::join!(
            child.wait(),
            read_capped(&mut stdout, OUTPUT_CAP),
            read_capped(&mut stderr, OUTPUT_CAP),
        );
        (status, out, err)
    })
    .await;

    let (status, stdout, stderr) = match finished {
        Ok(inner) => inner,
        Err(_) => {
            return RunOutcome::failed(format!(
                "timed out after {}",
                crate::time::humanize_ms(timeout.as_millis() as i64)
            ));
        }
    };
    let status = match status {
        Ok(status) => status,
        Err(err) => return RunOutcome::failed(format!("could not run the script: {err}")),
    };

    let mut text = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr);
    if !stderr.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&stderr);
    }

    match status.success() {
        true => RunOutcome::finished(text, None),
        false => {
            let mut outcome = RunOutcome::failed(match status.code() {
                Some(code) => format!("exit status {code}"),
                None => "killed by a signal".to_string(),
            });
            outcome.output = clip(text);
            outcome
        }
    }
}

/// Drain a pipe to EOF but keep at most `cap` bytes: whatever does not fit is read and dropped,
/// so the child never blocks on a full pipe and the daemon never holds the overflow.
async fn read_capped<R>(reader: &mut R, cap: usize) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut kept = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    kept
}

/// A session title taken from the work itself, clipped on a word boundary.
pub fn title_from(prompt: &str) -> String {
    const LIMIT: usize = 60;
    let text = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return "Scheduled job".to_string();
    }
    if text.chars().count() <= LIMIT {
        return text;
    }
    let mut clipped = String::new();
    for word in text.split(' ') {
        if clipped.chars().count() + word.chars().count() + 1 > LIMIT - 1 {
            break;
        }
        if !clipped.is_empty() {
            clipped.push(' ');
        }
        clipped.push_str(word);
    }
    if clipped.chars().count() < LIMIT / 2 {
        clipped = text.chars().take(LIMIT - 1).collect();
    }
    clipped.push('…');
    clipped
}

fn clip(text: String) -> String {
    if text.len() <= OUTPUT_CAP {
        return text;
    }
    let mut end = OUTPUT_CAP;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut clipped = text[..end].to_string();
    clipped.push_str("\n… output clipped");
    clipped
}

fn clip_line(text: &str) -> String {
    const WIDTH: usize = 160;
    let flat = text.trim();
    if flat.chars().count() <= WIDTH {
        return flat.to_string();
    }
    let mut clipped: String = flat.chars().take(WIDTH - 1).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[tokio::test]
    async fn a_script_that_succeeds_keeps_what_it_printed() {
        let outcome = run_script("echo checking; echo nothing to report", None, seconds(10)).await;
        assert!(outcome.ok, "{outcome:?}");
        assert!(outcome.output.contains("checking"));
        assert!(outcome.output.contains("nothing to report"));
        assert!(outcome.session.is_none(), "a script opens no session");
    }

    #[tokio::test]
    async fn a_script_that_fails_says_why() {
        let outcome = run_script("echo broken >&2; exit 3", None, seconds(10)).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.error.as_deref(), Some("exit status 3"));
        assert!(
            outcome.output.contains("broken"),
            "stderr is part of the record"
        );
    }

    #[tokio::test]
    async fn a_script_that_prints_more_than_the_cap_is_clipped_not_buffered() {
        let outcome = run_script(
            "head -c 200000 /dev/zero | tr '\\0' 'x'; echo done >&2",
            None,
            seconds(10),
        )
        .await;
        assert!(outcome.ok, "{outcome:?}");
        assert!(
            outcome.output.len() <= OUTPUT_CAP + 64,
            "the stored result stays capped: {} bytes",
            outcome.output.len()
        );
        assert!(
            outcome.output.contains("output clipped"),
            "the clip marker says the output was cut"
        );
    }

    #[tokio::test]
    async fn the_pipe_reader_keeps_the_cap_and_drains_the_rest() {
        let data = vec![b'y'; OUTPUT_CAP * 4];
        let mut slice: &[u8] = &data;
        let kept = read_capped(&mut slice, OUTPUT_CAP).await;
        assert_eq!(kept.len(), OUTPUT_CAP);
        assert!(
            slice.is_empty(),
            "the overflow was drained, not left behind"
        );
    }

    #[tokio::test]
    async fn a_script_that_never_returns_times_out() {
        let outcome = run_script("sleep 5", None, Duration::from_millis(150)).await;
        assert!(!outcome.ok);
        assert!(
            outcome.error.as_deref().unwrap_or("").contains("timed out"),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_title_is_taken_from_the_work_and_stays_one_line() {
        assert_eq!(title_from("check the server"), "check the server");
        let long = "check   the server every ten minutes and also tell me about the disk";
        let title = title_from(long);
        assert!(title.chars().count() <= 60, "{title:?}");
        assert!(!title.contains("  "), "{title:?}");
        assert!(title.ends_with('…'), "{title:?}");
        assert_eq!(title_from("   "), "Scheduled job");
    }

    #[test]
    fn an_outcome_summarises_itself_in_one_line() {
        let mut outcome = RunOutcome::finished("all clear\nsecond line", None);
        assert_eq!(outcome.summary(), "all clear");
        outcome.output.clear();
        assert_eq!(outcome.summary(), "finished");
        assert_eq!(
            RunOutcome::failed("exit status 1").summary(),
            "exit status 1"
        );
    }

    #[test]
    fn huge_output_is_clipped_and_still_valid_utf8() {
        let mut text = "é".repeat(OUTPUT_CAP);
        text.push_str("tail");
        let clipped = clip(text);
        assert!(clipped.ends_with("… output clipped"));
        assert!(clipped.len() < OUTPUT_CAP + 64);
    }

    fn said(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![Block::Text {
                text: text.to_string(),
                trust: vikram_contracts::Trust::External,
            }],
        }
    }

    #[test]
    fn an_answer_is_the_last_thing_the_agent_said_and_only_that() {
        let messages = vec![
            Message::user("do the thing"),
            said("first draft"),
            Message::user("no, better"),
            said("the finished answer"),
        ];
        assert_eq!(
            answer_text(&messages).as_deref(),
            Some("the finished answer"),
            "a delegated job reports the answer, not the conversation that produced it"
        );
        assert_eq!(answer_text(&[Message::user("nothing yet")]), None);
        assert_eq!(answer_text(&[]), None);
    }

    #[test]
    fn a_team_is_refused_for_the_reasons_an_unattended_run_cannot_use_it() {
        let dir = tempfile::tempdir().unwrap();
        let rosters = vikram_runtime::hive_roster::HiveRosterManager::with_path(
            dir.path().join("hive-roster.json"),
        );
        assert!(
            members_of(&rosters, "ghost")
                .unwrap_err()
                .contains("no hive")
        );

        rosters.create("empty").unwrap();
        assert!(
            members_of(&rosters, "empty")
                .unwrap_err()
                .contains("no members")
        );

        rosters.create("crew").unwrap();
        rosters.add_member("crew", BotId::new("arun")).unwrap();
        rosters.add_member("crew", BotId::new("mira")).unwrap();
        assert_eq!(members_of(&rosters, "crew").unwrap().len(), 2);

        rosters.create("crowd").unwrap();
        for index in 0..=MAX_HIVE_ROSTER_MEMBERS {
            rosters
                .add_member("crowd", BotId::new(format!("bot{index}")))
                .unwrap();
        }
        assert!(
            members_of(&rosters, "crowd")
                .unwrap_err()
                .contains("stops at"),
            "a schedule must not start eleven model turns because somebody typed a big hive"
        );
    }

    #[test]
    fn an_order_is_read_the_way_an_operator_would_write_it() {
        assert_eq!(
            HiveRosterOrder::parse("parallel"),
            Some(HiveRosterOrder::Parallel)
        );
        assert_eq!(
            HiveRosterOrder::parse("SEQUENTIAL"),
            Some(HiveRosterOrder::Sequential)
        );
        assert_eq!(
            HiveRosterOrder::parse("one"),
            Some(HiveRosterOrder::Sequential)
        );
        assert_eq!(HiveRosterOrder::parse(""), None);
        assert_eq!(HiveRosterOrder::parse("sideways"), None);
        assert_eq!(HiveRosterOrder::Sequential.to_string(), "sequential");
        assert_eq!(HiveRosterOrder::default(), HiveRosterOrder::Parallel);
    }

    #[test]
    fn a_hive_roster_report_names_who_answered_and_who_did_not() {
        let finished = Fanout {
            parent: Some("ses_1".to_string()),
            results: vec![
                (
                    BotId::new("arun"),
                    RunOutcome::finished("port 80 is up", Some("ses_a".to_string())),
                ),
                (
                    BotId::new("mira"),
                    RunOutcome::failed("timed out after 10m"),
                ),
            ],
        };
        let outcome = hive_roster_report(finished);
        assert!(!outcome.ok);
        assert_eq!(outcome.session.as_deref(), Some("ses_1"));
        assert!(outcome.output.contains("arun: port 80 is up"));
        assert!(outcome.output.contains("mira: failed — timed out"));
        assert!(outcome.error.unwrap().contains("mira"));

        let clean = hive_roster_report(Fanout {
            parent: Some("ses_1".to_string()),
            results: vec![(
                BotId::new("arun"),
                RunOutcome::finished("all clear", Some("ses_a".to_string())),
            )],
        });
        assert!(clean.ok);
        assert_eq!(clean.output, "arun: all clear");
    }

    #[tokio::test]
    async fn a_maintenance_run_creates_the_store_and_reports_its_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_maintenance(dir.path().to_path_buf(), seconds(60)).await;
        assert!(outcome.ok, "{outcome:?}");
        assert!(
            dir.path().join("evolution.db").exists(),
            "the store is created"
        );
        assert!(outcome.output.contains("reflection:"), "{}", outcome.output);
        assert!(outcome.output.contains("cycle 1"), "{}", outcome.output);
        assert!(outcome.session.is_none(), "maintenance opens no session");
    }

    #[tokio::test]
    async fn a_second_maintenance_run_advances_the_durable_cycle_count() {
        let dir = tempfile::tempdir().unwrap();
        let first = run_maintenance(dir.path().to_path_buf(), seconds(60)).await;
        assert!(first.ok, "{first:?}");
        let second = run_maintenance(dir.path().to_path_buf(), seconds(60)).await;
        assert!(second.ok, "{second:?}");
        assert!(
            second.output.contains("cycle 2"),
            "the cycle count is durable: {}",
            second.output
        );
    }

    fn presence_store() -> ScheduleStore {
        ScheduleStore::open_in_memory().unwrap()
    }

    #[test]
    fn a_full_disk_puts_the_daemon_to_doze_once() {
        let store = presence_store();
        let at = 1_700_000_000_000;

        let lines = react_to_disk(&store, Some(0.96), at);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("96%"), "{}", lines[0]);
        assert!(lines[0].contains("dozing"), "{}", lines[0]);
        assert_eq!(Presence::load(&store).unwrap().state, Liveness::Dozing);
        assert_eq!(
            Presence::reason(&store).unwrap().as_deref(),
            Some(DISK_REASON),
            "the reason is what lets a later run undo this doze and nobody else's"
        );

        assert!(
            react_to_disk(&store, Some(0.97), at + 1).is_empty(),
            "a second full run has nothing left to change"
        );
        let trail = store.audit_trail(10).unwrap();
        assert_eq!(
            trail
                .iter()
                .filter(|row| row.kind == "presence.set")
                .count(),
            1,
            "the transition happens once: {trail:?}"
        );

        let recovered = react_to_disk(&store, Some(0.42), at + 2);
        assert_eq!(recovered.len(), 1, "{recovered:?}");
        assert!(recovered[0].contains("awake"), "{}", recovered[0]);
        assert_eq!(Presence::load(&store).unwrap().state, Liveness::Awake);
        assert_eq!(Presence::reason(&store).unwrap(), None);
    }

    #[test]
    fn a_hand_set_doze_is_not_reverted_by_a_recovered_disk() {
        let store = presence_store();
        let at = 1_700_000_000_000;
        Presence::default()
            .moved(Liveness::Dozing, at)
            .apply(&store, at)
            .unwrap();

        assert!(
            react_to_disk(&store, Some(0.40), at + 1).is_empty(),
            "a doze without the disk reason was the operator's"
        );
        let presence = Presence::load(&store).unwrap();
        assert_eq!(presence.state, Liveness::Dozing);
        assert_eq!(presence.since, at, "the operator's transition stands");
    }

    #[test]
    fn an_unreadable_disk_changes_nothing() {
        let store = presence_store();
        let at = 1_700_000_000_000;

        assert!(react_to_disk(&store, None, at).is_empty());
        assert_eq!(Presence::load(&store).unwrap().state, Liveness::Awake);
        assert_eq!(Presence::reason(&store).unwrap(), None);
        assert!(
            store.audit_trail(10).unwrap().is_empty(),
            "an unreadable probe is neither a full disk nor an empty one"
        );

        let dozed = presence_store();
        react_to_disk(&dozed, Some(0.96), at);
        assert!(react_to_disk(&dozed, None, at + 1).is_empty());
        assert_eq!(
            Presence::load(&dozed).unwrap().state,
            Liveness::Dozing,
            "unknown evidence cannot take a standing state back"
        );
    }
}
