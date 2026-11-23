use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::path::Path;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;

/// A copy is a second full database on the same disk, so the ceiling is about what one call may
/// commit to writing rather than about what SQLite can hold.
const MAX_BACKUP_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Sqlite,
    Dir,
}

impl Format {
    fn of_source(path: &Path) -> Self {
        if path.is_dir() {
            Self::Dir
        } else {
            Self::Sqlite
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "sqlite" => Some(Self::Sqlite),
            "dir" => Some(Self::Dir),
            _ => None,
        }
    }
}

pub(crate) struct DbBackup;

impl Tool for DbBackup {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "db.backup".into(),
            description: "Copy a SQLite database into a destination directory and integrity-check \
                          both the source and the copy."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "SQLite file to copy."},
                    "to": {"type": "string", "description": "Destination directory."},
                    "format": {"type": "string", "description": "`sqlite` or `dir`; inferred."},
                    "verify": {"type": "boolean", "description": "Check the copy. Default true."}
                },
                "required": ["path", "to"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "db.backup",
            path,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let (source, source_display) =
            canonical_resource(&ctx.workspace_root, &arg_str(args, "path")?)?;
        let (destination, destination_display) =
            canonical_resource(&ctx.workspace_root, &arg_str(args, "to")?)?;
        let verify = args.get("verify").and_then(Value::as_bool).unwrap_or(true);

        let format = match args.get("format").and_then(Value::as_str) {
            Some(name) => match Format::parse(name) {
                Some(format) => format,
                None => {
                    return Ok(refused(format!(
                        "`{name}` is not a db.backup format: use `sqlite` or `dir`"
                    )));
                }
            },
            None => Format::of_source(&source),
        };

        if !source.exists() {
            return Ok(refused(format!(
                "cannot back up `{source_display}`: it does not exist"
            )));
        }
        if destination.starts_with(&source) {
            return Ok(refused(format!(
                "refusing to back up `{source_display}` into itself: the destination \
                 `{destination_display}` is inside the source"
            )));
        }
        if format == Format::Dir {
            return Ok(refused(format!(
                "db.backup does not copy `{source_display}`: a live database directory needs its \
                 engine's own backup path (pg_basebackup, pg_dump); a hot copy of a running \
                 database is how people lose data, and this tool is not a database engine"
            )));
        }

        let length = match std::fs::metadata(&source) {
            Ok(meta) if meta.is_file() => meta.len(),
            Ok(_) => {
                return Ok(refused(format!(
                    "`{source_display}` is not a SQLite database file"
                )));
            }
            Err(error) => {
                return Ok(refused(format!("cannot read `{source_display}`: {error}")));
            }
        };
        if length > MAX_BACKUP_BYTES {
            return Ok(refused(format!(
                "`{source_display}` is {:.1} GiB, above the {} GiB ceiling db.backup will copy",
                length as f64 / (1024.0 * 1024.0 * 1024.0),
                MAX_BACKUP_BYTES / (1024 * 1024 * 1024)
            )));
        }

        if let Err(error) = std::fs::File::open(&source) {
            return Ok(refused(format!("cannot read `{source_display}`: {error}")));
        }
        let source_verdict = match integrity(&source) {
            Ok(verdict) => verdict,
            Err(error) => {
                return Ok(refused(format!(
                    "`{source_display}` is not readable as SQLite: {error}"
                )));
            }
        };
        if source_verdict != "ok" {
            return Ok(refused(format!(
                "refusing to copy `{source_display}`: integrity_check on the source says \
                 `{source_verdict}`"
            )));
        }

        if let Err(error) = std::fs::create_dir_all(&destination) {
            return Ok(refused(format!(
                "cannot create `{destination_display}`: {error}"
            )));
        }
        let stem = source
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("database");
        let copy = destination.join(format!("{stem}-{}.db", utc_stamp()));
        if copy.exists() {
            return Ok(refused(format!(
                "`{}` already exists; db.backup does not overwrite a backup",
                copy.display()
            )));
        }

        let bytes = match std::fs::copy(&source, &copy) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Ok(refused(format!(
                    "cannot copy `{source_display}` to `{}`: {error}",
                    copy.display()
                )));
            }
        };

        let copy_verdict = if verify {
            match integrity(&copy) {
                Ok(verdict) => verdict,
                Err(error) => {
                    return Ok(refused(format!(
                        "copied `{source_display}` to `{}` but the copy is not readable as \
                         SQLite: {error}",
                        copy.display()
                    )));
                }
            }
        } else {
            "skipped (verify=false)".to_owned()
        };
        if verify && copy_verdict != "ok" {
            return Ok(refused(format!(
                "copied `{source_display}` to `{}` but integrity_check on the copy says \
                 `{copy_verdict}`",
                copy.display()
            )));
        }

        Ok(ToolOutput {
            content: format!(
                "backed up `{source_display}` to `{}` ({bytes} bytes)\n\
                 source integrity_check: {source_verdict}\n\
                 copy integrity_check: {copy_verdict}",
                copy.display()
            ),
            ok: true,
        })
    }
}

