use super::*;
use crate::mcp::McpRegistry;
use crate::tools::web::search_tool_output;
use serde_json::{Value, json};
use std::sync::Arc;
use vikram_contracts::SideEffect;
use vikram_contracts::{ActionKind, Trust, VikramError};

fn ctx() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext {
        workspace_root: dir.path().to_path_buf(),
        sandbox: crate::sandbox::Sandbox::disabled("test context"),
        spill: crate::spill::Spill::disabled(),
        runtime: None,
        session: None,
    };
    (dir, ctx)
}

#[test]
fn write_then_read_round_trips_inside_the_workspace() {
    let (_dir, ctx) = ctx();
    let write = WriteFile;
    let read = ReadFile;
    let out = write
        .run(&json!({"path": "notes/a.txt", "content": "hello"}), &ctx)
        .unwrap();
    assert!(out.ok, "{}", out.content);
    let back = read.run(&json!({"path": "notes/a.txt"}), &ctx).unwrap();
    assert_eq!(back.content, "hello");
}

#[test]
fn writes_outside_the_workspace_are_refused_before_execution() {
    let (_dir, ctx) = ctx();
    let err = ReadFile
        .run(&json!({"path": "../../etc/passwd"}), &ctx)
        .unwrap_err();
    assert!(matches!(err, VikramError::PolicyDenied { .. }));
}

#[test]
fn terminal_tool_is_registered() {
    let registry = ToolRegistry::with_builtins();
    let tool = registry.get("terminal").expect("terminal registered");
    let spec = tool.spec();
    assert_eq!(spec.name, "terminal");
    assert!(spec.schema["properties"]["action"].is_object());
}

#[test]
fn terminal_declares_execute_permission() {
    let registry = ToolRegistry::with_builtins();
    let tool = registry.get("terminal").unwrap();
    let req = tool.permission(&json!({"action": "list"})).unwrap();
    assert_eq!(req.action, ActionKind::Execute);
    assert_eq!(req.verb, "terminal");
}

#[test]
fn computer_status_is_registered() {
    let registry = ToolRegistry::with_builtins();
    let tool = registry
        .get("computer.status")
        .expect("computer.status registered");
    assert_eq!(tool.spec().name, "computer.status");
    // Registration must not depend on the desktop helpers actually being installed.
    for name in [
        "computer.screenshot",
        "computer.click",
        "computer.type",
        "computer.key",
        "computer.scroll",
        "computer.windows",
    ] {
        assert!(registry.names().contains(&name.to_string()), "{name}");
    }
}

#[test]
fn every_builtin_declares_a_permission_shape() {
    let registry = ToolRegistry::with_builtins();
    for tool in registry.all() {
        let spec = tool.spec();
        assert!(!spec.name.is_empty());
        let sample = match spec.name.as_str() {
            "fs.write" => json!({"path": "a", "content": "b"}),
            "fs.edit" => json!({"path": "a", "old_string": "b", "new_string": "c"}),
            "search.text" => json!({"query": "x"}),
            "shell.run" => json!({"command": "echo hi"}),
            "git" => json!({"subcommand": "status"}),
            "build.run" => json!({}),
            "test.run" => json!({}),
            "memory.save" => json!({"key": "k", "value": "v"}),
            "memory.search" => json!({"query": "x"}),
            "memory.list" => json!({}),
            "memory.delete" => json!({"key": "k"}),
            "memory.note" => json!({"note": "x"}),
            "web.search" => json!({"query": "x"}),
            "team.create" => json!({"name": "t"}),
            "team.list" => json!({}),
            "team.delegate" => json!({"team": "t", "task": "x"}),
            "hive.create" => json!({"name": "t"}),
            "hive.list" => json!({}),
            "hive.show" => json!({"name": "t"}),
            "hive.delegate" => json!({"team": "t", "task": "x"}),
            "hive.delete" => json!({"name": "t"}),
            "bot.list" => json!({}),
            "bot.show" => json!({"name": "t"}),
            "bot.create" => json!({"name": "t", "role": "r", "description": "d"}),
            "agent.list" => json!({}),
            "agent.show" => json!({"name": "t"}),
            "skill.list" => json!({}),
            "skill.load" => json!({"name": "x"}),
            "skill.search" => json!({"query": "x"}),
            "skill.read" => json!({"name": "x", "path": "r"}),
            "diagnostics.collect" => json!({}),
            "process" => json!({"action": "list"}),
            "task" => json!({"action": "list"}),
            "ask_user" => json!({"question": "q"}),
            "browser.fetch" => json!({"url": "https://example.com"}),
            "browser.dom" | "browser.links" => json!({"url": "https://example.com"}),
            "browser.screenshot" => json!({"url": "https://example.com", "path": "shot.png"}),
            "browser.open" => json!({"url": "https://example.com"}),
            "browser.eval" => json!({"session": "browser-1", "js": "document.title"}),
            "browser.click" => json!({"session": "browser-1", "selector": "#go"}),
            "browser.type" => json!({"session": "browser-1", "selector": "#q", "text": "hi"}),
            "browser.scroll" => json!({"session": "browser-1", "x": 0, "y": 200}),
            "browser.text" | "browser.close" => json!({"session": "browser-1"}),
            "browser.sessions" => json!({}),
            "code.review" => json!({}),
            "codeindex" => json!({"action": "search", "query": "x"}),
            "session.search" => json!({"query": "x"}),
            "plugin.execute" => json!({"plugin": "p", "args": "x"}),
            "plugin.list" => json!({}),
            "plugin.install" | "plugin.validate" => json!({"path": "plugins/p"}),
            "plugin.uninstall" | "plugin.enable" | "plugin.disable" | "plugin.inspect" => {
                json!({"plugin": "p"})
            }
            "plugin.deps" => json!({}),
            "permission.check" => json!({"verb": "v", "resource": "r"}),
            "terminal" => json!({"action": "list"}),
            "computer.screenshot" => json!({"path": "shot.png"}),
            "computer.click" => json!({"x": 10, "y": 20}),
            "computer.type" => json!({"text": "hello"}),
            "computer.key" => json!({"keys": "ctrl+l"}),
            "computer.scroll" => json!({"direction": "down"}),
            "computer.windows" | "computer.status" => json!({}),
            "lsp.hover" | "lsp.definition" | "lsp.references" => {
                json!({"path": "a.rs", "line": 0, "column": 0})
            }
            "lsp.symbols" | "lsp.diagnostics" => json!({"path": "a.rs"}),
            _ => json!({"path": "."}),
        };
        let req = tool.permission(&sample).unwrap();
        assert_eq!(req.originating_trust, Trust::Model);
    }
}

