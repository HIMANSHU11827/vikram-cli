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
    /// What the workspace actually changed, collected after the last attempt.
    pub changes: ChangeReport,
}

/// Bounded evidence of what changed, not a judgment about whether the right thing
/// changed. `files` is `git status --porcelain` output (repo-relative paths);
/// `note` explains why the list may be empty or partial; `warnings` flags declared
/// `expected_changes` that git does not observe. A green plan with zero changed
/// files is reported as evidence, never scored.
#[derive(Clone, Debug, Default)]
pub struct ChangeReport {
    pub files: Vec<String>,
    pub note: Option<String>,
    pub warnings: Vec<String>,
    /// Declared `expect` patterns checked against the changed files' current
    /// content. Case-sensitive literal substring matches; `checked` is false
    /// when no file evidence was available (non-git workspace, git failure).
    pub expectations: Vec<Expectation>,
}

/// One declared expectation vs the changed files. Evidence, not a verdict:
/// a missing pattern is a warning, never a failure — the intent may have been
/// satisfied differently than the agent predicted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expectation {
    pub pattern: String,
    /// Repo-relative paths of changed files containing the pattern.
    pub found_in: Vec<String>,
    pub found: bool,
    pub checked: bool,
}

const MAX_CHANGE_FILES: usize = 200;
const MAX_EXPECTATIONS: usize = 32;
const MAX_EXPECTATION_LEN: usize = 200;
const MAX_EXPECT_FILE_BYTES: u64 = 1 << 20;
const MAX_EXPECT_SCAN_BYTES: u64 = 8 << 20;

/// Bound the declared patterns: at most `MAX_EXPECTATIONS`, each truncated to
/// `MAX_EXPECTATION_LEN` chars. Empty patterns are dropped; skipped or
/// truncated input is reported as a warning so the bound is never silent.
fn bound_expectations(patterns: &[String]) -> (Vec<String>, Option<String>) {
    let mut bounded = Vec::new();
    let mut adjusted = false;
    for pattern in patterns {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            adjusted = true;
            continue;
        }
        if bounded.len() >= MAX_EXPECTATIONS {
            adjusted = true;
            continue;
        }
        let truncated: String = trimmed.chars().take(MAX_EXPECTATION_LEN).collect();
        if truncated.chars().count() != trimmed.chars().count() {
            adjusted = true;
        }
        bounded.push(truncated);
    }
    let note = adjusted.then(|| {
        format!(
            "expectations bounded to {MAX_EXPECTATIONS} patterns of at most \
             {MAX_EXPECTATION_LEN} chars each"
        )
    });
    (bounded, note)
}

/// Search the post-change content of `files` (repo-root relative, resolved
/// against `repo_root`) for each declared pattern. Reads are byte-capped per
/// file and total; a scan that hits the budget sets `truncated` so the caller
/// can say the evidence is partial.
fn check_expectations(
    repo_root: &Path,
    files: &[String],
    patterns: &[String],
) -> (Vec<Expectation>, bool) {
    use std::io::Read;
    let mut expectations: Vec<Expectation> = patterns
        .iter()
        .map(|pattern| Expectation {
            pattern: pattern.clone(),
            found_in: Vec::new(),
            found: false,
            checked: false,
        })
        .collect();
    let mut scanned = 0u64;
    let mut truncated = false;
    for file in files {
        if scanned >= MAX_EXPECT_SCAN_BYTES {
            truncated = true;
            break;
        }
        let path = repo_root.join(file);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(mut handle) = std::fs::File::open(&path) else {
            continue;
        };
        let mut buf = Vec::new();
        let _ = handle
            .by_ref()
            .take(MAX_EXPECT_FILE_BYTES)
            .read_to_end(&mut buf);
        scanned += buf.len() as u64;
        let content = String::from_utf8_lossy(&buf);
        for expectation in &mut expectations {
            if content.contains(expectation.pattern.as_str()) {
                expectation.found = true;
                expectation.found_in.push(file.clone());
            }
        }
    }
    for expectation in &mut expectations {
        expectation.checked = true;
    }
    (expectations, truncated)
}

