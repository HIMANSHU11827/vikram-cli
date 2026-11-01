# VIKRAM MCP Ecosystem

## Overview

VIKRAM includes a complete Model Context Protocol (MCP) ecosystem:

1. **MCP Client** — connects to external MCP servers via STDIO, discovers and executes their tools.
2. **MCP Server** — exposes approved VIKRAM tools to external MCP clients via JSON-RPC 2.0 over STDIO.
3. **Private MCP Runtime** — manages third-party MCP server processes (start/stop/restart).
4. **Discovery** — catalog of available MCP servers.
5. **Visibility** — per-server Private/Public toggle.

## Components

- **Crate:** `vikram-mcp` (server, runtime, discovery, visibility)
- **Client:** `vikram-runtime/src/mcp.rs` (McpRegistry, STDIO transport)
- **Config:** `~/.vikram/mcp.json`
- **TUI:** InspectorKind::Mcp shows connected/configured
- **CLI:** `vikram mcp [list|status|serve]`

## Usage

### Connect to external MCP server
Add to `~/.vikram/mcp.json`:
```json
{"servers": [{"name": "fs", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"], "env": {}}]}
```

### Expose VIKRAM tools via MCP Server
```bash
vikram mcp serve
```

### Manage visibility
Visibility is per-server, defaults to Private. Toggle via VisibilityManager API.

## Tests
- 9 unit/integration tests in vikram-mcp crate
- All passing