#[test]
fn plugin_lifecycle_tools_are_registered() {
    let registry = ToolRegistry::with_builtins();
    let names = registry.names();
    for name in [
        "plugin.execute",
        "plugin.list",
        "plugin.install",
        "plugin.uninstall",
        "plugin.enable",
        "plugin.disable",
        "plugin.validate",
        "plugin.inspect",
        "plugin.deps",
    ] {
        assert!(
            names.contains(&name.to_string()),
            "{name} is not registered"
        );
    }
    assert_eq!(
        registry.get("plugin.install").unwrap().spec().schema["required"][0],
        "path"
    );
    assert_eq!(
        registry.get("plugin.inspect").unwrap().spec().schema["required"][0],
        "plugin"
    );
}

#[test]
fn plugin_lifecycle_tools_declare_permission_shapes() {
    let registry = ToolRegistry::with_builtins();
    let cases: [(&str, Value, ActionKind, SideEffect); 7] = [
        (
            "plugin.install",
            json!({"path": "plugins/p"}),
            ActionKind::Write,
            SideEffect::Destructive,
        ),
        (
            "plugin.uninstall",
            json!({"plugin": "p"}),
            ActionKind::Write,
            SideEffect::Destructive,
        ),
        (
            "plugin.enable",
            json!({"plugin": "p"}),
            ActionKind::Write,
            SideEffect::LocalMutation,
        ),
        (
            "plugin.disable",
            json!({"plugin": "p"}),
            ActionKind::Write,
            SideEffect::LocalMutation,
        ),
        (
            "plugin.validate",
            json!({"path": "plugins/p"}),
            ActionKind::Read,
            SideEffect::None,
        ),
        (
            "plugin.inspect",
            json!({"plugin": "p"}),
            ActionKind::Read,
            SideEffect::None,
        ),
        ("plugin.deps", json!({}), ActionKind::Read, SideEffect::None),
    ];
    for (name, sample, action, side_effect) in cases {
        let tool = registry
            .get(name)
            .unwrap_or_else(|| panic!("{name} registered"));
        let req = tool.permission(&sample).unwrap();
        assert_eq!(req.action, action, "{name}");
        assert_eq!(req.verb, name, "{name}");
        assert_eq!(req.side_effect, side_effect, "{name}");
        assert_eq!(req.originating_trust, Trust::Model, "{name}");
    }
}

#[test]
fn plugin_install_rejects_a_directory_without_a_manifest() {
    let (_dir, ctx) = ctx();
    let out = PluginInstall
        .run(&json!({"path": "plugins/absent"}), &ctx)
        .unwrap();
    assert!(!out.ok);
    assert!(out.content.contains("no plugin.json"), "{}", out.content);
}

#[test]
fn plugin_uninstall_reports_an_unknown_plugin() {
    let (_dir, ctx) = ctx();
    let out = PluginUninstall
        .run(&json!({"plugin": "definitely-not-installed-9f3a"}), &ctx)
        .unwrap();
    assert!(!out.ok);
    assert!(out.content.contains("plugin not found"), "{}", out.content);
}

#[test]
fn plugin_inspect_reports_an_unknown_plugin() {
    let (_dir, ctx) = ctx();
    let out = PluginInspect
        .run(&json!({"plugin": "definitely-not-installed-9f3a"}), &ctx)
        .unwrap();
    assert!(!out.ok);
    assert!(out.content.contains("plugin not found"), "{}", out.content);
}

#[test]
fn plugin_validate_reports_manifest_problems() {
    let (dir, ctx) = ctx();
    std::fs::create_dir_all(dir.path().join("plugins/bad")).unwrap();
    std::fs::write(
        dir.path().join("plugins/bad/plugin.json"),
        r#"{"name":"bad","version":"1.0"}"#,
    )
    .unwrap();
    let out = PluginValidate
        .run(&json!({"path": "plugins/bad"}), &ctx)
        .unwrap();
    assert!(!out.ok);
    assert!(out.content.contains("not semver"), "{}", out.content);
    assert!(out.content.contains("no `entry`"), "{}", out.content);
}