fn refused(content: String) -> ToolOutput {
    ToolOutput { content, ok: false }
}

/// The verdict `PRAGMA integrity_check` gives for a database, or the reason SQLite would not read
/// it at all. A multi-row answer means SQLite found problems, so every row is kept.
fn integrity(path: &Path) -> Result<String, String> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    let mut statement = connection
        .prepare("PRAGMA integrity_check")
        .map_err(|error| error.to_string())?;
    let verdicts: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(if verdicts.is_empty() {
        "no verdict returned".to_owned()
    } else {
        verdicts.join("; ")
    })
}

fn utc_stamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let clock = seconds % 86_400;
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        clock / 3600,
        clock % 3600 / 60,
        clock % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
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

    fn make_database(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO notes (body) VALUES ('hello backup');",
            )
            .unwrap();
    }

    #[test]
    fn a_backup_is_readable_and_passes_integrity() {
        let directory = tempfile::tempdir().unwrap();
        make_database(&directory.path().join("app.db"));

        let output = DbBackup
            .run(
                &json!({"path": "app.db", "to": "backups"}),
                &workspace(directory.path()),
            )
            .unwrap();

        assert!(output.ok, "{}", output.content);
        assert!(
            output.content.contains("source integrity_check: ok"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("copy integrity_check: ok"),
            "{}",
            output.content
        );

        let copy = std::fs::read_dir(directory.path().join("backups"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let connection =
            Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let body: String = connection
            .query_row("SELECT body FROM notes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(body, "hello backup");
    }

    #[test]
    fn a_destination_inside_the_source_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        make_database(&directory.path().join("app.db"));

        let output = DbBackup
            .run(
                &json!({"path": "app.db", "to": "app.db/backups"}),
                &workspace(directory.path()),
            )
            .unwrap();

        assert!(!output.ok, "{}", output.content);
        assert!(output.content.contains("into itself"), "{}", output.content);
        assert!(output.content.contains("app.db"), "{}", output.content);
    }

    #[test]
    fn a_missing_source_is_an_error_not_a_panic() {
        let directory = tempfile::tempdir().unwrap();

        let output = DbBackup
            .run(
                &json!({"path": "absent.db", "to": "backups"}),
                &workspace(directory.path()),
            )
            .unwrap();

        assert!(!output.ok, "{}", output.content);
        assert!(
            output.content.contains("does not exist"),
            "{}",
            output.content
        );
        assert!(output.content.contains("absent.db"), "{}", output.content);
    }

    #[test]
    fn a_directory_source_is_refused_with_the_hot_copy_reason() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("pgdata")).unwrap();

        let output = DbBackup
            .run(
                &json!({"path": "pgdata", "to": "backups"}),
                &workspace(directory.path()),
            )
            .unwrap();

        assert!(!output.ok, "{}", output.content);
        assert!(
            output.content.contains("hot copy of a running database"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("pg_basebackup"),
            "{}",
            output.content
        );
        assert!(output.content.contains("pgdata"), "{}", output.content);
    }

    #[test]
    fn the_tool_is_registered_and_advertises_its_schema() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry.get("db.backup").expect("db.backup registered");
        assert_eq!(tool.spec().name, "db.backup");
        assert_eq!(tool.spec().schema["type"], "object");
        assert_eq!(tool.spec().schema["properties"]["path"]["type"], "string");
        assert_eq!(tool.spec().schema["properties"]["to"]["type"], "string");
        assert_eq!(tool.spec().schema["required"][0], "path");
        assert_eq!(tool.spec().schema["required"][1], "to");
        assert!(registry.names().contains(&"db.backup".to_owned()));
    }

    #[test]
    fn the_permission_is_a_write_with_a_local_mutation() {
        let request = DbBackup
            .permission(&json!({"path": "app.db", "to": "backups"}))
            .unwrap();
        assert_eq!(request.action, ActionKind::Write);
        assert_eq!(request.side_effect, SideEffect::LocalMutation);
        assert_eq!(request.verb, "db.backup");
        assert_eq!(request.resource, "app.db");
    }

    #[test]
    fn the_utc_stamp_names_a_real_calendar_day() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_780), (2026, 11, 23));
    }
}
