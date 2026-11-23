use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;

/// `nvidia-smi` is a probe, not work: a hung driver must not hold the turn.
const GPU_TIMEOUT: Duration = Duration::from_secs(3);

const MAX_GPUS: usize = 8;

/// A resident page is 4096 bytes on every architecture this runtime targets (x86_64, aarch64),
/// and `/proc/<pid>/statm` reports the resident count in pages.
const PAGE_BYTES: u64 = 4096;

const MAX_PROCESSES: usize = 5;

const GPU_MISSING: &str = "gpu: no GPU tool was found (nvidia-smi is not installed)";

pub(crate) struct SystemStatus;

impl Tool for SystemStatus {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "system.status".into(),
            description: "Report how this machine is doing: load average, memory used, free disk \
                          for the workspace filesystem, GPU utilization, the busiest resident \
                          processes, and the last stored self-check verdict."
                .into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "system.status",
            "host",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let database = crate::config::vikram_home().join("evolution.db");
        Ok(ToolOutput {
            content: report(&ctx.workspace_root, &database),
            ok: true,
        })
    }
}

fn report(workspace: &Path, database: &Path) -> String {
    report_with(workspace, database, probe_gpu())
}

fn report_with(workspace: &Path, database: &Path, gpus: Option<Vec<String>>) -> String {
    truncated(
        [
            load_line(),
            memory_line(),
            disk_line(workspace),
            gpu_section(gpus),
            processes_section(),
            self_check_line(database),
        ]
        .join("\n"),
    )
}

/// One line per GPU under a `gpu:` heading, or the honest sentence that no tool answered. A
/// machine without a GPU and one whose driver is broken read the same: this is a status line,
/// not a diagnosis, so it never invents a card and never fails the report.
fn gpu_section(gpus: Option<Vec<String>>) -> String {
    let rows: Vec<String> = gpus
        .unwrap_or_default()
        .into_iter()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .take(MAX_GPUS)
        .collect();
    if rows.is_empty() {
        return GPU_MISSING.to_owned();
    }
    let mut body = String::from("gpu:");
    for row in rows {
        body.push_str("\n  ");
        body.push_str(&row);
    }
    body
}

/// `name, utilization.gpu, memory.used, memory.total` per card, straight from `nvidia-smi`.
/// `None` covers every way the probe can decline to answer: absent binary, non-zero exit, or a
/// timeout.
fn probe_gpu() -> Option<Vec<String>> {
    let mut command = std::process::Command::new("nvidia-smi");
    command.args([
        "--query-gpu=name,utilization.gpu,memory.used,memory.total",
        "--format=csv,noheader",
    ]);
    match crate::verification::wait_with_timeout(&mut command, GPU_TIMEOUT, None) {
        Ok(crate::verification::WaitOutcome::Finished(output)) if output.status.success() => Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_owned)
                .collect(),
        ),
        _ => None,
    }
}

fn processes_section() -> String {
    let rows = busiest(process_entries(Path::new("/proc")));
    if rows.is_empty() {
        return "busiest processes: unavailable (no readable /proc on this platform)".to_owned();
    }
    let mut body = String::from("busiest processes:");
    for row in rows {
        body.push('\n');
        body.push_str(&row);
    }
    body
}

fn process_entries(root: &Path) -> Vec<(String, u64, u64)> {
    let Ok(directory) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    directory
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid: u64 = entry.file_name().to_str()?.parse().ok()?;
            let comm = std::fs::read_to_string(entry.path().join("comm")).ok()?;
            let statm = std::fs::read_to_string(entry.path().join("statm")).ok()?;
            let resident: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
            Some((
                comm.trim().to_owned(),
                pid,
                resident.saturating_mul(PAGE_BYTES),
            ))
        })
        .collect()
}

fn busiest(entries: Vec<(String, u64, u64)>) -> Vec<String> {
    let mut entries = entries;
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.2));
    entries
        .into_iter()
        .take(MAX_PROCESSES)
        .map(|(comm, pid, resident)| {
            format!(
                "  {comm} (pid {pid})  {:.1} MiB",
                resident as f64 / (1024.0 * 1024.0)
            )
        })
        .collect()
}