#[test]
fn plugin_validate_accepts_a_clean_manifest() {
    let (dir, ctx) = ctx();
    std::fs::create_dir_all(dir.path().join("plugins/good")).unwrap();
    std::fs::write(
        dir.path().join("plugins/good/plugin.json"),
        r#"{"name":"good","version":"1.0.0","description":"d","entry":"main.sh"}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("plugins/good/main.sh"), "#!/bin/sh\n").unwrap();
    let out = PluginValidate
        .run(&json!({"path": "plugins/good"}), &ctx)
        .unwrap();
    assert!(out.ok, "{}", out.content);
    assert!(out.content.contains("no problems found"), "{}", out.content);
}

#[test]
fn plugin_deps_answers_for_the_workspace() {
    let (_dir, ctx) = ctx();
    let out = PluginDeps.run(&json!({}), &ctx).unwrap();
    assert!(out.ok);
    assert!(!out.content.is_empty());
}

#[test]
fn shell_declares_an_execute_side_effect() {
    let req = RunShell
        .permission(&json!({"command": "echo hello"}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Execute);
    assert_eq!(req.verb, "shell.run");
    assert_eq!(req.side_effect, SideEffect::LocalMutation);
}

#[test]
fn edit_replaces_exact_string() {
    let (_dir, ctx) = ctx();
    let write = WriteFile;
    let edit = EditFile;
    let read = ReadFile;
    write
        .run(&json!({"path": "a.txt", "content": "hello world"}), &ctx)
        .unwrap();
    let out = edit
        .run(
            &json!({"path": "a.txt", "old_string": "world", "new_string": "rust"}),
            &ctx,
        )
        .unwrap();
    assert!(out.ok, "{}", out.content);
    let back = read.run(&json!({"path": "a.txt"}), &ctx).unwrap();
    assert_eq!(back.content, "hello rust");
}

#[test]
fn edit_old_string_not_found_returns_false() {
    let (_dir, ctx) = ctx();
    let write = WriteFile;
    let edit = EditFile;
    write
        .run(&json!({"path": "a.txt", "content": "hello world"}), &ctx)
        .unwrap();
    let out = edit
        .run(
            &json!({"path": "a.txt", "old_string": "nope", "new_string": "rust"}),
            &ctx,
        )
        .unwrap();
    assert!(!out.ok);
    assert_eq!(out.content, "old_string not found in file");
}

#[test]
fn edit_old_string_multiple_returns_false() {
    let (_dir, ctx) = ctx();
    let write = WriteFile;
    let edit = EditFile;
    write
        .run(&json!({"path": "a.txt", "content": "aa"}), &ctx)
        .unwrap();
    let out = edit
        .run(
            &json!({"path": "a.txt", "old_string": "a", "new_string": "b"}),
            &ctx,
        )
        .unwrap();
    assert!(!out.ok);
    assert_eq!(out.content, "old_string appears 2 times, must be unique");
}

#[test]
fn delete_removes_file_and_reports_content() {
    let (_dir, ctx) = ctx();
    let write = WriteFile;
    let delete = DeleteFile;
    let read = ReadFile;
    write
        .run(&json!({"path": "a.txt", "content": "goodbye"}), &ctx)
        .unwrap();
    let out = delete.run(&json!({"path": "a.txt"}), &ctx).unwrap();
    assert!(out.ok, "{}", out.content);
    assert!(out.content.contains("deleted a.txt"));
    assert!(out.content.contains("--- deleted content ---"));
    assert!(out.content.contains("goodbye"));
    let back = read.run(&json!({"path": "a.txt"}), &ctx).unwrap();
    assert!(!back.ok);
}

#[test]
fn delete_nonexistent_returns_false() {
    let (_dir, ctx) = ctx();
    let delete = DeleteFile;
    let out = delete.run(&json!({"path": "nope.txt"}), &ctx).unwrap();
    assert!(!out.ok);
}

#[test]
fn edit_and_delete_outside_workspace_are_refused() {
    let (_dir, ctx) = ctx();
    let edit = EditFile;
    let delete = DeleteFile;
    let err = edit
        .run(
            &json!({"path": "../../etc/passwd", "old_string": "a", "new_string": "b"}),
            &ctx,
        )
        .unwrap_err();
    assert!(matches!(err, VikramError::PolicyDenied { .. }));
    let err = delete
        .run(&json!({"path": "../../etc/passwd"}), &ctx)
        .unwrap_err();
    assert!(matches!(err, VikramError::PolicyDenied { .. }));
}

#[test]
fn edit_and_delete_declare_write_permission() {
    let edit_req = EditFile
        .permission(&json!({"path": "a", "old_string": "b", "new_string": "c"}))
        .unwrap();
    assert_eq!(edit_req.action, ActionKind::Write);
    assert_eq!(edit_req.verb, "fs.edit");
    assert_eq!(edit_req.side_effect, SideEffect::LocalMutation);
    let del_req = DeleteFile.permission(&json!({"path": "a"})).unwrap();
    assert_eq!(del_req.action, ActionKind::Write);
    assert_eq!(del_req.verb, "fs.delete");
    assert_eq!(del_req.side_effect, SideEffect::Destructive);
}

#[test]
fn web_search_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"web.search".to_string()));
}

#[test]
fn git_status_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"git".to_string()));
}

#[test]
fn build_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"build.run".to_string()));
}

#[test]
fn test_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"test.run".to_string()));
}

#[test]
fn git_status_declares_read_permission() {
    let req = Git.permission(&json!({"subcommand": "status"})).unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "git");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn build_declares_execute_permission() {
    let req = BuildRun.permission(&json!({})).unwrap();
    assert_eq!(req.action, ActionKind::Execute);
    assert_eq!(req.verb, "build.run");
    assert_eq!(req.side_effect, SideEffect::LocalMutation);
}

#[test]
fn test_declares_execute_permission() {
    let req = TestRun.permission(&json!({})).unwrap();
    assert_eq!(req.action, ActionKind::Execute);
    assert_eq!(req.verb, "test.run");
    assert_eq!(req.side_effect, SideEffect::LocalMutation);
}

#[test]
fn diagnostics_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(
        registry
            .names()
            .contains(&"diagnostics.collect".to_string())
    );
}

#[test]
fn process_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"process".to_string()));
}

#[test]
fn task_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"task".to_string()));
}

#[test]
fn diagnostics_declares_read_permission() {
    let req = Diagnostics.permission(&json!({})).unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "diagnostics.collect");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn process_declares_execute_permission() {
    let req = Process.permission(&json!({"action": "list"})).unwrap();
    assert_eq!(req.action, ActionKind::Execute);
    assert_eq!(req.verb, "process.run");
    assert_eq!(req.side_effect, SideEffect::LocalMutation);
}

#[test]
fn task_declares_write_permission() {
    let req = Task.permission(&json!({"action": "list"})).unwrap();
    assert_eq!(req.action, ActionKind::Write);
    assert_eq!(req.verb, "task.manage");
    assert_eq!(req.side_effect, SideEffect::LocalMutation);
}

#[test]
fn mcp_tool_name_formats_with_prefix() {
    assert_eq!(mcp_tool_name("fs", "read"), "mcp.fs.read");
    assert_eq!(mcp_tool_name("git", "status"), "mcp.git.status");
}

