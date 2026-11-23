use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;

const DEFAULT_SINCE_MINUTES: u64 = 60;
const MAX_SINCE_MINUTES: u64 = 1_440;
/// Reading a log is a tail problem: the newest entries are the ones that matter, so a file larger
/// than this is read from its end instead of refused.
const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_JOURNAL_LINES: usize = 50_000;
const MAX_LISTED_SOURCES: usize = 20;
const JOURNAL_TIMEOUT: Duration = Duration::from_secs(20);

/// The whole vocabulary: a line is a failed login when it contains one of these, and nothing else
/// counts. Each pair is (label shown to the reader, lowercase needle matched in the line).
const SHAPES: &[(&str, &str)] = &[
    ("Failed password", "failed password"),
    ("authentication failure", "authentication failure"),
    ("Invalid user", "invalid user"),
    ("Failed publickey", "failed publickey"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Journald,
    File,
}

pub(crate) struct SystemThreats;

impl Tool for SystemThreats {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "system.threats".into(),
            description: "Count failed authentication lines in journald or a workspace log file \
                          and group them by source address. This counts failed logins and nothing \
                          else: it reads no packets, watches no processes, and does not detect \
                          threats."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "since_minutes": {
                        "type": "integer",
                        "description": "Window to count over, in minutes. Default 60, maximum 1440."
                    },
                    "source": {
                        "type": "string",
                        "enum": ["journald", "file"],
                        "description": "Where to read from; inferred as `file` when `path` is given."
                    },
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative log file to read when source=file."
                    }
                }
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "system.threats",
            resource_of(args),
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let since_minutes = window_minutes(args);
        let source = match source_of(args) {
            Ok(source) => source,
            Err(message) => return Ok(refused(message)),
        };
        let (reading, window) = match source {
            Source::Journald => (
                read_journald(since_minutes, ctx),
                format!("the last {since_minutes} minute(s), applied by `journalctl --since`",),
            ),
            Source::File => {
                let Some(raw) = args.get("path").and_then(Value::as_str) else {
                    return Ok(refused(
                        "source=file needs a `path` naming the log file; nothing was read"
                            .to_owned(),
                    ));
                };
                let (path, display) = canonical_resource(&ctx.workspace_root, raw)?;
                (
                    read_file(&path, &display),
                    format!(
                        "none — `{display}` was counted in full, its timestamps were not parsed, \
                         so the requested {since_minutes} minute(s) could not be applied"
                    ),
                )
            }
        };
        Ok(match reading {
            Reading::Read(log) => ToolOutput {
                content: truncated(report(&log, &window)),
                ok: true,
            },
            Reading::Unavailable(reason) => no_claim(&reason),
        })
    }
}

fn refused(content: String) -> ToolOutput {
    ToolOutput { content, ok: false }
}

/// The honest answer when nothing could be read: a success that says so, so a caller never reads
/// an empty report as "no threats".
fn no_claim(reason: &str) -> ToolOutput {
    ToolOutput {
        content: truncated(format!(
            "system.threats: no authentication log was readable; nothing is claimed\n\
             reason: {reason}\n\
             this is not a report of zero failed logins — the log was not read"
        )),
        ok: true,
    }
}

fn window_minutes(args: &Value) -> u64 {
    args.get("since_minutes")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_SINCE_MINUTES)
        .clamp(1, MAX_SINCE_MINUTES)
}

fn source_of(args: &Value) -> Result<Source, String> {
    match args.get("source").and_then(Value::as_str) {
        Some("journald") => Ok(Source::Journald),
        Some("file") => Ok(Source::File),
        Some(other) => Err(format!(
            "`{other}` is not a system.threats source: use `journald` or `file`"
        )),
        None if args.get("path").is_some() => Ok(Source::File),
        None => Ok(Source::Journald),
    }
}

