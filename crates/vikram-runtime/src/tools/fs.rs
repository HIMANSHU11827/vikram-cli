use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;

/// Largest payload `fs.write`/`fs.edit` will put on disk in one call. Bigger content is refused
/// before anything is touched — a tool call is not a file-transfer channel.
const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;

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
                        "edited {}: {} -> {} bytes",
                        raw,
                        content.len(),
                        new_content.len()
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

    fn workspace_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            workspace_root: dir.to_path_buf(),
            ..ToolContext::default()
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
}
