# Tools

Tools are the action surface. Each implements the `Tool` trait: name, description,
permission request, run. Built-ins plus MCP-provided tools are registered in
`ToolRegistry`.

## Built-ins

Filesystem (`fs.*`), shell (`shell.run`), web (`web.search`), browser (`browser.fetch`,
`browser.open`, `browser.dom`, `browser.click`, …), documents (`doc.read`), system
(`system.status`, `system.threats`), databases (`db.backup`), deployment (`deploy.run`),
research (`research`, `cross_check`), git, computer, PTY, terminals (`terminal.*`), LSP, MCP,
plugins, skills, memory, and agent delegation (`worker.note`, `worker.notes`).

## Execution

Tool calls are gated by policy + posture. Results are journalled (ToolCall/ToolResult).
`ToolContext` carries workspace_root + sandbox.

## Files

- `crates/vikram-runtime/src/tools/` (one module per tool family; `mod.rs` registers them)

## Notes

Spawned commands are sandboxed (bubblewrap/sandbox-exec) when enabled. Tool output is
capped (20k). Timeouts are hardcoded per tool.