fn resource_of(args: &Value) -> String {
    match source_of(args) {
        Ok(Source::File) => args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("file")
            .to_owned(),
        Ok(Source::Journald) => "journald".to_owned(),
        Err(_) => args
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("system.threats")
            .to_owned(),
    }
}

struct ReadLog {
    lines: Vec<String>,
    label: String,
    notes: Vec<String>,
}

enum Reading {
    Read(ReadLog),
    Unavailable(String),
}

fn read_journald(since_minutes: u64, ctx: &ToolContext) -> Reading {
    if crate::computer::which("journalctl").is_none() {
        return Reading::Unavailable(
            "journalctl is not installed on this machine, so journald cannot be read".to_owned(),
        );
    }
    let mut command = std::process::Command::new("journalctl");
    command
        .arg("--since")
        .arg(format!("-{since_minutes} min"))
        .args(["--no-pager", "--output", "short-iso", "--lines"])
        .arg(MAX_JOURNAL_LINES.to_string());
    ctx.sandbox.wrap_command(&mut command, &ctx.workspace_root);
    match crate::verification::wait_with_timeout(&mut command, JOURNAL_TIMEOUT, ctx.cancel.as_ref())
    {
        Ok(crate::verification::WaitOutcome::Finished(output)) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<String> = text.lines().map(str::to_owned).collect();
            let mut notes = Vec::new();
            if lines.len() >= MAX_JOURNAL_LINES {
                notes.push(format!(
                    "journalctl returned its {MAX_JOURNAL_LINES}-line cap; older entries inside \
                     the window were not counted"
                ));
            }
            Reading::Read(ReadLog {
                lines,
                label: "journald".to_owned(),
                notes,
            })
        }
        Ok(crate::verification::WaitOutcome::Finished(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Reading::Unavailable(format!(
                "journald could not be read: journalctl exited {} ({})",
                output.status.code().unwrap_or(-1),
                first_line(&stderr)
            ))
        }
        Ok(crate::verification::WaitOutcome::TimedOut(_)) => Reading::Unavailable(format!(
            "journald could not be read: journalctl did not answer within {}s",
            JOURNAL_TIMEOUT.as_secs()
        )),
        Err(error) => Reading::Unavailable(format!("journald could not be read: {error}")),
    }
}

fn read_file(path: &Path, display: &str) -> Reading {
    use std::io::{Read, Seek, SeekFrom};

    let length = match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => meta.len(),
        Ok(_) => return Reading::Unavailable(format!("`{display}` is not a file")),
        Err(error) => return Reading::Unavailable(format!("cannot read `{display}`: {error}")),
    };
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return Reading::Unavailable(format!("cannot read `{display}`: {error}")),
    };
    let tail_only = length > MAX_LOG_BYTES;
    if tail_only && let Err(error) = file.seek(SeekFrom::Start(length - MAX_LOG_BYTES)) {
        return Reading::Unavailable(format!("cannot read `{display}`: {error}"));
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.read_to_end(&mut bytes) {
        return Reading::Unavailable(format!("cannot read `{display}`: {error}"));
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut notes = Vec::new();
    if tail_only {
        // Seeking into the middle of a file lands mid-line, and that fragment is not a log entry.
        if !lines.is_empty() {
            lines.remove(0);
        }
        notes.push(format!(
            "`{display}` is {length} bytes; only its last {MAX_LOG_BYTES} bytes were read"
        ));
    }
    Reading::Read(ReadLog {
        lines,
        label: format!("file `{display}`"),
        notes,
    })
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 200 {
        format!("{}…", line.chars().take(200).collect::<String>())
    } else if line.is_empty() {
        "no stderr".to_owned()
    } else {
        line.to_owned()
    }
}

#[derive(Default)]
struct Tally {
    total: usize,
    shapes: [usize; SHAPES.len()],
    sources: BTreeMap<String, usize>,
    unattributed: usize,
}

fn shape_of(line: &str) -> Option<usize> {
    let lowered = line.to_ascii_lowercase();
    SHAPES
        .iter()
        .position(|(_, needle)| lowered.contains(needle))
}

/// The address a failed-login line blames, when one is written in a shape this tool knows:
/// sshd's `from 203.0.113.5 port 22` and PAM's `rhost=203.0.113.5`.
fn source_ip(line: &str) -> Option<String> {
    let token = if let Some((_, rest)) = line.split_once("rhost=") {
        rest.split_whitespace().next()?
    } else if let Some((_, rest)) = line.split_once(" from ") {
        rest.split_whitespace().next()?
    } else {
        return None;
    };
    let cleaned = token.trim_matches(|character: char| {
        !(character.is_ascii_alphanumeric() || matches!(character, '.' | ':' | '-' | '_'))
    });
    let plausible = !cleaned.is_empty()
        && cleaned.len() <= 64
        && cleaned.chars().any(|character| character.is_ascii_digit())
        && cleaned.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | ':' | '-' | '_')
        });
    plausible.then(|| cleaned.to_owned())
}