#[test]
fn mcp_tool_declares_execute_network_permission() {
    use std::sync::Mutex;
    let tool = McpTool {
        server: "fs".into(),
        tool: "read".into(),
        description: "read".into(),
        schema: json!({"type": "object"}),
        registry: Arc::new(Mutex::new(McpRegistry::load(
            crate::config::McpConfig::default().into_specs(),
        ))),
    };
    let spec = tool.spec();
    assert_eq!(spec.name, "mcp.fs.read");
    let req = tool.permission(&json!({})).unwrap();
    assert_eq!(req.action, ActionKind::Execute);
    assert_eq!(req.verb, "mcp.call");
    assert_eq!(req.side_effect, SideEffect::LocalMutation);
}

#[test]
fn web_search_declares_network_permission() {
    let req = WebSearch.permission(&json!({"query": "rust"})).unwrap();
    assert_eq!(req.action, ActionKind::Network);
    assert_eq!(req.verb, "web.search");
    assert_eq!(req.resource, "rust");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn web_search_reports_a_backend_failure_instead_of_an_empty_success() {
    let out = search_tool_output(Err(crate::search::SearchError::Status {
        backend: "bing",
        status: 503,
        detail: "service unavailable".into(),
    }));
    assert!(!out.ok);
    assert!(out.content.contains("503"), "{}", out.content);
    assert!(out.content.contains("web search failed"), "{}", out.content);
}

#[test]
fn web_search_treats_zero_parsed_results_as_a_failure() {
    let out = search_tool_output(Ok(Vec::new()));
    assert!(!out.ok, "{}", out.content);
    assert!(out.content.contains("zero results"), "{}", out.content);
}

#[test]
fn web_search_renders_title_url_and_snippet_for_the_model() {
    let out = search_tool_output(Ok(vec![crate::search::SearchResult {
        title: "Rust".into(),
        url: "https://www.rust-lang.org/".into(),
        snippet: "A fast language.".into(),
    }]));
    assert!(out.ok, "{}", out.content);
    assert!(out.content.contains("1. Rust"), "{}", out.content);
    assert!(
        out.content.contains("URL: https://www.rust-lang.org/"),
        "{}",
        out.content
    );
    assert!(out.content.contains("A fast language."), "{}", out.content);
}

#[test]
fn browser_fetch_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"browser.fetch".to_string()));
}

#[test]
fn code_review_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"code.review".to_string()));
}

