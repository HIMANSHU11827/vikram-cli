//! The edit → build → test → repair loop, as a first-class operation rather than an emergent
//! property of a chat loop.
//!
//! The host proposes a repair attempt; this module runs the verification plan (build, then
//! tests), classifies the failure, and — while budget remains — hands the failure back for the
//! next attempt. Nothing edits files here: the repair callback does that, and the workspace is
//! expected to be isolated per attempt (a git worktree when git is available, the working tree
//! otherwise) by the caller.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use vikram_contracts::VikramError;

use crate::sandbox::Sandbox;

/// Why a verification run failed, from reading its output. Unknown shapes stay unknown; the
/// raw output is always preserved for the model of the next attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailureKind {
    CompileError,
    TestFailure,
    LintFailure,
    RuntimeError,
    Timeout,
    Unknown,
}

impl FailureKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureKind::CompileError => "compile_error",
            FailureKind::TestFailure => "test_failure",
            FailureKind::LintFailure => "lint_failure",
            FailureKind::RuntimeError => "runtime_error",
            FailureKind::Timeout => "timeout",
            FailureKind::Unknown => "unknown",
        }
    }

    pub fn classify(output: &str, timed_out: bool) -> Self {
        if timed_out {
            return FailureKind::Timeout;
        }
        let lower = output.to_ascii_lowercase();
        if lower.contains("error[e") || lower.contains("could not compile") {
            return FailureKind::CompileError;
        }
        if lower.contains("failures:") || lower.contains("test result: failed") {
            return FailureKind::TestFailure;
        }
        if lower.contains("clippy::") || lower.contains("warning") && lower.contains("deny") {
            return FailureKind::LintFailure;
        }
        if lower.contains("panicked") || lower.contains("error:") {
            return FailureKind::RuntimeError;
        }
        FailureKind::Unknown
    }
}

/// One step of a verification plan. Shelling out keeps the plan build-system agnostic.
#[derive(Clone, Debug)]
pub struct PlanStep {
    pub name: String,
    pub command: String,
    pub timeout: Duration,
}

impl PlanStep {
    pub fn new(name: impl Into<String>, command: impl Into<String>, timeout: Duration) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            timeout,
        }
    }
}

/// A verification plan. The default is the Rust loop; callers override freely.
#[derive(Clone, Debug)]
pub struct VerificationPlan {
    pub steps: Vec<PlanStep>,
}

impl VerificationPlan {
    pub fn cargo() -> Self {
        Self {
            steps: vec![
                PlanStep::new("build", "cargo build --workspace", Duration::from_secs(600)),
                PlanStep::new("test", "cargo test --workspace", Duration::from_secs(600)),
            ],
        }
    }
}

#[derive(Clone, Debug)]
pub struct StepResult {
    pub name: String,
    pub ok: bool,
    pub output: String,
    pub duration_ms: u64,
    pub timed_out: bool,
}

#[derive(Clone, Debug)]
pub struct VerificationOutcome {
    pub ok: bool,
    pub attempts: u32,
    /// The failing step of the last attempt, with the classified kind.
    pub failure: Option<(String, FailureKind)>,
    pub steps: Vec<StepResult>,
}

/// The budget: how much work the loop may do before it must report honestly.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub max_attempts: u32,
    pub deadline: Option<Instant>,
}

impl Budget {
    pub fn attempts(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            deadline: None,
        }
    }
}

/// Run the plan once in `workspace_root` and return the steps with their results.
fn attempt_steps(
    workspace_root: &Path,
    plan: &VerificationPlan,
    sandbox: &Sandbox,
) -> Vec<StepResult> {
    plan.steps
        .iter()
        .map(|step| run_step(workspace_root, step, sandbox))
        .collect()
}

fn run_step(workspace_root: &Path, step: &PlanStep, sandbox: &Sandbox) -> StepResult {
    let started = Instant::now();
    let mut cmd = shell_cmd(&step.command);
    sandbox.wrap_command(&mut cmd, workspace_root);
    cmd.current_dir(workspace_root);
    let sandbox_note = sandbox.enforcement_notice();
    #[cfg(not(target_os = "windows"))]
    if let Ok(vars) = std::env::var("CARGO_INCREMENTAL") {
        cmd.env("CARGO_INCREMENTAL", vars);
    }

    let mut timed_out = false;
    let output = match wait_with_timeout(&mut cmd, step.timeout) {
        Ok(Some(output)) => output,
        Ok(None) => {
            timed_out = true;
            std::process::Output {
                status: exit_status_failure(),
                stdout: Vec::new(),
                stderr: b"verification step timed out".to_vec(),
            }
        }
        Err(e) => {
            // A spawn or wait failure is not a timeout; report what happened.
            return StepResult {
                name: step.name.clone(),
                ok: false,
                output: e,
                duration_ms: started.elapsed().as_millis() as u64,
                timed_out: false,
            };
        }
    };
    let mut text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if let Some(note) = &sandbox_note {
        text.push_str(&format!("\n[sandbox] {note}"));
    }
    StepResult {
        name: step.name.clone(),
        ok: output.status.success(),
        output: text,
        duration_ms: started.elapsed().as_millis() as u64,
        timed_out,
    }
}

