# VIKRAM MCP Architecture

## Components

### MCP Client (existing, in vikram-runtime)
- `McpRegistry` — manages STDIO connections to external MCP servers
- `McpTool` — adapts external MCP tools into VIKRAM's Tool trait
- Connects on startup, discovers tools, registers them in ToolRegistry
- Config: `~/.vikram/mcp.json`

### MCP Server (new, in vikram-mcp crate)
- `McpServer` — JSON-RPC 2.0 server over STDIO
- Exposes approved VIKRAM tools to external MCP clients
- `handle_request()` processes initialize, tools/list, tools/call, resources/list, prompts/list
- Only explicitly approved tools are exposed (not all)

### Private MCP Runtime (new, in vikram-mcp crate)
- `PrivateRuntime` — manages third-party MCP server processes
- Start, stop, restart servers
- Process isolation with kill_on_drop
- Base dir: `~/.vikram/mcp-runtime/`

### Discovery (new, in vikram-mcp crate)
- `DiscoveryCatalog` — catalog of available MCP servers
- Built-in catalog with well-known servers (filesystem, git, memory)
- Search by name/description

### Visibility (new, in vikram-mcp crate)
- `VisibilityManager` — per-server Private/Public toggle
- `Visibility::Private` — only internal VIKRAM access
- `Visibility::Public` — approved tools exposed via MCP Server
- Default: Private

## Data Flow

```
External MCP Client (Claude Code, Codex, etc.)
    ↓ MCP protocol (STDIO/HTTP)
VIKRAM MCP Server (vikram-mcp)
    ↓ Tool dispatch
VIKRAM ToolRegistry
    ↓ Permission check
VIKRAM Runtime (execute tool)
```

```
VIKRAM Runtime
    ↓ McpTool (Tool trait)
External MCP Server (third-party, managed by PrivateRuntime)
    ↓ JSON-RPC (STDIO)
Third-party process
```

## Integration with RuntimeInner

- `mcp: Arc<Mutex<McpRegistry>>` — client connections
- `mcp_visibility: Mutex<VisibilityManager>` — visibility state
- `mcp_private_runtime: Mutex<PrivateRuntime>` — managed servers