fn tally(lines: &[String]) -> Tally {
    let mut tally = Tally::default();
    for line in lines {
        let Some(index) = shape_of(line) else {
            continue;
        };
        tally.total += 1;
        tally.shapes[index] += 1;
        match source_ip(line) {
            Some(address) => *tally.sources.entry(address).or_default() += 1,
            None => tally.unattributed += 1,
        }
    }
    tally
}

/// Speaks only from the tallied counts: a shared shape of failures, never proof of an attack.
fn assess(tally: &Tally) -> Option<String> {
    if tally.total == 0 {
        return None;
    }
    if tally.sources.is_empty() {
        return Some(format!(
            "{} failures with no source address in the log — the cause cannot be told from this log",
            tally.total
        ));
    }
    let heaviest = tally.sources.values().copied().max().unwrap_or(0);
    if heaviest >= 5 {
        return Some(format!(
            "one address tried {heaviest} times in this window — that looks like a brute-force \
             attempt, not a typo"
        ));
    }
    let repeating = tally.sources.values().filter(|count| **count >= 2).count();
    if repeating >= 3 {
        return Some(format!(
            "{repeating} addresses tried more than once each — that looks like a coordinated scan"
        ));
    }
    if tally.total >= 10 {
        return Some(
            "many single failures from different addresses — that looks like automated scanning"
                .to_owned(),
        );
    }
    Some(format!(
        "no pattern: {} failures spread across addresses, which is what typos and stale credentials \
         look like",
        tally.total
    ))
}