fn load_line() -> String {
    let Ok(text) = std::fs::read_to_string("/proc/loadavg") else {
        return "load average: unavailable (no /proc/loadavg on this platform)".to_owned();
    };
    let mut fields = text.split_whitespace();
    let one = fields.next().and_then(|value| value.parse::<f64>().ok());
    let five = fields.next().and_then(|value| value.parse::<f64>().ok());
    let fifteen = fields.next().and_then(|value| value.parse::<f64>().ok());
    let (Some(one), Some(five), Some(fifteen)) = (one, five, fifteen) else {
        return "load average: /proc/loadavg could not be read as three numbers".to_owned();
    };
    let (runnable, total) = match fields.next().and_then(|value| value.split_once('/')) {
        Some((runnable, total)) => (runnable.to_owned(), total.to_owned()),
        None => ("?".to_owned(), "?".to_owned()),
    };
    let cpus = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    format!(
        "load average: {one:.2} {five:.2} {fifteen:.2} (1m, 5m, 15m; {cpus} CPUs; {runnable} \
         runnable, {total} total)"
    )
}

fn memory_line() -> String {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return "memory: unavailable (no /proc/meminfo on this platform)".to_owned();
    };
    let total = meminfo_kb(&text, "MemTotal:");
    let available = meminfo_kb(&text, "MemAvailable:").or_else(|| {
        let free = meminfo_kb(&text, "MemFree:")?;
        let buffers = meminfo_kb(&text, "Buffers:").unwrap_or(0);
        let cached = meminfo_kb(&text, "Cached:").unwrap_or(0);
        Some(free + buffers + cached)
    });
    match (total, available) {
        (Some(total), Some(available)) if total > 0 => {
            let used = total.saturating_sub(available);
            format!(
                "memory: {:.2} GiB used / {:.2} GiB total ({:.1}% used)",
                gib(used),
                gib(total),
                percent(gib(used), gib(total))
            )
        }
        _ => "memory: /proc/meminfo has no usable MemTotal/MemAvailable pair".to_owned(),
    }
}

fn meminfo_kb(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
    })
}

fn disk_line(workspace: &Path) -> String {
    match disk_space(workspace) {
        Some(space) => format!(
            "disk {}: {:.2} GiB free / {:.2} GiB total ({:.1}% free)",
            workspace.display(),
            gib_bytes(space.free),
            gib_bytes(space.total),
            percent(space.free, space.total)
        ),
        None => format!("disk {}: unavailable", workspace.display()),
    }
}

struct DiskSpace {
    free: f64,
    total: f64,
}