#[test]
fn browser_fetch_declares_network_permission() {
    let req = BrowserFetch
        .permission(&json!({"url": "https://example.com"}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Network);
    assert_eq!(req.verb, "browser.fetch");
    assert_eq!(req.resource, "https://example.com");
    assert_eq!(req.side_effect, SideEffect::None);
}

// --- DOM-aware browser tools ---------------------------------------------------------------
//
// No test here starts a browser: `--dump-dom` needs a network page to be worth anything, and a
// unit test must not depend on either. What is tested is the tool surface, the permission
// shape, and the refusals that happen before a browser is ever looked for.

#[test]
fn browser_dom_is_registered() {
    let registry = ToolRegistry::with_builtins();
    let tool = registry.get("browser.dom").expect("browser.dom registered");
    assert_eq!(tool.spec().name, "browser.dom");
    assert!(tool.spec().schema["properties"]["url"].is_object());
}

#[test]
fn browser_links_is_registered() {
    let registry = ToolRegistry::with_builtins();
    let tool = registry
        .get("browser.links")
        .expect("browser.links registered");
    assert_eq!(tool.spec().name, "browser.links");
    assert!(tool.spec().schema["properties"]["url"].is_object());
}

#[test]
fn browser_screenshot_is_registered() {
    let registry = ToolRegistry::with_builtins();
    let tool = registry
        .get("browser.screenshot")
        .expect("browser.screenshot registered");
    assert_eq!(tool.spec().name, "browser.screenshot");
    for field in ["url", "path"] {
        assert!(
            tool.spec().schema["properties"][field].is_object(),
            "browser.screenshot must document `{field}`"
        );
    }
}

#[test]
fn browser_dom_declares_network_permission() {
    let req = BrowserDom
        .permission(&json!({"url": "https://example.com"}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Network);
    assert_eq!(req.verb, "browser.dom");
    assert_eq!(req.resource, "https://example.com");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn browser_links_declares_network_permission() {
    let req = BrowserLinks
        .permission(&json!({"url": "https://example.com"}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Network);
    assert_eq!(req.verb, "browser.links");
    assert_eq!(req.resource, "https://example.com");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn browser_screenshot_declares_a_local_mutation() {
    let req = BrowserScreenshot
        .permission(&json!({"url": "https://example.com", "path": "shots/home.png"}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Network);
    assert_eq!(req.verb, "browser.screenshot");
    assert_eq!(req.resource, "shots/home.png");
    assert_eq!(req.side_effect, SideEffect::LocalMutation);
}

#[test]
fn browser_tools_refuse_a_permission_without_a_url() {
    assert!(BrowserDom.permission(&json!({})).is_err());
    assert!(BrowserLinks.permission(&json!({})).is_err());
    assert!(
        BrowserScreenshot
            .permission(&json!({"path": "shot.png"}))
            .is_err()
    );
    assert!(
        BrowserScreenshot
            .permission(&json!({"url": "https://example.com"}))
            .is_err()
    );
}

#[test]
fn browser_screenshot_refuses_a_path_outside_the_workspace() {
    let (_dir, ctx) = ctx();
    let err = BrowserScreenshot
        .run(
            &json!({"url": "https://example.com", "path": "../escape.png"}),
            &ctx,
        )
        .unwrap_err();
    assert!(matches!(err, VikramError::PolicyDenied { .. }));
}

// --- interactive browser tools -------------------------------------------------------------
//
// As above: nothing here launches a browser. What is tested is the tool surface, the permission
// shape each call declares, and the refusals that happen before a browser is looked for.

#[test]
fn interactive_browser_tools_are_registered() {
    let registry = ToolRegistry::with_builtins();
    let names = registry.names();
    for name in [
        "browser.open",
        "browser.eval",
        "browser.click",
        "browser.type",
        "browser.scroll",
        "browser.text",
        "browser.close",
        "browser.sessions",
    ] {
        assert!(
            names.contains(&name.to_string()),
            "{name} is not registered"
        );
    }
}

#[test]
fn interactive_browser_tools_document_the_arguments_they_need() {
    let registry = ToolRegistry::with_builtins();
    let expected: [(&str, &[&str]); 8] = [
        ("browser.open", &["url"]),
        ("browser.eval", &["session", "js"]),
        ("browser.click", &["session", "selector"]),
        ("browser.type", &["session", "selector", "text"]),
        ("browser.scroll", &["session"]),
        ("browser.text", &["session"]),
        ("browser.close", &["session"]),
        ("browser.sessions", &[]),
    ];
    for (name, fields) in expected {
        let tool = registry
            .get(name)
            .unwrap_or_else(|| panic!("{name} registered"));
        let schema = tool.spec().schema;
        for field in fields {
            assert!(
                schema["properties"][*field].is_object(),
                "{name} must document `{field}`"
            );
        }
    }
}

#[test]
fn interactive_browser_tools_declare_their_permission_shapes() {
    let registry = ToolRegistry::with_builtins();
    let cases: [(&str, Value, ActionKind, SideEffect); 8] = [
        (
            "browser.open",
            json!({"url": "https://example.com"}),
            ActionKind::Network,
            SideEffect::None,
        ),
        (
            "browser.eval",
            json!({"session": "browser-1", "js": "1"}),
            ActionKind::Execute,
            SideEffect::None,
        ),
        (
            "browser.click",
            json!({"session": "browser-1", "selector": "#go"}),
            ActionKind::Execute,
            SideEffect::LocalMutation,
        ),
        (
            "browser.type",
            json!({"session": "browser-1", "selector": "#q", "text": "hi"}),
            ActionKind::Execute,
            SideEffect::LocalMutation,
        ),
        (
            "browser.scroll",
            json!({"session": "browser-1", "y": 10}),
            ActionKind::Execute,
            SideEffect::None,
        ),
        (
            "browser.text",
            json!({"session": "browser-1"}),
            ActionKind::Read,
            SideEffect::None,
        ),
        (
            "browser.close",
            json!({"session": "browser-1"}),
            ActionKind::Execute,
            SideEffect::None,
        ),
        (
            "browser.sessions",
            json!({}),
            ActionKind::Read,
            SideEffect::None,
        ),
    ];
    for (name, args, action, side_effect) in cases {
        let tool = registry
            .get(name)
            .unwrap_or_else(|| panic!("{name} registered"));
        let req = tool
            .permission(&args)
            .unwrap_or_else(|e| panic!("{name} refused its own sample: {e}"));
        assert_eq!(req.action, action, "{name}");
        assert_eq!(req.verb, name, "{name}");
        assert_eq!(req.side_effect, side_effect, "{name}");
        assert_eq!(req.originating_trust, Trust::Model, "{name}");
    }
}

#[test]
fn an_interaction_names_its_session_as_the_resource() {
    let registry = ToolRegistry::with_builtins();
    for name in ["browser.click", "browser.type", "browser.text"] {
        let tool = registry.get(name).unwrap();
        let req = tool
            .permission(&json!({"session": "browser-7", "selector": "#a", "text": "t"}))
            .unwrap();
        assert_eq!(req.resource, "browser-7", "{name}");
    }
    let req = BrowserOpen
        .permission(&json!({"url": "https://example.com/x"}))
        .unwrap();
    assert_eq!(req.resource, "https://example.com/x");
    assert_eq!(req.action, ActionKind::Network);
}

#[test]
fn interactive_browser_tools_refuse_a_permission_without_their_arguments() {
    assert!(BrowserOpen.permission(&json!({})).is_err());
    assert!(BrowserEval.permission(&json!({"js": "1"})).is_err());
    assert!(BrowserClick.permission(&json!({"selector": "#a"})).is_err());
    // The session is all a permission request needs: a call with no `text` is still described
    // to policy, and the missing argument is caught when the tool runs.
    assert!(
        BrowserType
            .permission(&json!({"session": "browser-1", "selector": "#a"}))
            .is_ok()
    );
    assert!(BrowserScroll.permission(&json!({})).is_err());
    assert!(BrowserText.permission(&json!({})).is_err());
    assert!(BrowserClose.permission(&json!({})).is_err());
    // `browser.sessions` needs nothing at all.
    assert!(BrowserSessions.permission(&json!({})).is_ok());
}

#[test]
fn an_interaction_with_an_unknown_session_fails_without_a_browser() {
    let tool = ToolRegistry::with_builtins()
        .get("browser.text")
        .expect("browser.text registered");
    let (_dir, ctx) = ctx();
    let output = tool
        .run(&json!({"session": "browser-does-not-exist"}), &ctx)
        .unwrap();
    assert!(!output.ok, "{output:?}");
    assert!(
        output.content.contains("unknown browser session"),
        "{}",
        output.content
    );
}

#[test]
fn an_interaction_with_an_empty_selector_is_refused_before_the_browser() {
    let (_dir, ctx) = ctx();
    let output = BrowserClick
        .run(
            &json!({"session": "browser-does-not-exist", "selector": "  "}),
            &ctx,
        )
        .unwrap();
    assert!(!output.ok, "{output:?}");
    assert!(
        output.content.contains("must not be empty"),
        "{}",
        output.content
    );
}

#[test]
fn a_scroll_without_an_offset_is_refused() {
    let (_dir, ctx) = ctx();
    let output = BrowserScroll
        .run(&json!({"session": "browser-does-not-exist"}), &ctx)
        .unwrap();
    assert!(!output.ok, "{output:?}");
    assert!(
        output.content.contains("both `x` and `y` are zero"),
        "{}",
        output.content
    );
}

#[test]
fn browser_sessions_lists_nothing_when_nothing_is_open() {
    let (_dir, ctx) = ctx();
    let output = BrowserSessions.run(&json!({}), &ctx).unwrap();
    assert!(output.ok, "{output:?}");
    assert!(
        output.content.contains("no browser sessions"),
        "{}",
        output.content
    );
}

#[test]
fn code_review_declares_read_permission() {
    let req = CodeReview.permission(&json!({})).unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "code.review");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn ask_user_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"ask_user".to_string()));
}

#[test]
fn permission_check_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"permission.check".to_string()));
}

