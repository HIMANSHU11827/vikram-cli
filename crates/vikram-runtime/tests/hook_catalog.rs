//! The shipped hook catalog is data the runtime must be able to load, not documentation.
//!
//! These tests read the same `modules/` directories `vikram modules install` copies, so a manifest
//! that stops validating or a rule that targets the wrong point fails here before release.

use std::path::{Path, PathBuf};

use serde_json::json;
use vikram_runtime::hooks::{HookContext, HookPoint, HookRegistry};
use vikram_runtime::plugins::{PluginManifest, PluginRegistry, parse_permissions};

fn catalog() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create module directory");
    for entry in std::fs::read_dir(from)
        .expect("read module directory")
        .flatten()
    {
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            std::fs::copy(&path, &target).expect("copy module file");
        }
    }
}

#[test]
fn the_catalog_holds_one_hundred_valid_hook_modules() {
    let mut count = 0;
    for entry in std::fs::read_dir(catalog())
        .expect("the modules directory ships with the workspace")
        .flatten()
    {
        let dir = entry.path();
        if !dir.is_dir() || !dir.join("plugin.json").is_file() {
            continue;
        }
        count += 1;
        let manifest =
            PluginManifest::from_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        let declared = manifest
            .components
            .as_ref()
            .and_then(|components| components.get("hooks"))
            .unwrap_or_else(|| panic!("{} declares no hook component", manifest.id()));
        assert!(
            !manifest.hook_rules.is_empty(),
            "{} declares a hook component but no rules",
            manifest.id()
        );
        let points: Vec<HookPoint> = declared
            .iter()
            .filter_map(|raw| HookPoint::parse(raw))
            .collect();
        assert_eq!(
            points.len(),
            declared.len(),
            "{} declares an unknown hook point: {declared:?}",
            manifest.id()
        );
        let granted = parse_permissions(&manifest.permissions);
        for rule in &manifest.hook_rules {
            rule.validate()
                .unwrap_or_else(|e| panic!("{}: {e}", manifest.id()));
            let point = rule.point().expect("validated above");
            assert!(
                points.contains(&point),
                "{} has a rule for {}, which it does not declare",
                manifest.id(),
                point.as_str()
            );
            assert!(
                granted.contains(&point.permission()),
                "{} lacks the `{}` permission for {}",
                manifest.id(),
                point.permission().as_str(),
                point.as_str()
            );
        }
    }
    assert_eq!(count, 100, "the catalog is exactly 100 modules");
}

#[test]
fn every_module_registers_and_none_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let plugins_dir = temp.path().join("plugins");
    copy_tree(&catalog(), &plugins_dir);

    let registry = PluginRegistry::scan(&plugins_dir);
    assert_eq!(registry.plugins().len(), 100, "every directory loads");
    let hooks = HookRegistry::from_plugins(&registry);
    assert!(
        hooks.rejected().is_empty(),
        "manifests the runtime would refuse: {:?}",
        hooks.rejected()
    );
    let mut plugins: Vec<String> = hooks
        .hooks()
        .iter()
        .map(|hook| hook.plugin().to_owned())
        .collect();
    plugins.sort();
    plugins.dedup();
    assert_eq!(plugins.len(), 100, "every module contributes a hook");
}

#[test]
fn catalog_modules_change_each_stage_of_a_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    let plugins_dir = temp.path().join("plugins");
    copy_tree(&catalog(), &plugins_dir);
    let hooks = HookRegistry::from_plugins(&PluginRegistry::scan(&plugins_dir));

    let refused = hooks.dispatch(
        &HookContext::new(HookPoint::ToolCall)
            .with_text("shell.run")
            .with_tool("shell.run", json!({"command": "rm -rf /"})),
    );
    assert!(
        refused
            .effect
            .deny
            .as_deref()
            .is_some_and(|reason| reason.contains("root filesystem")),
        "the most specific guard must win: {:?}",
        refused.effect
    );

    let context = hooks.dispatch(&HookContext::new(HookPoint::ContextBuild).with_text("hello"));
    assert!(
        context
            .effect
            .append
            .iter()
            .any(|text| text.contains("ISO-8601")),
        "a context module must reach the prefix"
    );

    let result = hooks.dispatch(
        &HookContext::new(HookPoint::ToolResult)
            .with_tool("test.run", json!({}))
            .with_result("test result: FAILED. 0 passed", false),
    );
    assert!(
        result
            .effect
            .content
            .as_deref()
            .is_some_and(|content| content.contains("Tests failed")),
        "a result module must annotate the output"
    );

    let model = hooks.dispatch(&HookContext::new(HookPoint::ModelRequest));
    assert!(
        model.effect.max_output_tokens.is_some(),
        "a model module must set the output cap"
    );

    let turn = hooks.dispatch(
        &HookContext::new(HookPoint::TurnStart).with_text("please deploy to production now"),
    );
    assert!(
        turn.effect.deny.is_some(),
        "a turn module must be able to refuse the request"
    );
}

#[test]
fn disabling_a_module_restores_the_default_behaviour() {
    let temp = tempfile::tempdir().expect("temp dir");
    let plugins_dir = temp.path().join("plugins");
    copy_tree(&catalog(), &plugins_dir);
    PluginRegistry::scan(&plugins_dir)
        .disable("guard-rm-recursive")
        .expect("the module is installed");
    let hooks = HookRegistry::from_plugins(&PluginRegistry::scan(&plugins_dir));

    let call = hooks.dispatch(
        &HookContext::new(HookPoint::ToolCall)
            .with_text("shell.run")
            .with_tool("shell.run", json!({"command": "rm -rf ./target"})),
    );
    assert!(
        call.effect.deny.is_none(),
        "a disabled module must not modify the call"
    );
}