#[cfg(unix)]
fn disk_space(path: &Path) -> Option<DiskSpace> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` writes into a struct this function owns, and the path is NUL-terminated.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    let block = stats.f_frsize.max(stats.f_bsize).max(1) as f64;
    let total = block * stats.f_blocks as f64;
    let free = block * stats.f_bavail as f64;
    (total > 0.0).then_some(DiskSpace { free, total })
}

#[cfg(not(unix))]
fn disk_space(_path: &Path) -> Option<DiskSpace> {
    None
}

fn self_check_line(database: &Path) -> String {
    match last_self_check(database) {
        Some((millis, summary)) => {
            format!("self-check: {summary} (recorded {} s epoch)", millis / 1000)
        }
        None => "self-check: no verdict stored".to_owned(),
    }
}

/// The last verdict the evolution self-check journaled. It lives in the evolution store's
/// `learning_events` table; opening it read-only keeps this probe from migrating or writing a
/// database the evolution engine owns, and any missing table or file is simply "no verdict".
fn last_self_check(database: &Path) -> Option<(i64, String)> {
    let connection =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let mut statement = connection
        .prepare(
            "SELECT timestamp_ms, summary FROM learning_events \
             WHERE kind = 'self_check' ORDER BY timestamp_ms DESC LIMIT 1",
        )
        .ok()?;
    statement
        .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
        .ok()
}

fn gib(kilobytes: u64) -> f64 {
    kilobytes as f64 / (1024.0 * 1024.0)
}

fn gib_bytes(bytes: f64) -> f64 {
    bytes / (1024.0 * 1024.0 * 1024.0)
}

fn percent(part: f64, total: f64) -> f64 {
    if total == 0.0 {
        return 0.0;
    }
    part / total * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_names_load_memory_disk_and_self_check() {
        let directory = tempfile::tempdir().unwrap();
        let text = report(directory.path(), &directory.path().join("absent.db"));
        for prefix in ["load average:", "memory:", "disk ", "self-check:"] {
            assert!(
                text.lines().any(|line| line.starts_with(prefix)),
                "missing `{prefix}` in:\n{text}"
            );
        }
        assert!(text.contains("no verdict stored"), "{text}");
        assert!(
            text.contains(&directory.path().display().to_string()),
            "{text}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn every_number_in_the_report_parses() {
        let directory = tempfile::tempdir().unwrap();
        let text = report(Path::new("/"), &directory.path().join("absent.db"));
        for line in text.lines() {
            for token in
                line.split(|character: char| !(character.is_ascii_digit() || character == '.'))
            {
                if token.trim_matches('.').is_empty() {
                    continue;
                }
                assert!(
                    token.parse::<f64>().is_ok(),
                    "`{token}` in `{line}` is not a number"
                );
            }
        }
        assert!(text.contains("runnable"), "{text}");
        assert!(text.contains("GiB"), "{text}");
    }

    #[test]
    fn a_missing_disk_path_degrades_instead_of_failing() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("gone");
        let text = report(&missing, &directory.path().join("absent.db"));
        assert!(
            text.lines()
                .any(|line| line.starts_with("disk ") && line.ends_with(": unavailable")),
            "{text}"
        );
    }

    #[test]
    fn a_stored_verdict_is_read_from_the_evolution_journal() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("evolution.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE learning_events (id TEXT, timestamp_ms INTEGER, agent_id TEXT, \
                 kind TEXT, summary TEXT, payload TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO learning_events VALUES ('a', 1700000000000, 'self-check', \
                 'self_check', 'self-check: health Healthy, 0 issue(s)', '{}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO learning_events VALUES ('b', 1600000000000, 'self-check', \
                 'self_check', 'self-check: health Critical', '{}')",
                [],
            )
            .unwrap();
        drop(connection);

        let line = self_check_line(&database);
        assert!(line.contains("health Healthy"), "{line}");
        assert!(
            !line.contains("Critical"),
            "the newest verdict wins: {line}"
        );
        assert!(line.contains("recorded 1700000000 s epoch"), "{line}");
    }

    #[test]
    fn a_database_without_the_journal_says_no_verdict_stored() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("empty.db");
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated (x INTEGER);")
            .unwrap();
        assert_eq!(self_check_line(&database), "self-check: no verdict stored");
    }

    #[test]
    fn the_busiest_processes_are_listed_by_resident_memory() {
        let entries = vec![
            ("idle".to_owned(), 10, 1024 * 1024),
            ("biggest".to_owned(), 11, 512 * 1024 * 1024),
            ("third".to_owned(), 12, 64 * 1024 * 1024),
            ("second".to_owned(), 13, 256 * 1024 * 1024),
            ("fourth".to_owned(), 14, 32 * 1024 * 1024),
            ("fifth".to_owned(), 15, 16 * 1024 * 1024),
            ("sixth".to_owned(), 16, 8 * 1024 * 1024),
        ];
        let rows = busiest(entries);
        assert_eq!(rows.len(), MAX_PROCESSES, "{rows:?}");
        let order: Vec<&str> = rows
            .iter()
            .map(|row| row.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(order, ["biggest", "second", "third", "fourth", "fifth"]);
        assert_eq!(rows[0], "  biggest (pid 11)  512.0 MiB");
    }

    #[test]
    fn a_missing_gpu_tool_says_so_instead_of_failing() {
        let directory = tempfile::tempdir().unwrap();
        let text = report_with(directory.path(), &directory.path().join("absent.db"), None);
        assert!(
            text.lines()
                .any(|line| line == "gpu: no GPU tool was found (nvidia-smi is not installed)"),
            "{text}"
        );
        assert_eq!(gpu_section(None), GPU_MISSING);
        assert_eq!(gpu_section(Some(vec!["  ".to_owned()])), GPU_MISSING);
        assert_eq!(
            gpu_section(Some(vec![
                "NVIDIA RTX 4090, 12 %, 2048 MiB, 24564 MiB".to_owned()
            ])),
            "gpu:\n  NVIDIA RTX 4090, 12 %, 2048 MiB, 24564 MiB"
        );
    }

    #[test]
    fn the_tool_is_registered_and_advertises_its_schema() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry
            .get("system.status")
            .expect("system.status registered");
        assert_eq!(tool.spec().name, "system.status");
        assert_eq!(tool.spec().schema["type"], "object");
        assert!(registry.names().contains(&"system.status".to_owned()));
    }

    #[test]
    fn the_permission_is_a_read_with_no_side_effect() {
        let request = SystemStatus.permission(&json!({})).unwrap();
        assert_eq!(request.action, ActionKind::Read);
        assert_eq!(request.side_effect, SideEffect::None);
        assert_eq!(request.verb, "system.status");
    }
}