/// `git status --porcelain` evidence for `workspace_root`, plus declared-vs-actual
/// comparisons: `declared` paths against the status list, and `expect` patterns
/// against the changed files' current content. Paths in `files` are
/// repository-root relative; `declared` entries are matched against both that
/// form and the workspace-relative form.
fn collect_changes(workspace_root: &Path, declared: &[String], expect: &[String]) -> ChangeReport {
    let (patterns, bound_note) = bound_expectations(expect);
    let inside = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output();
    let available = inside.as_ref().map(|o| o.status.success()).unwrap_or(false);
    if !available {
        let note = if inside.is_err() {
            "git is not available; change evidence unavailable".to_owned()
        } else {
            "workspace is not a git repository; change evidence unavailable".to_owned()
        };
        let mut report = ChangeReport {
            note: Some(note),
            ..ChangeReport::default()
        };
        if !declared.is_empty() {
            report
                .warnings
                .push("declared changes could not be checked without git evidence".to_owned());
        }
        if !patterns.is_empty() {
            report
                .warnings
                .push("expected content could not be checked without git evidence".to_owned());
            report.expectations = patterns
                .iter()
                .map(|pattern| Expectation {
                    pattern: pattern.clone(),
                    found_in: Vec::new(),
                    found: false,
                    checked: false,
                })
                .collect();
        }
        if let Some(bound_note) = bound_note {
            report.warnings.push(bound_note);
        }
        return report;
    }

    let prefix = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "--show-prefix"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();

    let status = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["status", "--porcelain"])
        .output();
    let (files, note) = match status {
        Err(_) => (
            Vec::new(),
            Some("git is not available; change evidence unavailable".to_owned()),
        ),
        Ok(o) if !o.status.success() => (
            Vec::new(),
            Some(format!(
                "git status failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
        ),
        Ok(o) => {
            let mut files = Vec::new();
            let mut truncated = false;
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if files.len() >= MAX_CHANGE_FILES {
                    truncated = true;
                    break;
                }
                // Porcelain v1: "XY <path>", renames as "XY old -> new", paths may be
                // C-quoted when they contain unusual bytes.
                let Some(rest) = line.get(3..) else {
                    continue;
                };
                let path = rest
                    .rsplit_once(" -> ")
                    .map(|(_, new)| new)
                    .unwrap_or(rest)
                    .trim()
                    .trim_matches('"');
                if !path.is_empty() {
                    files.push(path.to_owned());
                }
            }
            let note = truncated
                .then(|| format!("git status truncated to the first {MAX_CHANGE_FILES} entries"));
            (files, note)
        }
    };

    let mut warnings = Vec::new();
    for declared_path in declared {
        let normalized = normalize_declared_path(declared_path, workspace_root);
        let prefixed = format!("{prefix}{normalized}");
        let observed = files.iter().any(|f| f == &normalized || f == &prefixed);
        if !observed {
            warnings.push(format!(
                "declared change '{declared_path}' not observed in git status"
            ));
        }
    }

    let mut expectations = Vec::new();
    if !patterns.is_empty() {
        let repo_root = Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_owned()));
        match repo_root {
            Some(root) => {
                let (checked, truncated) = check_expectations(&root, &files, &patterns);
                for expectation in &checked {
                    if expectation.checked && !expectation.found {
                        warnings.push(format!(
                            "expected content '{}' not found in changed files",
                            expectation.pattern
                        ));
                    }
                }
                if truncated {
                    warnings.push(format!(
                        "expectation scan truncated after {MAX_EXPECT_SCAN_BYTES} bytes"
                    ));
                }
                expectations = checked;
            }
            None => {
                warnings
                    .push("expected content could not be checked: git rev-parse failed".to_owned());
                expectations = patterns
                    .iter()
                    .map(|pattern| Expectation {
                        pattern: pattern.clone(),
                        found_in: Vec::new(),
                        found: false,
                        checked: false,
                    })
                    .collect();
            }
        }
    }
    if let Some(bound_note) = bound_note {
        warnings.push(bound_note);
    }
    ChangeReport {
        files,
        note,
        warnings,
        expectations,
    }
}

