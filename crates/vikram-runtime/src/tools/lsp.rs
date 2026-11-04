use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::lsp::{LspClient, LspError, LspPool};

use crate::tools::*;
/// A 0-based integer argument (`line` or `column`).
fn arg_u32(args: &Value, key: &str) -> Result<u32, VikramError> {
    let value = args.get(key).and_then(Value::as_u64).ok_or_else(|| {
        VikramError::ProviderRejected(format!("missing integer argument `{key}`"))
    })?;
    u32::try_from(value)
        .map_err(|_| VikramError::ProviderRejected(format!("`{key}` is out of range: {value}")))
}

fn lsp_position_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path, resolved inside the workspace root."
            },
            "line": {
                "type": "integer",
                "minimum": 0,
                "description": "0-based line number."
            },
            "column": {
                "type": "integer",
                "minimum": 0,
                "description": "0-based UTF-16 column number."
            }
        },
        "required": ["path", "line", "column"]
    })
}

fn lsp_file_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path, resolved inside the workspace root."
            }
        },
        "required": ["path"]
    })
}

fn lsp_permission(verb: &str, args: &Value) -> Result<PermissionRequest, VikramError> {
    let path = arg_str(args, "path")?;
    Ok(PermissionRequest::new(
        ActionKind::Read,
        verb,
        path,
        SideEffect::None,
        Trust::Model,
    ))
}

/// Pretty-print an LSP result. `null` is a real answer ("nothing here"), so it is spelled out.
fn json_output(verb: &str, value: &Value, empty: &str) -> ToolOutput {
    if value.is_null() {
        return ToolOutput {
            content: empty.to_string(),
            ok: true,
        };
    }
    match serde_json::to_string_pretty(value) {
        Ok(text) => ToolOutput {
            content: truncated(text),
            ok: true,
        },
        Err(e) => ToolOutput {
            content: format!("{verb}: cannot format the result: {e}"),
            ok: false,
        },
    }
}

/// A language server failure is a failed observation: the model can read "install rust-analyzer".
fn lsp_failed(verb: &str, err: &LspError) -> ToolOutput {
    ToolOutput {
        content: format!("{verb} failed: {err}"),
        ok: false,
    }
}

/// Resolve the path argument inside the workspace. `Ok(None)` means the path escapes nothing but
/// does not exist (or is not a file), which is reported before any server is started for it.
fn lsp_file_arg(args: &Value, ctx: &ToolContext) -> Result<Option<(PathBuf, String)>, VikramError> {
    let raw = arg_str(args, "path")?;
    let (path, display) = canonical_resource(&ctx.workspace_root, &raw)?;
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some((path, display)))
}

fn run_lsp_position(
    verb: &str,
    query: fn(&mut LspClient, &Path, u32, u32) -> Result<Value, LspError>,
    args: &Value,
    ctx: &ToolContext,
) -> Result<ToolOutput, VikramError> {
    let line = arg_u32(args, "line")?;
    let column = arg_u32(args, "column")?;
    let Some((path, display)) = lsp_file_arg(args, ctx)? else {
        let raw = arg_str(args, "path")?;
        return Ok(ToolOutput {
            content: format!("{verb}: `{raw}` is not a readable file in the workspace"),
            ok: false,
        });
    };
    let outcome = LspPool::global().call(&ctx.workspace_root, &path, |client| {
        query(client, &path, line, column)
    });
    Ok(match outcome {
        Ok(value) => json_output(
            verb,
            &value,
            &format!("{verb}: nothing at {display}:{line}:{column}"),
        ),
        Err(err) => lsp_failed(verb, &err),
    })
}

fn run_lsp_file(
    verb: &str,
    query: fn(&mut LspClient, &Path) -> Result<Value, LspError>,
    empty: &str,
    args: &Value,
    ctx: &ToolContext,
) -> Result<ToolOutput, VikramError> {
    let Some((path, _display)) = lsp_file_arg(args, ctx)? else {
        let raw = arg_str(args, "path")?;
        return Ok(ToolOutput {
            content: format!("{verb}: `{raw}` is not a readable file in the workspace"),
            ok: false,
        });
    };
    Ok(
        match LspPool::global().call(&ctx.workspace_root, &path, |client| query(client, &path)) {
            Ok(value) => json_output(verb, &value, empty),
            Err(err) => lsp_failed(verb, &err),
        },
    )
}

/// Type and documentation under a position, as the language server sees it.
pub(crate) struct LspHover;

impl Tool for LspHover {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp.hover".into(),
            description: "Ask the language server what is at a position (type, signature, docs). \
                          Requires the server for the file's language to be installed."
                .into(),
            schema: lsp_position_schema(),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        lsp_permission("lsp.hover", args)
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        run_lsp_position("lsp.hover", LspClient::hover, args, ctx)
    }
}

/// Where a symbol is defined.
pub(crate) struct LspDefinition;

impl Tool for LspDefinition {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp.definition".into(),
            description: "Jump to where the symbol at a position is defined. \
                          Requires the server for the file's language to be installed."
                .into(),
            schema: lsp_position_schema(),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        lsp_permission("lsp.definition", args)
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        run_lsp_position("lsp.definition", LspClient::definition, args, ctx)
    }
}

/// Every use of a symbol.
pub(crate) struct LspReferences;

impl Tool for LspReferences {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp.references".into(),
            description: "Find every reference to the symbol at a position, declaration included. \
                          Requires the server for the file's language to be installed."
                .into(),
            schema: lsp_position_schema(),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        lsp_permission("lsp.references", args)
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        run_lsp_position("lsp.references", LspClient::references, args, ctx)
    }
}

/// The symbol outline of a file.
pub(crate) struct LspSymbols;

impl Tool for LspSymbols {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp.symbols".into(),
            description: "List the symbols a file declares, as reported by its language server. \
                          Requires the server for the file's language to be installed."
                .into(),
            schema: lsp_file_schema(),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        lsp_permission("lsp.symbols", args)
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        run_lsp_file(
            "lsp.symbols",
            LspClient::document_symbols,
            &format!("lsp.symbols: `{raw}` declares no symbols"),
            args,
            ctx,
        )
    }
}

/// Compiler and lint diagnostics from the language server.
pub(crate) struct LspDiagnostics;

impl Tool for LspDiagnostics {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp.diagnostics".into(),
            description: "Collect the diagnostics the language server reports for one file \
                          (errors, warnings, lints). Requires the server for the file's language \
                          to be installed."
                .into(),
            schema: lsp_file_schema(),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        lsp_permission("lsp.diagnostics", args)
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        run_lsp_file(
            "lsp.diagnostics",
            LspClient::diagnostics,
            "lsp.diagnostics: the server reported no diagnostics",
            args,
            ctx,
        )
    }
}
