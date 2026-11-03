# Tools

Tools are the action surface. Each implements the `Tool` trait: name, description,
permission request, run. Built-ins plus MCP-provided tools are registered in
`ToolRegistry`.

## Built-ins

Filesystem (read/write/edit), shell, web fetch/search, git, browser, computer, PTY,
LSP, MCP, skills, memory, and agent delegation.

## Execution

Tool calls are gated by policy + posture. Results are journalled (ToolCall/ToolResult).
`ToolContext` carries workspace_root + sandbox.

## Files

- `crates/vikram-runtime/src/tools.rs`

## Notes

Spawned commands are sandboxed (bubblewrap/sandbox-exec) when enabled. Tool output is
capped (20k). Timeouts are hardcoded per tool.