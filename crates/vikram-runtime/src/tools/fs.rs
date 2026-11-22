use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, ToolCall, Trust, VikramError};

use crate::tools::*;
use crate::types::ApprovalPreview;

/// Largest payload `fs.write`/`fs.edit` will put on disk in one call. Bigger content is refused
/// before anything is touched — a tool call is not a file-transfer channel.
const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;

/// Lines of each side of an approval preview. The cut is announced with its size, so a shortened
/// preview can never be mistaken for a small change.
const PREVIEW_LINES: usize = 200;

/// Unchanged lines kept around an edit's hunk. The point of the preview is the change, so a
/// whole-file before/after would show the head of a large file and hide the line being edited.
const PREVIEW_CONTEXT: usize = 3;

/// Changed lines an edit reports back to the model before eliding the rest.
const RESULT_CHANGED_LINES: usize = 10;

/// One side of a preview, capped at [`PREVIEW_LINES`] with the remainder counted out loud.
fn capped(side: &str) -> String {
    let total = side.lines().count();
    if total <= PREVIEW_LINES {
        return side.to_owned();
    }
    let mut out: String = side
        .lines()
        .take(PREVIEW_LINES)
        .map(|line| format!("{line}\n"))
        .collect();
    out.push_str(&format!(
        "… [{PREVIEW_LINES} of {total} lines shown, {} cut]",
        total - PREVIEW_LINES
    ));
    out
}

/// Join unchanged context around the replaced text into one side of a diff.
fn hunk_side(head: &[&str], middle: &str, tail: &[&str]) -> String {
    let mut out = String::new();
    for line in head {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(middle);
    if !middle.is_empty() && !middle.ends_with('\n') && !tail.is_empty() {
        out.push('\n');
    }
    for line in tail {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The window one edit changes: the matched text plus a few unchanged lines either side.
fn edit_hunk(content: &str, old: &str, new: &str) -> Option<(String, String)> {
    let start = content.find(old)?;
    let end = start + old.len();
    let prefix: Vec<&str> = content[..start].lines().collect();
    let head = &prefix[prefix.len().saturating_sub(PREVIEW_CONTEXT)..];
    let suffix = content[end..].strip_prefix('\n').unwrap_or(&content[end..]);
    let tail: Vec<&str> = suffix.lines().take(PREVIEW_CONTEXT).collect();
    Some((hunk_side(head, old, &tail), hunk_side(head, new, &tail)))
}

/// The first `limit` changed lines, removals then additions, with the rest counted.
fn changed_lines(old: &str, new: &str, limit: usize) -> String {
    let mut lines: Vec<String> = old
        .lines()
        .map(|line| format!("- {line}"))
        .chain(new.lines().map(|line| format!("+ {line}")))
        .collect();
    let total = lines.len();
    let mut out = lines
        .drain(..limit.min(total))
        .collect::<Vec<_>>()
        .join("\n");
    if total > limit {
        out.push_str(&format!("\n… [{} more changed lines]", total - limit));
    }
    out
}

pub(crate) struct ReadFile;

impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.read".into(),
            description: "Read a text file from the workspace.".into(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "fs.read",
            path,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let (path, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(ToolOutput {
                content: truncated(text),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("cannot read `{}`: {e}", path.display()),
                ok: false,
            }),
        }
    }
}

pub(crate) struct ListDir;

impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.list".into(),
            description: "List a directory in the workspace.".into(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "fs.list",
            path,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let (path, _) = canonical_resource(&ctx.workspace_root, raw)?;
        let mut entries: Vec<String> = Vec::new();
        match std::fs::read_dir(&path) {
            Ok(iter) => {
                for entry in iter {
                    // A directory entry is still reported even when its OS-level
                    // metadata read fails or its name is not valid UTF-8: convert
                    // the name lossily (replacement chars mark the invalid bytes)
                    // instead of panicking on `.unwrap()` or silently skipping.
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(_) => continue,
                    };
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name == ".git" || name == "target" {
                        continue;
                    }
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    entries.push(if is_dir { format!("{name}/") } else { name });
                }
            }
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("cannot list `{}`: {e}", path.display()),
                    ok: false,
                });
            }
        }
        entries.sort();
        Ok(ToolOutput {
            content: truncated(entries.join("\n")),
            ok: true,
        })
    }
}