pub(crate) fn shell_cmd(command: &str) -> Command {
    #[cfg(windows)]
    {
        let comspec = std::env::var("COMSPEC")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "cmd".to_owned());
        let mut c = Command::new(&comspec);
        c.args(["/C", command]);
        c
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sh".to_owned());
        let mut c = Command::new(&shell);
        c.args(["-c", command]);
        c
    }
}

/// Run `cmd` under `timeout`, capturing stdout and stderr.
///
/// `Ok(Some)` is a finished command, `Ok(None)` is a timeout, and `Err` is a
/// spawn or wait failure — the three must stay distinct so a command that never
/// started is never reported as having timed out. On Unix the child leads its
/// own process group, so a timeout kills the whole group: a backgrounded
/// grandchild cannot outlive the call. The pipes are drained on detached threads
/// with a deadline (the `computer.rs` pattern): `wait_with_output` would block
/// on pipe EOF forever when a surviving grandchild holds stdout open.
pub(crate) fn wait_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
) -> Result<Option<std::process::Output>, String> {
    use std::io::Read;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start command: {e}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_tree(&mut child);
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                kill_tree(&mut child);
                return Err(format!("failed while waiting: {e}"));
            }
        }
    };
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    // Detached readers: they must not be joined, or a grandchild holding the pipe
    // would block this call exactly like `wait_with_output` did. They exit when
    // the pipe finally closes.
    match child.stdout.take() {
        Some(mut pipe) => {
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = pipe.read_to_end(&mut buffer);
                let _ = out_tx.send(buffer);
            });
        }
        None => {
            let _ = out_tx.send(Vec::new());
        }
    }
    match child.stderr.take() {
        Some(mut pipe) => {
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = pipe.read_to_end(&mut buffer);
                let _ = err_tx.send(buffer);
            });
        }
        None => {
            let _ = err_tx.send(Vec::new());
        }
    }
    let drain_deadline = Instant::now() + Duration::from_secs(1);
    let collect = |rx: std::sync::mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(drain_deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_default()
    };
    Ok(Some(std::process::Output {
        status,
        stdout: collect(out_rx),
        stderr: collect(err_rx),
    }))
}

/// Kill the child and, on Unix, its whole process group. The child was spawned
/// with `process_group(0)`, so its pid is also the pgid and a negative pid
/// addresses the group — `child.kill()` alone would leave grandchildren running.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        // Prefer the libc call: Termux has no `kill` binary on PATH (it's a shell builtin),
        // so spawning it would silently leave the process group alive.
        // SAFETY: kill(2) is always safe to call; a stale pgid just returns ESRCH.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(windows))]
fn exit_status_failure() -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(1)
}

#[cfg(windows)]
fn exit_status_failure() -> std::process::ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(1)
}

/// Was the failure transient enough that a plain retry is a reasonable attempt?
fn retryable(kind: &FailureKind) -> bool {
    !matches!(kind, FailureKind::Timeout)
}

/// The loop. `repair` is called with the failing step and its output, and is expected to edit
/// the workspace in place. Each attempt runs its own steps so double-counting successes is
/// impossible: the loop stops at the first fully green attempt.
pub fn verify(
    workspace_root: &Path,
    plan: &VerificationPlan,
    budget: &Budget,
    sandbox: &Sandbox,
    mut repair: impl FnMut(&str, &str) -> Option<Vec<PathBuf>>,
) -> Result<VerificationOutcome, VikramError> {
    let mut all_steps: Vec<StepResult> = Vec::new();
    let mut attempt: u32 = 0;

    if let Some(deadline) = budget.deadline
        && Instant::now() >= deadline
    {
        return Err(VikramError::InvalidState(
            "verification budget exhausted before the first attempt".into(),
        ));
    }

    loop {
        attempt += 1;
        let mut green = true;
        let mut failing: Option<(String, FailureKind)> = None;
        for step in attempt_steps(workspace_root, plan, sandbox) {
            all_steps.push(step.clone());
            if !step.ok {
                let kind = FailureKind::classify(&step.output, step.timed_out);
                failing = Some((step.name.clone(), kind.clone()));
                green = false;
                break;
            }
        }

        if green {
            return Ok(VerificationOutcome {
                ok: true,
                attempts: attempt,
                failure: None,
                steps: all_steps,
            });
        }

        let (name, kind) = failing.expect("not green implies a failing step");
        let spent_out = all_steps
            .iter()
            .rev()
            .find(|s| s.name == name && !s.ok)
            .map(|s| s.output.clone())
            .unwrap_or_default();

        let out_of_attempts = attempt >= budget.max_attempts;
        let out_of_time = budget
            .deadline
            .map(|d| Instant::now() >= d)
            .unwrap_or(false);
        if out_of_attempts || out_of_time || !retryable(&kind) {
            return Ok(VerificationOutcome {
                ok: false,
                attempts: attempt,
                failure: Some((name, kind)),
                steps: all_steps,
            });
        }
        let Some(edits) = repair(&name, &spent_out) else {
            return Ok(VerificationOutcome {
                ok: false,
                attempts: attempt,
                failure: Some((name, kind)),
                steps: all_steps,
            });
        };
        if edits.is_empty() {
            // A repair that touched nothing cannot change the next result.
            return Ok(VerificationOutcome {
                ok: false,
                attempts: attempt,
                failure: Some((name, kind)),
                steps: all_steps,
            });
        }
    }
}