fn report(log: &ReadLog, window: &str) -> String {
    let tally = tally(&log.lines);
    let mut out = String::new();
    out.push_str(
        "system.threats counts failed authentication lines and nothing else: it reads no \
         packets, watches no processes, and says nothing about intent\n",
    );
    out.push_str(&format!("source: {}\n", log.label));
    out.push_str(&format!("window: {window}\n"));
    for note in &log.notes {
        out.push_str(&format!("note: {note}\n"));
    }
    out.push_str(&format!("lines read: {}\n", log.lines.len()));
    if tally.total == 0 {
        out.push_str(
            "failed-login lines: 0 — none of the documented shapes appeared in what was read; a \
             zero is not evidence that no attempts occurred\n",
        );
        return out;
    }
    out.push_str(&format!("failed-login lines: {}\n", tally.total));
    if let Some(assessment) = assess(&tally) {
        out.push_str(&format!("assessment: {assessment}\n"));
    }
    out.push_str("by shape:\n");
    for (index, (label, _)) in SHAPES.iter().enumerate() {
        let count = tally.shapes[index];
        if count > 0 {
            out.push_str(&format!("  {label}: {count}\n"));
        }
    }
    let mut ranked: Vec<(String, usize)> = tally.sources.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    if ranked.is_empty() {
        out.push_str("by source address: none — no address could be parsed from those lines\n");
    } else {
        out.push_str("by source address:\n");
        for (address, count) in ranked.iter().take(MAX_LISTED_SOURCES) {
            out.push_str(&format!("  {address}: {count}\n"));
        }
        if ranked.len() > MAX_LISTED_SOURCES {
            out.push_str(&format!(
                "  … and {} more address(es) not listed\n",
                ranked.len() - MAX_LISTED_SOURCES
            ));
        }
    }
    if tally.unattributed > 0 {
        out.push_str(&format!(
            "unattributed: {} (no address was parseable on the line)\n",
            tally.unattributed
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(root: &Path) -> ToolContext {
        ToolContext {
            workspace_root: root.to_path_buf(),
            ..ToolContext::default()
        }
    }

    fn from_sources(sources: &[(&str, usize)], unattributed: usize) -> Tally {
        let mut tally = Tally {
            total: sources.iter().map(|(_, count)| count).sum::<usize>() + unattributed,
            unattributed,
            ..Tally::default()
        };
        for (address, count) in sources {
            tally.sources.insert((*address).to_owned(), *count);
        }
        tally
    }

    #[test]
    fn one_source_repeating_is_named_a_brute_force_attempt() {
        let assessment = assess(&from_sources(&[("203.0.113.5", 5)], 0)).unwrap();
        assert!(assessment.contains("tried 5 times"), "{assessment}");
        assert!(assessment.contains("brute-force attempt"), "{assessment}");
        assert!(!assessment.contains("coordinated scan"), "{assessment}");
        assert!(
            !assessment.contains("compromised") && !assessment.contains("safe"),
            "the assessment must not claim a compromise or safety: {assessment}"
        );
    }

    #[test]
    fn many_addresses_trying_twice_is_named_a_scan() {
        let tally = from_sources(
            &[("203.0.113.5", 2), ("198.51.100.7", 2), ("192.0.2.9", 2)],
            0,
        );
        let assessment = assess(&tally).unwrap();
        assert!(
            assessment.contains("3 addresses tried more than once each"),
            "{assessment}"
        );
        assert!(assessment.contains("coordinated scan"), "{assessment}");
        assert!(!assessment.contains("brute-force"), "{assessment}");
    }

    #[test]
    fn failures_with_no_address_are_said_to_be_undecidable() {
        let tally = from_sources(&[], 6);
        assert_eq!(
            assess(&tally).as_deref(),
            Some(
                "6 failures with no source address in the log — the cause cannot be told from this \
                 log"
            )
        );
    }

    #[test]
    fn a_clean_log_has_no_assessment() {
        assert_eq!(assess(&Tally::default()), None);
        let log = ReadLog {
            lines: Vec::new(),
            label: "journald".to_owned(),
            notes: Vec::new(),
        };
        let report = report(&log, "none");
        assert!(
            !report.contains("assessment:"),
            "a zero count has nothing to assess: {report}"
        );
    }

    #[test]
    fn a_failed_login_burst_is_reported_with_its_source() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("auth.log"),
            "Nov 23 10:00:01 host sshd[1]: Failed password for root from 203.0.113.5 port 40022 ssh2\n\
             Nov 23 10:00:05 host sshd[1]: Failed password for invalid user admin from 203.0.113.5 port 40026 ssh2\n\
             Nov 23 10:00:09 host sshd[1]: Invalid user oracle from 198.51.100.7 port 50122\n\
             Nov 23 10:00:11 host sshd[1]: Failed password for root from 198.51.100.7 port 50130 ssh2\n\
             Nov 23 10:00:20 host CRON[9]: (root) CMD (logrotate)\n\
             Nov 23 10:01:00 host sshd[1]: Accepted password for root from 192.0.2.1 port 60000 ssh2\n",
        )
        .unwrap();

        let output = SystemThreats
            .run(
                &json!({"source": "file", "path": "auth.log"}),
                &workspace(directory.path()),
            )
            .unwrap();

        assert!(output.ok, "{}", output.content);
        assert!(
            output.content.contains("failed-login lines: 4"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("203.0.113.5: 2"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("198.51.100.7: 2"),
            "{}",
            output.content
        );
        assert!(
            !output.content.contains("192.0.2.1"),
            "an accepted login is not a failed login: {}",
            output.content
        );
        assert!(
            !output.content.contains("CRON"),
            "a line outside the documented shapes is not counted: {}",
            output.content
        );
    }

    #[test]
    fn an_unreadable_log_is_reported_as_no_claim() {
        let directory = tempfile::tempdir().unwrap();

        let output = SystemThreats
            .run(
                &json!({"source": "file", "path": "absent.log"}),
                &workspace(directory.path()),
            )
            .unwrap();

        assert!(
            output.ok,
            "an unreadable log is a degraded success, not a tool failure: {}",
            output.content
        );
        assert!(
            output
                .content
                .contains("no authentication log was readable; nothing is claimed"),
            "{}",
            output.content
        );
        assert!(output.content.contains("absent.log"), "{}", output.content);
        assert!(
            !output.content.contains("failed-login lines: 0"),
            "an unread log must not be dressed up as a zero count: {}",
            output.content
        );
    }

    #[test]
    fn a_log_with_no_failed_logins_says_zero_is_not_evidence() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("auth.log"),
            "Nov 23 10:01:00 host sshd[1]: Accepted password for root from 192.0.2.1 port 60000 ssh2\n",
        )
        .unwrap();

        let output = SystemThreats
            .run(
                &json!({"source": "file", "path": "auth.log"}),
                &workspace(directory.path()),
            )
            .unwrap();

        assert!(output.ok, "{}", output.content);
        assert!(
            output.content.contains("a zero is not evidence"),
            "{}",
            output.content
        );
    }

    #[test]
    fn the_window_defaults_and_is_capped_at_a_day() {
        assert_eq!(window_minutes(&json!({})), 60);
        assert_eq!(window_minutes(&json!({"since_minutes": 15})), 15);
        assert_eq!(window_minutes(&json!({"since_minutes": 0})), 1);
        assert_eq!(window_minutes(&json!({"since_minutes": 100_000})), 1_440);
    }

    #[test]
    fn an_unknown_source_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let output = SystemThreats
            .run(&json!({"source": "wifi"}), &workspace(directory.path()))
            .unwrap();
        assert!(!output.ok, "{}", output.content);
        assert!(output.content.contains("journald"), "{}", output.content);
        assert!(output.content.contains("file"), "{}", output.content);
    }

    #[test]
    fn the_tool_is_registered_and_advertises_its_schema() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry
            .get("system.threats")
            .expect("system.threats registered");
        assert_eq!(tool.spec().name, "system.threats");
        assert_eq!(tool.spec().schema["type"], "object");
        assert_eq!(
            tool.spec().schema["properties"]["since_minutes"]["type"],
            "integer"
        );
        assert_eq!(
            tool.spec().schema["properties"]["source"]["enum"][0],
            "journald"
        );
        assert_eq!(
            tool.spec().schema["properties"]["source"]["enum"][1],
            "file"
        );
        assert_eq!(tool.spec().schema["properties"]["path"]["type"], "string");
        assert!(
            tool.spec().description.contains("counts failed logins"),
            "the description must not claim detection: {}",
            tool.spec().description
        );
        assert!(registry.names().contains(&"system.threats".to_owned()));
    }

    #[test]
    fn the_permission_is_a_read_with_no_side_effect() {
        let from_file = SystemThreats
            .permission(&json!({"source": "file", "path": "auth.log"}))
            .unwrap();
        assert_eq!(from_file.action, ActionKind::Read);
        assert_eq!(from_file.side_effect, SideEffect::None);
        assert_eq!(from_file.verb, "system.threats");
        assert_eq!(from_file.resource, "auth.log");

        let from_journald = SystemThreats.permission(&json!({})).unwrap();
        assert_eq!(from_journald.resource, "journald");
    }
}