/// Best-effort durable checkpoint of a workspace mutation, so an agent's edit is never the only
/// copy: the original bytes are spooled per-session and `restore_file` can bring them back. A no-op
/// outside a session (a direct CLI tool call), where there is no conversation to recover into.
fn checkpoint(ctx: &ToolContext, path: &std::path::Path, operation: &str, before: Option<&[u8]>) {
    let (Some(runtime), Some(session)) = (ctx.runtime.as_ref(), ctx.session.as_ref()) else {
        return;
    };
    let _ = runtime
        .workspace
        .record_file_change(session, None, path, operation, before);
}

pub(crate) struct WriteFile;

impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.write".into(),
            description: "Write a text file inside the workspace, creating parents.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "fs.write",
            path,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn preview(&self, call: &ToolCall, ctx: &ToolContext) -> Option<ApprovalPreview> {
        let raw = call.arguments.get("path")?.as_str()?;
        let content = call.arguments.get("content")?.as_str()?;
        let (path, _) = canonical_resource(&ctx.workspace_root, raw).ok()?;
        let before = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // A file that is not there yet is a create: its before side is genuinely empty.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            // Exists but is not readable text — a text diff would be a guess, so show none.
            Err(_) => return None,
        };
        Some(ApprovalPreview::Diff {
            path: raw.to_owned(),
            before: capped(&before),
            after: capped(content),
        })
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let content = arg_str(args, "content")?;
        if content.len() > MAX_WRITE_BYTES {
            return Ok(ToolOutput {
                content: format!(
                    "content is {} bytes, over the {MAX_WRITE_BYTES} byte write limit",
                    content.len()
                ),
                ok: false,
            });
        }
        let (path, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Ok(ToolOutput {
                content: format!("cannot create `{}`: {e}", parent.display()),
                ok: false,
            });
        }
        let existed = path.is_file();
        let before = if existed {
            std::fs::read(&path).ok()
        } else {
            None
        };
        let bytes = content.len();
        match std::fs::write(&path, content) {
            Ok(()) => {
                checkpoint(
                    ctx,
                    &path,
                    if existed { "modify" } else { "create" },
                    before.as_deref(),
                );
                Ok(ToolOutput {
                    content: format!("wrote {} bytes to {}", bytes, path.display()),
                    ok: true,
                })
            }
            Err(e) => Ok(ToolOutput {
                content: format!("cannot write `{}`: {e}", path.display()),
                ok: false,
            }),
        }
    }
}

pub(crate) struct EditFile;

impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.edit".into(),
            description: "Edit a file by replacing an exact string.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "fs.edit",
            path,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn preview(&self, call: &ToolCall, ctx: &ToolContext) -> Option<ApprovalPreview> {
        let raw = call.arguments.get("path")?.as_str()?;
        let old_string = call.arguments.get("old_string")?.as_str()?;
        let new_string = call.arguments.get("new_string")?.as_str()?;
        // Nothing changes, so there is no diff to show — an empty one would be a lie about effect.
        if old_string == new_string {
            return None;
        }
        let (path, _) = canonical_resource(&ctx.workspace_root, raw).ok()?;
        let content = std::fs::read_to_string(&path).ok()?;
        let (before, after) = edit_hunk(&content, old_string, new_string)?;
        Some(ApprovalPreview::Diff {
            path: raw.to_owned(),
            before: capped(&before),
            after: capped(&after),
        })
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let old_string = arg_str(args, "old_string")?;
        let new_string = arg_str(args, "new_string")?;
        let (path, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        let content = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("cannot read `{}`: {e}", path.display()),
                    ok: false,
                });
            }
        };
        let count = content.matches(&old_string).count();
        if count == 0 {
            return Ok(ToolOutput {
                content: "old_string not found in file".into(),
                ok: false,
            });
        }
        if count > 1 {
            return Ok(ToolOutput {
                content: format!("old_string appears {count} times, must be unique"),
                ok: false,
            });
        }
        let new_content = content.replace(&old_string, &new_string);
        // The requested state already holds. Reporting "edited ... bytes" here would claim a change
        // that did not happen, and an empty diff would read as one.
        if new_content == content {
            return Ok(ToolOutput {
                content: format!("no change: {raw} already holds the requested text"),
                ok: true,
            });
        }
        if new_content.len() > MAX_WRITE_BYTES {
            return Ok(ToolOutput {
                content: format!(
                    "result is {} bytes, over the {MAX_WRITE_BYTES} byte write limit",
                    new_content.len()
                ),
                ok: false,
            });
        }
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Ok(ToolOutput {
                content: format!("cannot create `{}`: {e}", parent.display()),
                ok: false,
            });
        }
        match std::fs::write(&path, &new_content) {
            Ok(()) => {
                checkpoint(ctx, &path, "modify", Some(content.as_bytes()));
                Ok(ToolOutput {
                    content: truncated(format!(
                        "edited {raw}: {} -> {} lines\n{}",
                        old_string.lines().count(),
                        new_string.lines().count(),
                        changed_lines(&old_string, &new_string, RESULT_CHANGED_LINES)
                    )),
                    ok: true,
                })
            }
            Err(e) => Ok(ToolOutput {
                content: format!("cannot write `{}`: {e}", path.display()),
                ok: false,
            }),
        }
    }
}