#[test]
fn ask_user_declares_read_permission() {
    let req = AskUser.permission(&json!({"question": "q"})).unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "ask_user");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn ask_user_returns_structured_question() {
    let (_dir, ctx) = ctx();
    let out = AskUser
        .run(&json!({"question": "What is your name?"}), &ctx)
        .unwrap();
    assert!(out.ok, "{}", out.content);
    assert!(out.content.starts_with("QUESTION:"));
    assert!(out.content.contains("What is your name?"));
    assert!(out.content.contains("Status: awaiting_user_response"));
}

#[test]
fn ask_user_includes_choices_as_json() {
    let (_dir, ctx) = ctx();
    let out = AskUser
        .run(
            &json!({"question": "Pick one", "choices": ["yes", "no"]}),
            &ctx,
        )
        .unwrap();
    assert!(out.ok, "{}", out.content);
    assert!(out.content.contains("yes"));
    assert!(out.content.contains("no"));
    assert!(out.content.contains("Choices: [\"yes\",\"no\"]"));
}

#[test]
fn ask_user_generates_unique_request_ids() {
    let (_dir, ctx) = ctx();
    let a = AskUser.run(&json!({"question": "q1"}), &ctx).unwrap();
    let b = AskUser.run(&json!({"question": "q2"}), &ctx).unwrap();
    let id_a = a
        .content
        .lines()
        .next()
        .unwrap()
        .trim_start_matches("QUESTION:");
    let id_b = b
        .content
        .lines()
        .next()
        .unwrap()
        .trim_start_matches("QUESTION:");
    assert_ne!(id_a, id_b);
    assert!(!id_a.is_empty());
    assert!(!id_b.is_empty());
}

#[test]
fn permission_check_declares_read_permission() {
    let req = PermissionCheck
        .permission(&json!({"verb": "v", "resource": "r"}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "permission.check");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn hive_create_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"hive.create".to_string()));
}

#[test]
fn hive_list_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"hive.list".to_string()));
}

#[test]
fn hive_show_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"hive.show".to_string()));
}

#[test]
fn hive_delegate_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"hive.delegate".to_string()));
}

#[test]
fn hive_delete_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"hive.delete".to_string()));
}

#[test]
fn hive_create_declares_write_permission() {
    let req = HiveCreate.permission(&json!({"name": "t"})).unwrap();
    assert_eq!(req.action, ActionKind::Write);
    assert_eq!(req.verb, "hive.create");
    assert_eq!(req.side_effect, SideEffect::LocalMutation);
}

#[test]
fn hive_list_declares_read_permission() {
    let req = HiveList.permission(&json!({})).unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "hive.list");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn hive_show_declares_read_permission() {
    let req = HiveShow.permission(&json!({"name": "t"})).unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "hive.show");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn hive_delegate_declares_delegate_permission() {
    let req = HiveDelegate
        .permission(&json!({"team": "t", "task": "x"}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Delegate);
    assert_eq!(req.verb, "hive.delegate");
    assert_eq!(req.side_effect, SideEffect::LocalMutation);
}

#[test]
fn hive_delete_declares_write_permission() {
    let req = HiveDelete.permission(&json!({"name": "t"})).unwrap();
    assert_eq!(req.action, ActionKind::Write);
    assert_eq!(req.verb, "hive.delete");
    assert_eq!(req.side_effect, SideEffect::Destructive);
}

#[test]
fn bot_list_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"bot.list".to_string()));
}

#[test]
fn bot_show_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"bot.show".to_string()));
}

#[test]
fn bot_create_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"bot.create".to_string()));
}

#[test]
fn agent_list_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"agent.list".to_string()));
}

#[test]
fn agent_show_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"agent.show".to_string()));
}

#[test]
fn bot_list_declares_read_permission() {
    let req = BotList.permission(&json!({})).unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "bot.list");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn bot_show_declares_read_permission() {
    let req = BotShow.permission(&json!({"name": "t"})).unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "bot.show");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn bot_create_declares_write_permission() {
    let req = BotCreate
        .permission(&json!({"name": "t", "role": "r", "description": "d"}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Write);
    assert_eq!(req.verb, "bot.create");
    assert_eq!(req.side_effect, SideEffect::LocalMutation);
}

#[test]
fn agent_list_declares_read_permission() {
    let req = AgentList.permission(&json!({})).unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "agent.list");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn agent_show_declares_read_permission() {
    let req = AgentShow.permission(&json!({"name": "t"})).unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "agent.show");
    assert_eq!(req.side_effect, SideEffect::None);
}

// --- language server tools -----------------------------------------------------------------
//
// No test here starts a language server: servers may not be installed, and a unit test that
// depended on rust-analyzer would pass or fail on the tester's machine. What is tested is the
// part that must never change — the tool surface and the permission shape — plus the failure
// paths that need no server at all.

#[test]
fn lsp_hover_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"lsp.hover".to_string()));
}

#[test]
fn lsp_definition_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"lsp.definition".to_string()));
}

#[test]
fn lsp_references_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"lsp.references".to_string()));
}

#[test]
fn lsp_symbols_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"lsp.symbols".to_string()));
}

#[test]
fn lsp_diagnostics_is_registered() {
    let registry = ToolRegistry::with_builtins();
    assert!(registry.names().contains(&"lsp.diagnostics".to_string()));
}

#[test]
fn lsp_position_tools_require_a_path_a_line_and_a_column() {
    let registry = ToolRegistry::with_builtins();
    for name in ["lsp.hover", "lsp.definition", "lsp.references"] {
        let spec = registry.get(name).expect(name).spec();
        assert_eq!(spec.schema["type"], "object");
        let required = spec.schema["required"].as_array().expect("required list");
        for field in ["path", "line", "column"] {
            assert!(
                required.iter().any(|value| value == field),
                "`{name}` must require `{field}`"
            );
            assert!(
                spec.schema["properties"][field].is_object(),
                "`{name}` must document `{field}`"
            );
        }
    }
}

