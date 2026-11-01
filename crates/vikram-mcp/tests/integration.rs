use serde_json::json;
use vikram_mcp::discovery::DiscoveryCatalog;
use vikram_mcp::server::{ExposedTool, McpServer, McpServerConfig};
use vikram_mcp::visibility::{Visibility, VisibilityManager};

#[test]
fn server_handles_initialize() {
    let server = McpServer::new(McpServerConfig::default());
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
    let resp = server.handle_request(&req);
    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
}

#[test]
fn server_lists_tools() {
    let mut server = McpServer::new(McpServerConfig::default());
    server.register_tool(ExposedTool {
        name: "test_tool".to_string(),
        description: "A test tool".to_string(),
        input_schema: json!({"type": "object"}),
    });
    let req = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    let resp = server.handle_request(&req);
    let tools = resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
}

#[test]
fn server_rejects_unknown_tool() {
    let server = McpServer::new(McpServerConfig::default());
    let req = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "nonexistent", "arguments": {}}});
    let resp = server.handle_request(&req);
    assert!(resp["error"].is_object());
}

#[test]
fn server_handles_unknown_method() {
    let server = McpServer::new(McpServerConfig::default());
    let req = json!({"jsonrpc": "2.0", "id": 4, "method": "foo/bar"});
    let resp = server.handle_request(&req);
    assert_eq!(resp["error"]["code"], -32601);
}

#[test]
fn visibility_defaults_to_private() {
    let mut vm = VisibilityManager::new();
    vm.register("my-server".to_string());
    assert!(!vm.is_public("my-server"));
}

#[test]
fn visibility_toggle() {
    let mut vm = VisibilityManager::new();
    vm.register("my-server".to_string());
    vm.set_visibility("my-server", Visibility::Public);
    assert!(vm.is_public("my-server"));
    vm.set_visibility("my-server", Visibility::Private);
    assert!(!vm.is_public("my-server"));
}

#[test]
fn visibility_independent() {
    let mut vm = VisibilityManager::new();
    vm.register("a".to_string());
    vm.register("b".to_string());
    vm.set_visibility("a", Visibility::Public);
    assert!(vm.is_public("a"));
    assert!(!vm.is_public("b"));
}

#[test]
fn discovery_builtin() {
    let catalog = DiscoveryCatalog::builtin();
    assert!(!catalog.entries.is_empty());
    assert!(catalog.get("filesystem").is_some());
}

#[test]
fn discovery_search() {
    let catalog = DiscoveryCatalog::builtin();
    let results = catalog.search("git");
    assert!(!results.is_empty());
}