pub(crate) struct DeleteFile;

impl Tool for DeleteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.delete".into(),
            description: "Delete a file inside the workspace.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Write,
            "fs.delete",
            path,
            SideEffect::Destructive,
            Trust::Model,
        ))
    }

    fn preview(&self, call: &ToolCall, ctx: &ToolContext) -> Option<ApprovalPreview> {
        let raw = call.arguments.get("path")?.as_str()?;
        let (path, _) = canonical_resource(&ctx.workspace_root, raw).ok()?;
        // Bytes, not text: a binary file has no readable diff, so it gets no preview.
        let before = std::fs::read_to_string(&path).ok()?;
        Some(ApprovalPreview::Diff {
            path: raw.to_owned(),
            before: capped(&before),
            after: String::new(),
        })
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let (path, _) = canonical_resource(&ctx.workspace_root, &raw)?;
        // Read bytes, not text: a binary file must still be deletable, and the checkpoint
        // already speaks bytes. The transcript shows a lossy rendering of whatever was there.
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("cannot read `{}`: {e}", path.display()),
                    ok: false,
                });
            }
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {
                checkpoint(ctx, &path, "delete", Some(&bytes));
                let content = String::from_utf8_lossy(&bytes);
                Ok(ToolOutput {
                    content: truncated(format!(
                        "deleted {}\n--- deleted content ---\n{}",
                        raw, content
                    )),
                    ok: true,
                })
            }
            Err(e) => Ok(ToolOutput {
                content: format!("cannot delete `{}`: {e}", path.display()),
                ok: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ApprovalPreview;

    fn workspace_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            workspace_root: dir.to_path_buf(),
            ..ToolContext::default()
        }
    }

    fn call(name: &str, arguments: Value) -> vikram_contracts::ToolCall {
        vikram_contracts::ToolCall::new(name, arguments)
    }

    fn diff(preview: Option<ApprovalPreview>) -> (String, String, String) {
        match preview {
            Some(ApprovalPreview::Diff {
                path,
                before,
                after,
            }) => (path, before, after),
            other => panic!("expected a diff preview, got {other:?}"),
        }
    }

    #[test]
    fn delete_removes_a_file_that_is_not_valid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x41]).unwrap();
        let ctx = workspace_ctx(dir.path());
        let out = DeleteFile.run(&json!({"path": "blob.bin"}), &ctx).unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(!path.exists());
    }

    #[test]
    fn write_refuses_content_over_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = workspace_ctx(dir.path());
        let huge = "x".repeat(MAX_WRITE_BYTES + 1);
        let out = WriteFile
            .run(&json!({"path": "big.txt", "content": huge}), &ctx)
            .unwrap();
        assert!(!out.ok, "{}", out.content);
        assert!(!dir.path().join("big.txt").exists());
    }

    #[test]
    fn edit_refuses_a_result_over_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "small").unwrap();
        let ctx = workspace_ctx(dir.path());
        let huge = "y".repeat(MAX_WRITE_BYTES + 1);
        let out = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "small", "new_string": huge}),
                &ctx,
            )
            .unwrap();
        assert!(!out.ok, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "small");
    }

    #[test]
    fn write_preview_shows_the_file_before_and_after() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old line\n").unwrap();
        let ctx = workspace_ctx(dir.path());
        let preview = WriteFile.preview(
            &call(
                "fs.write",
                json!({"path": "a.txt", "content": "new line\n"}),
            ),
            &ctx,
        );
        let (path, before, after) = diff(preview);
        assert_eq!(path, "a.txt");
        assert_eq!(before, "old line\n");
        assert_eq!(after, "new line\n");
    }

    #[test]
    fn write_preview_of_a_new_file_shows_nothing_before() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = workspace_ctx(dir.path());
        let preview = WriteFile.preview(
            &call("fs.write", json!({"path": "new.txt", "content": "hello\n"})),
            &ctx,
        );
        let (_, before, after) = diff(preview);
        assert_eq!(
            before, "",
            "a file that does not exist yet has no before side"
        );
        assert_eq!(after, "hello\n");
    }

    #[test]
    fn edit_preview_is_a_diff_of_the_changed_hunk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();
        let ctx = workspace_ctx(dir.path());
        let preview = EditFile.preview(
            &call(
                "fs.edit",
                json!({"path": "f.txt", "old_string": "two", "new_string": "TWO"}),
            ),
            &ctx,
        );
        let (path, before, after) = diff(preview);
        assert_eq!(path, "f.txt");
        assert_eq!(before, "one\ntwo\nthree\n");
        assert_eq!(after, "one\nTWO\nthree\n");
    }

    #[test]
    fn edit_preview_keeps_the_change_visible_in_a_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=400).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &body).unwrap();
        let ctx = workspace_ctx(dir.path());
        let preview = EditFile.preview(
            &call(
                "fs.edit",
                json!({"path": "big.txt", "old_string": "line 399", "new_string": "line 399 edited"}),
            ),
            &ctx,
        );
        let (_, before, after) = diff(preview);
        assert!(
            before.contains("line 399\n") && after.contains("line 399 edited\n"),
            "the changed line is the point of the preview:\nbefore={before}\nafter={after}"
        );
        assert!(
            !before.contains("line 1\n"),
            "the preview is the hunk, not the first 200 lines of the file"
        );
    }

    #[test]
    fn a_preview_longer_than_the_line_cap_is_cut_and_says_how_much() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=300).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &body).unwrap();
        let ctx = workspace_ctx(dir.path());
        let preview = WriteFile.preview(
            &call(
                "fs.write",
                json!({"path": "big.txt", "content": "replaced\n"}),
            ),
            &ctx,
        );
        let (_, before, _) = diff(preview);
        assert_eq!(
            before.lines().count(),
            PREVIEW_LINES + 1,
            "the cap keeps exactly {PREVIEW_LINES} lines plus one announcement line: {before}"
        );
        assert!(
            before.contains("200 of 300") && before.contains("100 cut"),
            "the cut is announced with its size: {before}"
        );
    }

    #[test]
    fn delete_preview_shows_the_content_that_will_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
        let ctx = workspace_ctx(dir.path());
        let preview = DeleteFile.preview(&call("fs.delete", json!({"path": "gone.txt"})), &ctx);
        let (path, before, after) = diff(preview);
        assert_eq!(path, "gone.txt");
        assert_eq!(before, "bye\n");
        assert_eq!(after, "", "a delete leaves nothing behind");
    }

    #[test]
    fn an_edit_that_changes_nothing_reports_no_change_instead_of_an_empty_diff() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "same\n").unwrap();
        let ctx = workspace_ctx(dir.path());
        let args = json!({"path": "f.txt", "old_string": "same", "new_string": "same"});
        let out = EditFile.run(&args, &ctx).unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(
            out.content.contains("no change"),
            "the result says the file already holds the requested text: {}",
            out.content
        );
        assert!(
            EditFile.preview(&call("fs.edit", args), &ctx).is_none(),
            "an empty diff is not a preview"
        );
    }

    #[test]
    fn edit_result_names_the_first_changed_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let ctx = workspace_ctx(dir.path());
        let out = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "beta", "new_string": "BETA"}),
                &ctx,
            )
            .unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(
            out.content.contains("- beta") && out.content.contains("+ BETA"),
            "the changed line is shown, not just byte counts: {}",
            out.content
        );
    }

    #[test]
    fn a_tool_that_does_not_opt_in_carries_no_preview() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "text\n").unwrap();
        let ctx = workspace_ctx(dir.path());
        assert!(
            ReadFile
                .preview(&call("fs.read", json!({"path": "f.txt"})), &ctx)
                .is_none(),
            "reading has no effect to preview, and the default is no preview"
        );
    }
}
