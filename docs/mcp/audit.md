# VIKRAM MCP Ecosystem — Repository Audit

## Existing MCP Implementation (as of start)

### MCP Client (partial)
- Location: crates/vikram-runtime/src/mcp.rs (458 lines)
- Transport: STDIO only (spawns child process, JSON-RPC 2.0 over stdin/stdout)
- Protocol: initialize, notifications/initialized, tools/list, tools/call
- Config: McpConfig / McpServerConfig in config.rs (name, command, args, env)
- Config path: ~/.vikram/mcp.json
- Registry: McpRegistry with connect/disconnect/call_tool/connect_all/connect_by_name
- Tool integration: McpTool implements Tool trait, registered in ToolRegistry::with_builtins()
- Tool naming: mcp.{server}.{tool}
- Runtime integration: RuntimeInner.mcp: Arc<Mutex<McpRegistry>>, connect_all spawned on startup
- Metrics: mcp_connected, mcp_configured
- TUI: InspectorKind::Mcp shows connected/configured, /mcp command
- CLI: vikram mcp lists configured servers

### Missing (not implemented)
1. MCP Server mode — no server exposing VIKRAM tools via MCP protocol
2. Streamable HTTP transport — only STDIO
3. OAuth/authentication — no auth
4. Resource discovery — only tools/list, no resources/list
5. Prompt discovery — no prompts/list
6. Private MCP Runtime — no managed process lifecycle (start/stop/restart/update/uninstall)
7. Discovery/catalog — no server discovery
8. Visibility toggle — no Private/Public per-server
9. Dual mode — no simultaneous client+server
10. Permission integration — MCP tools use existing Tool trait but no per-server permissions
11. MCP Server TUI — no server management UI
12. MCP Server CLI — no server start/stop commands

### Reusable components
- McpRegistry (client connections, tool discovery)
- McpTool (Tool trait adapter)
- McpConfig / McpServerConfig (configuration)
- ToolRegistry / Tool trait (tool execution pipeline)
- RuntimeInner (central runtime, event bus, metrics)
- RuntimeEvent (event system)
- PermissionRequest / ActionKind (permissions)
- Trust levels (trust system)
- TUI inspector framework
- CLI command structure
- Store (SQLite) for persistence

### Architecture
- Rust workspace, edition 2024
- Async runtime: tokio
- TUI: ratatui
- Config: JSON files in ~/.vikram/
- Store: SQLite (rusqlite)
- No external MCP SDK — hand-rolled JSON-RPC 2.0

### Integration points for new MCP features
- RuntimeInner.mcp → extend for server + runtime management
- ToolRegistry → add MCP server tools
- RuntimeEvent → add McpServerStarted/Stopped, etc.
- Metrics → add mcp_server_* fields
- TUI panels.rs → add server management UI
- CLI main.rs → add mcp server subcommands
- config.rs → add server config, visibility, auth