/// Normalize a caller-declared path for comparison with porcelain output: absolute
/// paths under the workspace become workspace-relative, separators unify on '/', and
/// leading "./" is dropped.
fn normalize_declared_path(declared: &str, workspace_root: &Path) -> String {
    let trimmed = declared.trim();
    let mut normalized = Path::new(trimmed)
        .strip_prefix(workspace_root)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| trimmed.replace('\\', "/"));
    while let Some(rest) = normalized.strip_prefix("./") {
        normalized = rest.to_owned();
    }
    normalized
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
    let output = match wait_with_timeout(&mut cmd, step.timeout, None) {
        Ok(WaitOutcome::Finished(output)) => output,
        Ok(WaitOutcome::TimedOut(partial)) => {
            timed_out = true;
            let mut stderr = partial.stderr;
            if !stderr.is_empty() && !stderr.ends_with(b"\n") {
                stderr.push(b'\n');
            }
            stderr.extend_from_slice(b"verification step timed out");
            std::process::Output {
                status: partial.status,
                stdout: partial.stdout,
                stderr,
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

/// What a waited-on command came to.
///
/// `Finished` is a command that exited on its own; `TimedOut` is one killed at the deadline,
/// carrying whatever its pipes had produced by then. The distinction stays explicit so a caller
/// never mistakes a killed command for a complete one — and never throws away output it already
/// paid for waiting on.
#[derive(Debug)]
pub(crate) enum WaitOutcome {
    Finished(std::process::Output),
    TimedOut(std::process::Output),
}

/// Run `cmd` under `timeout`, capturing stdout and stderr.
///
/// `Finished` is a command that exited, `TimedOut` is a deadline kill with the partial output
/// its pipes had delivered, and `Err` is a spawn or wait failure — the three must stay distinct
/// so a command that never started is never reported as having timed out. `cancel`, when
/// present, is polled on the same cadence as the deadline: a cancelled call kills the process
/// tree and reports `cancelled`, which is how a tool honouring `ToolContext::cancel` actually
/// stops the command it spawned rather than only abandoning the wait. On Unix the child leads
/// its own process group, so a timeout or cancel kills the whole group: a backgrounded
/// grandchild cannot outlive the call.
///
/// The pipes are drained from the first moment the child exists, on detached reader threads
/// that forward bytes as they arrive. A child that has written more than the pipe buffer blocks
/// on its next write until someone drains — reading only after exit would deadlock exactly the
/// commands that produce the most output, which are the builds and logs this exists to read.
/// The readers are never joined: a surviving grandchild holding the write end would wedge the
/// join exactly as it once wedged the wait, so collection runs under a deadline instead.
pub(crate) fn wait_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
    cancel: Option<&vikram_contracts::CancellationToken>,
) -> Result<WaitOutcome, String> {
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

    let stdout = PipeReader::start(child.stdout.take());
    let stderr = PipeReader::start(child.stderr.take());
    let mut out_buffer = Vec::new();
    let mut err_buffer = Vec::new();

    let deadline = Instant::now() + timeout;
    let status = loop {
        stdout.drain(&mut out_buffer);
        stderr.drain(&mut err_buffer);
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let status = kill_tree(&mut child);
                    // SIGKILL closes the group's pipe write ends, so EOF — and with it the rest
                    // of the stream — arrives quickly; the deadline is for a grandchild that
                    // escaped the group and still holds the pipe open.
                    let collect_by = Instant::now() + DRAIN_AFTER_KILL;
                    stdout.collect(&mut out_buffer, collect_by);
                    stderr.collect(&mut err_buffer, collect_by);
                    return Ok(WaitOutcome::TimedOut(std::process::Output {
                        status: status.unwrap_or_else(|_| exit_status_failure()),
                        stdout: out_buffer,
                        stderr: err_buffer,
                    }));
                }
                if cancel.is_some_and(|token| token.is_cancelled()) {
                    let _ = kill_tree(&mut child);
                    return Err("cancelled".to_owned());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = kill_tree(&mut child);
                return Err(format!("failed while waiting: {e}"));
            }
        }
    };
    let collect_by = Instant::now() + DRAIN_AFTER_EXIT;
    stdout.collect(&mut out_buffer, collect_by);
    stderr.collect(&mut err_buffer, collect_by);
    Ok(WaitOutcome::Finished(std::process::Output {
        status,
        stdout: out_buffer,
        stderr: err_buffer,
    }))
}

/// How long a finished or killed child's pipes may take to reach EOF. The common case is
/// immediate; the cap exists for the one that is not — a grandchild that escaped the process
/// group and still holds the write end open.
const DRAIN_AFTER_KILL: Duration = Duration::from_secs(1);
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(1);

/// One pipe, read on a detached thread that forwards bytes as they arrive.
///
/// The channel closing is the only end-of-output signal a pipe has. The reader is never joined:
/// it exits when the write end closes, whenever that is, and the caller collects under a
/// deadline instead of waiting for that to happen.
struct PipeReader {
    chunks: std::sync::mpsc::Receiver<Vec<u8>>,
}