/// Isolate per-attempt work via `git worktree` when the workspace is a git repo; otherwise the
/// caller's tree is used as-is (that limitation is reported, not hidden).
pub fn isolate_attempts(workspace_root: &Path, base: &str) -> Result<Option<PathBuf>, VikramError> {
    let ok = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return Ok(None);
    }
    let dir = std::env::temp_dir().join(format!("vikram-verify-{}-{}", std::process::id(), base));
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["worktree", "add", "--detach"])
        .arg(&dir)
        .output()
        .map_err(|e| VikramError::InvalidState(e.to_string()))?;
    if !out.status.success() {
        return Err(VikramError::InvalidState(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(Some(dir))
}

pub fn remove_isolation(isolate_dir: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(isolate_dir)
        .args([
            "worktree",
            "remove",
            "--force",
            isolate_dir.to_string_lossy().as_ref(),
        ])
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn workspace_with(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vikram-verify-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
        dir
    }

    #[test]
    fn a_green_plan_says_so_in_one_attempt() {
        let plan = VerificationPlan {
            steps: vec![PlanStep::new("true", "true", Duration::from_secs(5))],
        };
        let ws = workspace_with("green", "");
        let outcome = verify(
            &ws,
            &plan,
            &Budget::attempts(3),
            &Sandbox::disabled("test"),
            |_, _| None,
        )
        .unwrap();
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(outcome.attempts, 1);
    }

    #[test]
    fn a_transient_failure_is_repaired_and_then_passes() {
        let dir = workspace_with("retry", "");
        let counter = dir.join("attempts.txt");
        let counter_for_repair = counter.clone();
        let plan = VerificationPlan {
            steps: vec![
                PlanStep::new("check", "test -f marker.txt", Duration::from_secs(5)),
                PlanStep::new("all-ok", "true", Duration::from_secs(5)),
            ],
        };
        let outcome = verify(
            &dir,
            &plan,
            &Budget::attempts(3),
            &Sandbox::disabled("test"),
            |_, _| {
                let seen = counter_for_repair.metadata().map(|m| m.len()).unwrap_or(0);
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&counter_for_repair)
                    .unwrap();
                writeln!(f, "x").unwrap();
                if seen == 0 {
                    let _ = std::fs::write(dir.join("marker.txt"), "now ok");
                    return Some(vec![dir.join("marker.txt")]);
                }
                None
            },
        )
        .unwrap();
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(outcome.attempts, 2);
    }

    #[test]
    fn budget_exhaustion_reports_the_kind_not_a_crash() {
        let dir = workspace_with("exhaust", "");
        let plan = VerificationPlan {
            steps: vec![PlanStep::new(
                "always-fails",
                "echo 'error[E0425]: no' && false",
                Duration::from_secs(5),
            )],
        };
        let outcome = verify(
            &dir,
            &plan,
            &Budget::attempts(2),
            &Sandbox::disabled("test"),
            |_, _| Some(vec![dir.join("untouched.txt")]),
        )
        .unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.attempts, 2);
        let (_, kind) = outcome.failure.expect("failure classified");
        assert_eq!(kind, FailureKind::CompileError);
    }

    #[test]
    fn an_empty_repair_ends_the_loop_early() {
        let dir = workspace_with("noop-repair", "");
        let plan = VerificationPlan {
            steps: vec![PlanStep::new(
                "fails",
                "echo 'failures: 1' >/dev/null; false",
                Duration::from_secs(5),
            )],
        };
        let outcome = verify(
            &dir,
            &plan,
            &Budget::attempts(5),
            &Sandbox::disabled("test"),
            |_, _| Some(Vec::new()),
        )
        .unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.attempts, 1, "no-change repair must not burn budget");
    }

    #[test]
    fn timeouts_are_classified_as_timeout() {
        assert_eq!(
            FailureKind::classify("whatever", true),
            FailureKind::Timeout
        );
        assert_eq!(
            FailureKind::classify("error[E0102]: dup", false),
            FailureKind::CompileError
        );
        assert_eq!(
            FailureKind::classify("test result: FAILED. failures: 1", false),
            FailureKind::TestFailure
        );
    }
}
