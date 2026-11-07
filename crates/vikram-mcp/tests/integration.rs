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
    // `approved_tools` is an explicit allowlist with no implicit allow-all:
    // a registered tool is only advertised once it is approved.
    let config = McpServerConfig {
        approved_tools: vec!["test_tool".to_string()],
        ..McpServerConfig::default()
    };
    let mut server = McpServer::new(config);
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
fn server_lists_no_tools_when_none_approved() {
    // Empty allowlist exposes nothing, even for registered tools.
    let mut server = McpServer::new(McpServerConfig::default());
    server.register_tool(ExposedTool {
        name: "test_tool".to_string(),
        description: "A test tool".to_string(),
        input_schema: json!({"type": "object"}),
    });
    let req = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    let resp = server.handle_request(&req);
    let tools = resp["result"]["tools"].as_array().unwrap();
    assert!(tools.is_empty());
    let req = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "test_tool", "arguments": {}}});
    let resp = server.handle_request(&req);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("not approved")
    );
}

#[test]
fn server_rejects_auth_required_without_token() {
    // Misconfiguration is a startup error, not a silent rejection loop.
    let config = McpServerConfig {
        auth_required: true,
        ..McpServerConfig::default()
    };
    assert!(config.validate().is_err());
    assert!(McpServer::try_new(config).is_err());
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
fn visibility_exposed_tools_persist_across_managers() {
    // set_exposed_tools must persist like set_visibility does.
    let dir =
        std::env::temp_dir().join(format!("vikram-mcp-visibility-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("visibility.json");
    let _ = std::fs::remove_file(&path);
    {
        let mut vm = VisibilityManager::with_persist_path(path.clone());
        vm.set_exposed_tools("fs", vec!["read".to_string(), "write".to_string()]);
        vm.set_visibility("fs", Visibility::Public);
        vm.save().expect("save");
    }
    let vm = VisibilityManager::with_persist_path(path.clone());
    let entry = vm.get("fs").expect("entry loaded from disk");
    assert_eq!(entry.exposed_tools, vec!["read", "write"]);
    assert!(vm.is_public("fs"));
    // set_persist_path on a fresh manager also loads existing entries.
    let mut vm = VisibilityManager::new();
    vm.set_persist_path(path.clone());
    assert_eq!(
        vm.get("fs")
            .expect("entry loaded via set_persist_path")
            .exposed_tools,
        vec!["read", "write"]
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
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