impl PipeReader {
    fn start<T: std::io::Read + Send + 'static>(pipe: Option<T>) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        match pipe {
            Some(mut pipe) => {
                std::thread::spawn(move || {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match pipe.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => {
                                // A dropped receiver means nobody is collecting; stop reading
                                // rather than fill a pipe nobody will drain.
                                if sender.send(chunk[..read].to_vec()).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
            // No pipe: a closed channel reads as immediate EOF.
            None => drop(sender),
        }
        Self { chunks: receiver }
    }

    fn drain(&self, buffer: &mut Vec<u8>) {
        while let Ok(chunk) = self.chunks.try_recv() {
            buffer.extend_from_slice(&chunk);
        }
    }

    fn collect(&self, buffer: &mut Vec<u8>, deadline: Instant) {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            match self.chunks.recv_timeout(remaining) {
                Ok(chunk) => buffer.extend_from_slice(&chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// Kill the child and, on Unix, its whole process group. The child was spawned
/// with `process_group(0)`, so its pid is also the pgid and a negative pid
/// addresses the group — `child.kill()` alone would leave grandchildren running.
/// The wait is what harvests the exit status a `TimedOut` outcome reports.
fn kill_tree(child: &mut std::process::Child) -> std::io::Result<std::process::ExitStatus> {
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
    child.wait()
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
/// impossible: the loop stops at the first fully green attempt. `expected_changes` declares
/// the paths the caller believes were modified; `expect` declares content patterns the
/// caller expects to find in the changed files. The outcome's `changes` report compares
/// both declarations against git evidence as warnings only — neither affects `ok`.
pub fn verify(
    workspace_root: &Path,
    plan: &VerificationPlan,
    budget: &Budget,
    sandbox: &Sandbox,
    expected_changes: &[String],
    expect: &[String],
    mut repair: impl FnMut(&str, &str) -> Option<Vec<PathBuf>>,
) -> Result<VerificationOutcome, VikramError> {
    let mut outcome = verify_attempts(workspace_root, plan, budget, sandbox, &mut repair)?;
    outcome.changes = collect_changes(workspace_root, expected_changes, expect);
    Ok(outcome)
}

fn verify_attempts(
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
                changes: ChangeReport::default(),
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
                changes: ChangeReport::default(),
            });
        }
        let Some(edits) = repair(&name, &spent_out) else {
            return Ok(VerificationOutcome {
                ok: false,
                attempts: attempt,
                failure: Some((name, kind)),
                steps: all_steps,
                changes: ChangeReport::default(),
            });
        };
        if edits.is_empty() {
            // A repair that touched nothing cannot change the next result.
            return Ok(VerificationOutcome {
                ok: false,
                attempts: attempt,
                failure: Some((name, kind)),
                steps: all_steps,
                changes: ChangeReport::default(),
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
            &[],
            &[],
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
            &[],
            &[],
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
            &[],
            &[],
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
            &[],
            &[],
            |_, _| Some(Vec::new()),
        )
        .unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.attempts, 1, "no-change repair must not burn budget");
    }

    #[test]
    fn a_cancelled_wait_kills_the_process_instead_of_timing_out() {
        let dir = workspace_with("cancel", "");
        let marker = dir.join("survived");
        let token = vikram_contracts::CancellationToken::new();
        let cancel = token.clone();
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            cancel.cancel();
        });
        // The command would write the marker two seconds in; a live process means the cancel
        // only abandoned the wait, a dead one means the tree was actually killed.
        let mut cmd = shell_cmd(&format!("sleep 2; touch {}", marker.display()));
        let started = Instant::now();
        let result = wait_with_timeout(&mut cmd, Duration::from_secs(60), Some(&token));
        killer.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cancel must not wait out the timeout"
        );
        assert_eq!(result.unwrap_err(), "cancelled");
        std::thread::sleep(Duration::from_millis(2100));
        assert!(
            !marker.exists(),
            "the cancelled command's process tree was killed"
        );
    }

    #[test]
    fn a_command_emitting_more_than_the_pipe_buffer_completes() {
        let mut cmd = shell_cmd("yes x | head -c 70000");
        let started = Instant::now();
        let result = wait_with_timeout(&mut cmd, Duration::from_secs(10), None);
        let Ok(WaitOutcome::Finished(output)) = result else {
            panic!("70000 bytes is one pipe buffer over; it must complete, not deadlock");
        };
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the command itself runs in milliseconds"
        );
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            output.stdout.len(),
            70_000,
            "the whole output is delivered, not just what fit the pipe"
        );
    }

    #[test]
    fn a_command_emitting_200k_completes() {
        let mut cmd = shell_cmd("yes x | head -c 200000");
        let result = wait_with_timeout(&mut cmd, Duration::from_secs(10), None);
        let Ok(WaitOutcome::Finished(output)) = result else {
            panic!("200000 bytes must complete, not deadlock the wait");
        };
        assert_eq!(output.stdout.len(), 200_000);
    }

    #[test]
    fn a_genuinely_hung_command_times_out_with_its_partial_output() {
        let mut cmd = shell_cmd("echo started; sleep 5");
        let started = Instant::now();
        let result = wait_with_timeout(&mut cmd, Duration::from_millis(250), None);
        let Ok(WaitOutcome::TimedOut(partial)) = result else {
            panic!("a sleeping command must be reported as a timeout");
        };
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline is honored"
        );
        let text = String::from_utf8_lossy(&partial.stdout);
        assert!(
            text.contains("started"),
            "output produced before the deadline survives the kill: {text:?}"
        );
    }

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// A committed git repo fixture; `None` when git is unavailable so the
    /// change-evidence tests skip rather than fail on git-less hosts.
    fn git_repo(name: &str) -> Option<PathBuf> {
        let dir = workspace_with(name, "");
        if !git(&dir, &["init", "-q"]) {
            return None;
        }
        git(&dir, &["config", "user.email", "verify@test"]);
        git(&dir, &["config", "user.name", "verify"]);
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);
        Some(dir)
    }

    #[test]
    fn change_evidence_lists_what_git_reports() {
        let Some(dir) = git_repo("changes-listed") else {
            return;
        };
        std::fs::write(dir.join("a.txt"), "two").unwrap();
        std::fs::write(dir.join("b.txt"), "new").unwrap();
        let report = collect_changes(&dir, &[], &[]);
        assert!(report.files.contains(&"a.txt".to_owned()), "{report:?}");
        assert!(report.files.contains(&"b.txt".to_owned()), "{report:?}");
        assert!(report.note.is_none(), "{report:?}");
        assert!(report.warnings.is_empty(), "{report:?}");
    }

    #[test]
    fn a_non_git_workspace_reports_the_limitation_not_a_crash() {
        let dir = workspace_with("changes-no-git", "");
        let report = collect_changes(&dir, &["anything.rs".to_owned()], &["x".to_owned()]);
        assert!(report.files.is_empty(), "{report:?}");
        assert!(
            report
                .note
                .as_deref()
                .is_some_and(|n| n.contains("not a git repository")),
            "{report:?}"
        );
        assert_eq!(report.warnings.len(), 2, "{report:?}");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("declared changes could not be checked")),
            "{report:?}"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("expected content could not be checked")),
            "{report:?}"
        );
        assert!(!report.expectations[0].checked, "{report:?}");
    }

    #[test]
    fn declared_but_unobserved_changes_warn_without_failing() {
        let Some(dir) = git_repo("changes-declared") else {
            return;
        };
        std::fs::write(dir.join("a.txt"), "changed").unwrap();
        let plan = VerificationPlan {
            steps: vec![PlanStep::new("true", "true", Duration::from_secs(5))],
        };
        let declared = vec!["a.txt".to_owned(), "ghost.txt".to_owned()];
        let outcome = verify(
            &dir,
            &plan,
            &Budget::attempts(1),
            &Sandbox::disabled("test"),
            &declared,
            &[],
            |_, _| None,
        )
        .unwrap();
        assert!(
            outcome.ok,
            "warnings must not affect the verdict: {outcome:?}"
        );
        assert!(
            outcome.changes.files.contains(&"a.txt".to_owned()),
            "{outcome:?}"
        );
        assert_eq!(outcome.changes.warnings.len(), 1, "{outcome:?}");
        assert!(
            outcome.changes.warnings[0].contains("ghost.txt"),
            "{outcome:?}"
        );
    }

    #[test]
    fn expect_patterns_are_checked_against_changed_files() {
        let Some(dir) = git_repo("expect-found") else {
            return;
        };
        std::fs::write(dir.join("a.txt"), "fn handle_poll() {}\nPOLL_MARKER\n").unwrap();
        std::fs::write(dir.join("b.txt"), "unrelated\n").unwrap();
        let report = collect_changes(
            &dir,
            &[],
            &[
                "handle_poll".to_owned(),
                "POLL_MARKER".to_owned(),
                "ABSENT_SYMBOL".to_owned(),
            ],
        );
        assert_eq!(report.expectations.len(), 3, "{report:?}");
        let found = &report.expectations[0];
        assert!(found.found && found.checked, "{found:?}");
        assert_eq!(found.found_in, vec!["a.txt".to_owned()]);
        assert!(report.expectations[1].found, "{report:?}");
        let missing = &report.expectations[2];
        assert!(!missing.found && missing.checked, "{missing:?}");
        assert!(missing.found_in.is_empty(), "{missing:?}");
        assert!(
            report.warnings.iter().any(|w| w.contains("ABSENT_SYMBOL")),
            "missing expectation must warn: {report:?}"
        );
        // Matching is case-sensitive: a differently-cased pattern is absent.
        let report = collect_changes(&dir, &[], &["poll_marker".to_owned()]);
        assert!(!report.expectations[0].found, "{report:?}");
    }

    #[test]
    fn expect_patterns_only_search_changed_files() {
        let Some(dir) = git_repo("expect-scope") else {
            return;
        };
        // Committed content holds the needle; the change does not touch it.
        std::fs::write(dir.join("needle.txt"), "NEEDLE").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "needle"]);
        std::fs::write(dir.join("other.txt"), "no needle here").unwrap();
        let report = collect_changes(&dir, &[], &["NEEDLE".to_owned()]);
        assert!(!report.expectations[0].found, "{report:?}");
        assert!(
            report.expectations[0].found_in.is_empty(),
            "unchanged files must not satisfy expectations: {report:?}"
        );
    }

    #[test]
    fn expect_on_a_non_git_workspace_is_unchecked_not_missing() {
        let dir = workspace_with("expect-no-git", "");
        std::fs::write(dir.join("a.txt"), "handle_poll\n").unwrap();
        let report = collect_changes(&dir, &[], &["handle_poll".to_owned()]);
        assert_eq!(report.expectations.len(), 1, "{report:?}");
        assert!(!report.expectations[0].checked, "{report:?}");
        assert!(!report.expectations[0].found, "{report:?}");
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("not found in changed files")),
            "an unchecked expectation must not claim absence: {report:?}"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("could not be checked")),
            "{report:?}"
        );
    }

    #[test]
    fn expect_patterns_are_bounded() {
        let (patterns, note) = bound_expectations(&[
            "ok".to_owned(),
            "   ".to_owned(),
            "x".repeat(MAX_EXPECTATION_LEN + 50),
        ]);
        assert_eq!(patterns.len(), 2, "empty dropped, long kept truncated");
        assert_eq!(
            patterns[1].chars().count(),
            MAX_EXPECTATION_LEN,
            "over-length pattern truncated"
        );
        assert!(note.is_some(), "adjustment must be reported: {note:?}");

        let many: Vec<String> = (0..MAX_EXPECTATIONS + 10)
            .map(|i| format!("p{i}"))
            .collect();
        let (patterns, note) = bound_expectations(&many);
        assert_eq!(patterns.len(), MAX_EXPECTATIONS);
        assert!(note.is_some());

        let (patterns, note) = bound_expectations(&["clean".to_owned()]);
        assert_eq!(patterns, vec!["clean".to_owned()]);
        assert!(note.is_none());
    }

    #[test]
    fn expect_warnings_never_affect_the_verdict() {
        let Some(dir) = git_repo("expect-verdict") else {
            return;
        };
        std::fs::write(dir.join("a.txt"), "changed").unwrap();
        let plan = VerificationPlan {
            steps: vec![PlanStep::new("true", "true", Duration::from_secs(5))],
        };
        let outcome = verify(
            &dir,
            &plan,
            &Budget::attempts(1),
            &Sandbox::disabled("test"),
            &[],
            &["DEFINITELY_ABSENT".to_owned()],
            |_, _| None,
        )
        .unwrap();
        assert!(
            outcome.ok,
            "a missing expectation warns, it does not fail: {outcome:?}"
        );
        assert_eq!(outcome.changes.expectations.len(), 1, "{outcome:?}");
        assert!(!outcome.changes.expectations[0].found);
        assert!(
            outcome
                .changes
                .warnings
                .iter()
                .any(|w| w.contains("DEFINITELY_ABSENT")),
            "{outcome:?}"
        );
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