#[test]
fn lsp_file_tools_require_only_a_path() {
    let registry = ToolRegistry::with_builtins();
    for name in ["lsp.symbols", "lsp.diagnostics"] {
        let spec = registry.get(name).expect(name).spec();
        let required = spec.schema["required"].as_array().expect("required list");
        assert_eq!(required.len(), 1, "`{name}` requires exactly `path`");
        assert_eq!(required[0], "path");
    }
}

#[test]
fn lsp_hover_declares_read_permission() {
    let req = LspHover
        .permission(&json!({"path": "src/main.rs", "line": 3, "column": 7}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "lsp.hover");
    assert_eq!(req.resource, "src/main.rs");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn lsp_definition_declares_read_permission() {
    let req = LspDefinition
        .permission(&json!({"path": "src/main.rs", "line": 3, "column": 7}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "lsp.definition");
    assert_eq!(req.resource, "src/main.rs");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn lsp_references_declares_read_permission() {
    let req = LspReferences
        .permission(&json!({"path": "src/main.rs", "line": 3, "column": 7}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "lsp.references");
    assert_eq!(req.resource, "src/main.rs");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn lsp_symbols_declares_read_permission() {
    let req = LspSymbols
        .permission(&json!({"path": "src/main.rs"}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "lsp.symbols");
    assert_eq!(req.resource, "src/main.rs");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn lsp_diagnostics_declares_read_permission() {
    let req = LspDiagnostics
        .permission(&json!({"path": "src/main.rs"}))
        .unwrap();
    assert_eq!(req.action, ActionKind::Read);
    assert_eq!(req.verb, "lsp.diagnostics");
    assert_eq!(req.resource, "src/main.rs");
    assert_eq!(req.side_effect, SideEffect::None);
}

#[test]
fn lsp_tools_refuse_a_permission_without_a_path() {
    assert!(
        LspHover
            .permission(&json!({"line": 0, "column": 0}))
            .is_err()
    );
    assert!(LspSymbols.permission(&json!({})).is_err());
    assert!(LspDiagnostics.permission(&json!({})).is_err());
}

#[test]
fn lsp_tools_refuse_paths_outside_the_workspace() {
    let (_dir, ctx) = ctx();
    let err = LspHover
        .run(
            &json!({"path": "../../etc/passwd", "line": 0, "column": 0}),
            &ctx,
        )
        .unwrap_err();
    assert!(matches!(err, VikramError::PolicyDenied { .. }));
    let err = LspSymbols
        .run(&json!({"path": "../../etc/passwd"}), &ctx)
        .unwrap_err();
    assert!(matches!(err, VikramError::PolicyDenied { .. }));
    let err = LspDiagnostics
        .run(&json!({"path": "/etc/passwd"}), &ctx)
        .unwrap_err();
    assert!(matches!(err, VikramError::PolicyDenied { .. }));
}

#[test]
fn lsp_tools_report_a_missing_file_without_starting_a_server() {
    let (_dir, ctx) = ctx();
    let out = LspDefinition
        .run(&json!({"path": "nope.rs", "line": 0, "column": 0}), &ctx)
        .unwrap();
    assert!(!out.ok, "{}", out.content);
    assert!(
        out.content.contains("is not a readable file"),
        "{}",
        out.content
    );
    let out = LspSymbols.run(&json!({"path": "nope.rs"}), &ctx).unwrap();
    assert!(!out.ok, "{}", out.content);
    assert!(
        out.content.contains("is not a readable file"),
        "{}",
        out.content
    );
}

#[test]
fn lsp_tools_reject_a_missing_or_non_integer_position() {
    let (_dir, ctx) = ctx();
    WriteFile
        .run(&json!({"path": "a.rs", "content": "fn main() {}"}), &ctx)
        .unwrap();
    let err = LspHover
        .run(&json!({"path": "a.rs", "column": 0}), &ctx)
        .unwrap_err();
    assert!(matches!(err, VikramError::ProviderRejected(_)), "{err:?}");
    let err = LspHover
        .run(&json!({"path": "a.rs", "line": "0", "column": 0}), &ctx)
        .unwrap_err();
    assert!(matches!(err, VikramError::ProviderRejected(_)), "{err:?}");
}

#[test]
fn lsp_on_an_unsupported_extension_names_the_override_variable() {
    let (_dir, ctx) = ctx();
    WriteFile
        .run(&json!({"path": "notes.txt", "content": "hello"}), &ctx)
        .unwrap();
    let out = LspHover
        .run(&json!({"path": "notes.txt", "line": 0, "column": 0}), &ctx)
        .unwrap();
    assert!(!out.ok, "{}", out.content);
    assert!(
        out.content.contains("no language server for .txt"),
        "{}",
        out.content
    );
    assert!(out.content.contains("VIKRAM_LSP_TXT"), "{}", out.content);
}

// Real execution, not shape checks: each of these runs the tool and then looks at what it did
// to the filesystem, the shell or the network, so a tool that answers but does nothing fails.
#[test]
fn the_file_tools_create_edit_list_and_delete_for_real() {
    let (dir, ctx) = ctx();
    let written = WriteFile
        .run(
            &json!({"path": "notes/todo.md", "content": "# one\n"}),
            &ctx,
        )
        .unwrap();
    assert!(written.ok, "{}", written.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes/todo.md")).unwrap(),
        "# one\n",
        "the bytes are on disk, not only in the answer"
    );

    let edited = EditFile
        .run(
            &json!({"path": "notes/todo.md", "old_string": "# one", "new_string": "# two"}),
            &ctx,
        )
        .unwrap();
    assert!(edited.ok, "{}", edited.content);
    assert_eq!(
        ReadFile
            .run(&json!({"path": "notes/todo.md"}), &ctx)
            .unwrap()
            .content,
        "# two\n",
        "reading back shows the edit, not the original"
    );

    let listed = ListDir.run(&json!({"path": "notes"}), &ctx).unwrap();
    assert!(listed.content.contains("todo.md"), "{}", listed.content);

    let deleted = DeleteFile
        .run(&json!({"path": "notes/todo.md"}), &ctx)
        .unwrap();
    assert!(deleted.ok, "{}", deleted.content);
    assert!(
        !dir.path().join("notes/todo.md").exists(),
        "the file is gone from disk, not just from the answer"
    );
}

#[test]
fn an_edit_that_cannot_find_its_old_string_fails_instead_of_guessing() {
    let (dir, ctx) = ctx();
    WriteFile
        .run(&json!({"path": "a.txt", "content": "alpha\n"}), &ctx)
        .unwrap();
    let out = EditFile.run(
        &json!({"path": "a.txt", "old_string": "nothing like this", "new_string": "beta"}),
        &ctx,
    );
    let failed = match out {
        Err(_) => true,
        Ok(output) => !output.ok,
    };
    assert!(failed, "an edit with no match must not report success");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n",
        "a failed edit leaves the file alone"
    );
}

#[test]
fn a_shell_command_returns_its_real_output_and_exit_code() {
    let (_dir, ctx) = ctx();
    let out = RunShell
        .run(&json!({"command": "printf hello; exit 3"}), &ctx)
        .unwrap();
    assert!(out.content.contains("hello"), "{}", out.content);
    assert!(out.content.contains("[exit 3]"), "{}", out.content);
    assert!(!out.ok, "a non-zero exit is a failure, not a success");
}

#[test]
fn search_finds_the_text_the_file_tools_wrote() {
    let (_dir, ctx) = ctx();
    WriteFile
        .run(
            &json!({"path": "src/lib.rs", "content": "pub fn marker_xyz() {}\n"}),
            &ctx,
        )
        .unwrap();
    let out = SearchText
        .run(&json!({"query": "marker_xyz"}), &ctx)
        .unwrap();
    assert!(out.ok, "{}", out.content);
    assert!(out.content.contains("lib.rs"), "{}", out.content);
}

#[test]
fn git_status_runs_against_a_real_repository() {
    let (dir, ctx) = ctx();
    let init = RunShell
        .run(&json!({"command": "git init -q ."}), &ctx)
        .unwrap();
    assert!(init.ok, "{}", init.content);
    WriteFile
        .run(&json!({"path": "untracked.txt", "content": "x"}), &ctx)
        .unwrap();
    let out = Git.run(&json!({"subcommand": "status"}), &ctx).unwrap();
    assert!(out.ok, "{}", out.content);
    assert!(
        out.content.contains("untracked.txt"),
        "status lists the file that is not committed yet: {}",
        out.content
    );
    assert!(dir.path().join(".git").exists());
}

#[test]
fn the_terminal_tool_drives_a_real_pty() {
    let (_dir, ctx) = ctx();
    let opened = TerminalTool
        .run(&json!({"action": "open", "session": "smoke"}), &ctx)
        .unwrap();
    assert!(opened.ok, "{}", opened.content);
    TerminalTool
        .run(
            &json!({"action": "write", "session": "smoke", "input": "echo pty-works\n"}),
            &ctx,
        )
        .unwrap();
    let mut seen = String::new();
    for _ in 0..40 {
        let read = TerminalTool
            .run(&json!({"action": "read", "session": "smoke"}), &ctx)
            .unwrap();
        seen.push_str(&read.content);
        if seen.contains("pty-works") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = TerminalTool.run(&json!({"action": "close", "session": "smoke"}), &ctx);
    assert!(
        seen.contains("pty-works"),
        "a real PTY echoes back what was typed: {seen}"
    );
}

#[test]
fn web_search_returns_sources_or_says_why_it_could_not() {
    let (_dir, ctx) = ctx();
    let out = WebSearch
        .run(
            &json!({"query": "rust programming language", "limit": 3}),
            &ctx,
        )
        .unwrap();
    if out.ok {
        assert!(
            out.content.contains("http"),
            "a successful search carries links: {}",
            out.content
        );
    } else {
        assert!(
            out.content.trim().len() > 10,
            "a failed search explains itself: {}",
            out.content
        );
    }
}

#[test]
fn memory_and_tasks_round_trip_through_their_own_store() {
    let dir = tempfile::tempdir().unwrap();
    let _env = crate::config::lock_security_env();
    unsafe { std::env::set_var("VIKRAM_HOME", dir.path().join("home")) };
    let ctx = ToolContext {
        workspace_root: dir.path().to_path_buf(),
        sandbox: crate::sandbox::Sandbox::disabled("test context"),
        spill: crate::spill::Spill::disabled(),
        runtime: None,
        session: None,
    };

    let saved = MemorySave
        .run(&json!({"key": "tool-probe", "value": "remembered"}), &ctx)
        .unwrap();
    assert!(saved.ok, "{}", saved.content);
    let found = MemorySearch
        .run(&json!({"query": "remembered"}), &ctx)
        .unwrap();
    assert!(
        found.content.contains("tool-probe"),
        "a saved key is findable by what it says: {}",
        found.content
    );
    let listed = MemoryList.run(&json!({}), &ctx).unwrap();
    assert!(listed.content.contains("tool-probe"), "{}", listed.content);
    let removed = MemoryDelete
        .run(&json!({"key": "tool-probe"}), &ctx)
        .unwrap();
    assert!(removed.ok, "{}", removed.content);
    let after = MemoryList.run(&json!({}), &ctx).unwrap();
    assert!(
        !after.content.contains("tool-probe"),
        "a deleted key is gone from the listing: {}",
        after.content
    );

    let created = Task
        .run(&json!({"action": "create", "title": "probe"}), &ctx)
        .unwrap();
    assert!(created.ok, "{}", created.content);
    let tasks = Task.run(&json!({"action": "list"}), &ctx).unwrap();
    assert!(tasks.content.contains("probe"), "{}", tasks.content);
}

#[test]
fn the_host_inspection_tools_answer_without_a_model() {
    let (_dir, ctx) = ctx();
    let answers = vec![
        (
            "process",
            Process.run(&json!({"action": "list"}), &ctx).unwrap(),
        ),
        ("skill.list", SkillList.run(&json!({}), &ctx).unwrap()),
        ("plugin.list", PluginList.run(&json!({}), &ctx).unwrap()),
    ];
    for (name, out) in answers {
        assert!(out.ok, "{name} failed: {}", out.content);
        assert!(!out.content.trim().is_empty(), "{name} answered nothing");
    }
}
