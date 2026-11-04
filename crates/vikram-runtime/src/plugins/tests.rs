use super::*;
use std::path::Path;

fn write_plugin(root: &Path, dir: &str, body: &str) {
    let path = root.join(dir);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("plugin.json"), body).unwrap();
}

#[test]
fn installed_and_loaded_are_distinct_counts() {
    let dir = tempfile::tempdir().unwrap();
    write_plugin(
        dir.path(),
        "good",
        r#"{"name":"good","version":"1.0","entry":"main.js"}"#,
    );
    std::fs::write(dir.path().join("good/main.js"), "//").unwrap();
    write_plugin(dir.path(), "off", r#"{"name":"off","enabled":false}"#);
    write_plugin(dir.path(), "broken", "{not json");
    std::fs::write(dir.path().join("broken.txt"), "x").unwrap();

    let registry = PluginRegistry::scan(dir.path());
    assert_eq!(registry.installed(), 3);
    assert_eq!(registry.loaded(), 1);
}

#[test]
fn a_missing_entry_point_is_not_loaded() {
    let dir = tempfile::tempdir().unwrap();
    write_plugin(dir.path(), "p", r#"{"name":"p","entry":"missing.js"}"#);
    let registry = PluginRegistry::scan(dir.path());
    assert_eq!(registry.installed(), 1);
    assert_eq!(registry.loaded(), 0);
    assert!(registry.plugins()[0].error.is_some());
}

#[test]
fn a_missing_directory_is_an_empty_registry_not_an_error() {
    let registry = PluginRegistry::scan(Path::new("/nonexistent/vikram/plugins"));
    assert_eq!(registry.installed(), 0);
    assert_eq!(registry.loaded(), 0);
}

// ---- manifests -------------------------------------------------------

#[test]
fn legacy_manifest_still_parses() {
    let manifest = PluginManifest::parse(
        r#"{"name":"old","version":"1.0.0","entry":"main.js","enabled":true}"#,
    )
    .unwrap();
    assert_eq!(manifest.name, "old");
    assert_eq!(manifest.version, "1.0.0");
    assert_eq!(manifest.entry, "main.js");
    assert!(manifest.enabled);
    // id falls back to name; everything else takes its default.
    assert_eq!(manifest.id(), "old");
    assert!(manifest.description.is_empty());
    assert!(manifest.dependencies.is_empty());
    assert!(manifest.components.is_none());
}

#[test]
fn legacy_manifest_without_enabled_is_enabled() {
    let manifest = PluginManifest::parse(r#"{"name":"p","entry":"x.sh"}"#).unwrap();
    assert!(manifest.enabled);
}

#[test]
fn rich_manifest_parses_every_field() {
    let raw = r#"{
            "id": "acme.tools",
            "name": "acme",
            "version": "1.4.2",
            "description": "acme tools",
            "author": "acme",
            "license": "MIT",
            "homepage": "https://example.com",
            "repository": "https://github.com/acme/tools",
            "min_vikram_version": "0.1.0",
            "api_version": "1",
            "components": {"tools": ["run"], "skills": ["s1"]},
            "dependencies": [{"name": "core", "version_req": "^1.0.0"}],
            "permissions": ["fs.write", "network"],
            "entry": "bin/main.sh",
            "enabled": true
        }"#;
    let manifest = PluginManifest::parse(raw).unwrap();
    assert_eq!(manifest.id(), "acme.tools");
    assert_eq!(manifest.name, "acme");
    assert_eq!(manifest.version, "1.4.2");
    assert_eq!(manifest.description, "acme tools");
    assert_eq!(manifest.author, "acme");
    assert_eq!(manifest.license, "MIT");
    assert_eq!(manifest.homepage.as_deref(), Some("https://example.com"));
    assert_eq!(
        manifest.repository.as_deref(),
        Some("https://github.com/acme/tools")
    );
    assert_eq!(manifest.min_vikram_version.as_deref(), Some("0.1.0"));
    assert_eq!(manifest.api_version.as_deref(), Some("1"));
    let components = manifest.components.as_ref().unwrap();
    assert_eq!(components["tools"], vec!["run".to_string()]);
    assert_eq!(components["skills"], vec!["s1".to_string()]);
    assert_eq!(manifest.dependencies.len(), 1);
    assert_eq!(manifest.dependencies[0].name, "core");
    assert_eq!(manifest.dependencies[0].version_req, "^1.0.0");
    assert_eq!(manifest.permissions, vec!["fs.write", "network"]);
    assert_eq!(manifest.entry, "bin/main.sh");
    assert!(manifest.enabled);
}

#[test]
fn scan_reads_the_rich_manifest() {
    let dir = tempfile::tempdir().unwrap();
    write_plugin(
        dir.path(),
        "rich",
        r#"{"id":"rich.id","name":"rich","version":"2.0.0","entry":"main.sh","enabled":false}"#,
    );
    let registry = PluginRegistry::scan(dir.path());
    assert_eq!(registry.plugins()[0].name, "rich");
    assert_eq!(registry.plugins()[0].version.as_deref(), Some("2.0.0"));
    assert_eq!(registry.plugins()[0].id(), "rich.id");
    assert!(!registry.plugins()[0].loaded);
}

// ---- semver ----------------------------------------------------------

#[test]
fn semver_shape_is_checked_strictly() {
    assert!(is_semver("1.2.3"));
    assert!(!is_semver("1.2"));
    assert!(!is_semver("1"));
    assert!(!is_semver("v1.2.3"));
    assert!(!is_semver(""));
    assert!(is_semver("1.2.3-beta.1"));
}

#[test]
fn exact_requirements_match_only_the_same_version() {
    assert!(requirement_satisfied("1.2.3", "1.2.3"));
    assert!(requirement_satisfied("1.2.3", "=1.2.3"));
    assert!(!requirement_satisfied("1.2.4", "1.2.3"));
    assert!(!requirement_satisfied("2.0.0", "1.2.3"));
}

#[test]
fn caret_requirements_stay_within_a_compatible_major() {
    assert!(requirement_satisfied("1.2.3", "^1.2.3"));
    assert!(requirement_satisfied("1.9.0", "^1.2.3"));
    assert!(!requirement_satisfied("2.0.0", "^1.2.3"));
    assert!(!requirement_satisfied("1.2.2", "^1.2.3"));
    // 0.x is minor-locked, 0.0.x is exact.
    assert!(requirement_satisfied("0.3.5", "^0.3.1"));
    assert!(!requirement_satisfied("0.4.0", "^0.3.1"));
    assert!(requirement_satisfied("0.0.3", "^0.0.3"));
    assert!(!requirement_satisfied("0.0.4", "^0.0.3"));
}

#[test]
fn tilde_requirements_stay_within_a_minor() {
    assert!(requirement_satisfied("1.2.9", "~1.2.3"));
    assert!(!requirement_satisfied("1.3.0", "~1.2.3"));
    assert!(!requirement_satisfied("1.2.2", "~1.2.3"));
    assert!(requirement_satisfied("2.4.0", "~2.4"));
}

#[test]
fn greater_or_equal_requirements_have_no_upper_bound() {
    assert!(requirement_satisfied("1.2.3", ">=1.2.3"));
    assert!(requirement_satisfied("9.9.9", ">=1.2.3"));
    assert!(!requirement_satisfied("1.2.2", ">=1.2.3"));
}

#[test]
fn mismatched_and_unknown_requirements_do_not_satisfy() {
    assert!(!requirement_satisfied("1.0.0", "2.0.0"));
    assert!(!requirement_satisfied("not-a-version", "1.0.0"));
    assert!(!requirement_satisfied("1.0.0", "<2.0.0"));
    // A wildcard requirement accepts anything, including an unparseable version.
    assert!(requirement_satisfied("whatever", "*"));
    assert!(requirement_satisfied("whatever", ""));
}

// ---- dependency resolution ------------------------------------------

#[test]
fn resolve_dependencies_reports_a_missing_dependency() {
    let dir = tempfile::tempdir().unwrap();
    write_plugin(
        dir.path(),
        "needs",
        r#"{"name":"needs","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
    );
    std::fs::write(dir.path().join("needs/main.sh"), "#!/bin/sh\n").unwrap();
    let registry = PluginRegistry::scan(dir.path());
    let problems = registry.resolve_dependencies();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].0, "needs");
    assert!(problems[0].1[0].contains("missing dependency `core`"));
}

#[test]
fn resolve_dependencies_reports_an_unsatisfied_version() {
    let dir = tempfile::tempdir().unwrap();
    write_plugin(
        dir.path(),
        "core",
        r#"{"name":"core","version":"1.0.0","entry":"main.sh"}"#,
    );
    std::fs::write(dir.path().join("core/main.sh"), "#!/bin/sh\n").unwrap();
    write_plugin(
        dir.path(),
        "needs",
        r#"{"name":"needs","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^2.0.0"}]}"#,
    );
    std::fs::write(dir.path().join("needs/main.sh"), "#!/bin/sh\n").unwrap();
    let registry = PluginRegistry::scan(dir.path());
    let problems = registry.resolve_dependencies();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].0, "needs");
    assert!(problems[0].1[0].contains("requires `^2.0.0`"));
    assert!(problems[0].1[0].contains("1.0.0"));
}

#[test]
fn resolve_dependencies_is_empty_when_everything_is_satisfied() {
    let dir = tempfile::tempdir().unwrap();
    write_plugin(
        dir.path(),
        "core",
        r#"{"name":"core","version":"1.5.0","entry":"main.sh"}"#,
    );
    std::fs::write(dir.path().join("core/main.sh"), "#!/bin/sh\n").unwrap();
    write_plugin(
        dir.path(),
        "needs",
        r#"{"name":"needs","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
    );
    std::fs::write(dir.path().join("needs/main.sh"), "#!/bin/sh\n").unwrap();
    let registry = PluginRegistry::scan(dir.path());
    assert!(registry.resolve_dependencies().is_empty());
}

// ---- permissions -----------------------------------------------------

/// Write a plugin whose entry point echoes, so execution is observable.
fn exec_plugin(dir: &Path, name: &str, permissions: &str) {
    let body = if permissions.trim().is_empty() {
        format!(r#"{{"name":"{name}","version":"1.0.0","entry":"main.sh"}}"#)
    } else {
        format!(
            r#"{{"name":"{name}","version":"1.0.0","entry":"main.sh","permissions":{permissions}}}"#
        )
    };
    write_plugin(dir, name, &body);
    std::fs::write(dir.join(name).join("main.sh"), "#!/bin/sh\necho hi\n").unwrap();
}

#[test]
fn parse_permission_accepts_every_documented_spelling() {
    let cases: [(&str, PluginPermission); 18] = [
        ("fs.read", PluginPermission::FilesystemRead),
        ("filesystem.read", PluginPermission::FilesystemRead),
        ("fs.write", PluginPermission::FilesystemWrite),
        ("filesystem.write", PluginPermission::FilesystemWrite),
        ("network", PluginPermission::Network),
        ("process", PluginPermission::Process),
        ("exec", PluginPermission::Process),
        ("env", PluginPermission::Environment),
        ("environment", PluginPermission::Environment),
        ("secrets", PluginPermission::Secrets),
        ("skills", PluginPermission::Skills),
        ("tools", PluginPermission::Tools),
        ("agents", PluginPermission::Agents),
        ("hive", PluginPermission::Hive),
        ("bots", PluginPermission::Bots),
        ("ui", PluginPermission::Ui),
        ("tui", PluginPermission::Ui),
        ("background", PluginPermission::Background),
    ];
    for (spelling, expected) in cases {
        assert_eq!(parse_permission(spelling), Some(expected), "{spelling}");
    }
    // Case, surrounding space, scopes and separator variants are tolerated.
    assert_eq!(
        parse_permission(" FS.Read "),
        Some(PluginPermission::FilesystemRead)
    );
    assert_eq!(
        parse_permission("fs.read:workspace"),
        Some(PluginPermission::FilesystemRead)
    );
    assert_eq!(
        parse_permission("fs_write"),
        Some(PluginPermission::FilesystemWrite)
    );
    assert_eq!(
        parse_permission("fs:read"),
        Some(PluginPermission::FilesystemRead)
    );
    assert_eq!(parse_permission("PROCESS"), Some(PluginPermission::Process));
    // Anything else is unknown rather than a silent grant.
    for unknown in [
        "",
        "  ",
        "telepathy",
        "fs.exec",
        "network.admin",
        "telepathy:mind",
    ] {
        assert_eq!(parse_permission(unknown), None, "`{unknown}`");
    }
    // The canonical spelling always round-trips.
    for permission in PluginPermission::ALL {
        assert_eq!(parse_permission(permission.as_str()), Some(permission));
    }
}

#[test]
fn execute_refuses_a_plugin_that_declares_no_permissions() {
    let dir = tempfile::tempdir().unwrap();
    exec_plugin(dir.path(), "quiet", "");
    let registry = PluginRegistry::scan(dir.path());
    assert_eq!(registry.loaded(), 1, "the plugin still loads");
    let err = registry.execute("quiet", "").unwrap_err();
    assert!(err.contains("declares no permissions"), "{err}");
    assert!(err.contains("quiet"), "{err}");
    assert!(err.contains("process"), "{err}");
}

#[test]
fn execute_refuses_a_plugin_with_only_read_permissions() {
    let dir = tempfile::tempdir().unwrap();
    exec_plugin(dir.path(), "reader", r#"["fs.read"]"#);
    let registry = PluginRegistry::scan(dir.path());
    assert_eq!(registry.loaded(), 1);
    let err = registry.execute("reader", "").unwrap_err();
    assert!(err.contains("declares no execution permission"), "{err}");
    assert!(err.contains("process"), "{err}");
    assert!(
        registry
            .check_permissions("reader", &[PluginPermission::Process])
            .is_err()
    );
}

#[test]
fn execute_runs_a_plugin_that_declares_an_execution_permission() {
    let dir = tempfile::tempdir().unwrap();
    exec_plugin(dir.path(), "runner", r#"["fs.read","process"]"#);
    let registry = PluginRegistry::scan(dir.path());
    let out = registry
        .execute("runner", "one two")
        .expect("`process` permits execution");
    assert!(out.contains("exit: 0"), "{out}");
    assert!(out.contains("hi"), "{out}");

    // `network` is the other execution-capable permission.
    exec_plugin(dir.path(), "net", r#"["network"]"#);
    let registry = PluginRegistry::scan(dir.path());
    assert!(registry.execute("net", "").is_ok());
}

#[test]
fn permission_accessors_reflect_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    exec_plugin(
        dir.path(),
        "mixed",
        r#"["process","fs.read","fs.read","telepathy"]"#,
    );
    let registry = PluginRegistry::scan(dir.path());
    assert_eq!(
        registry.permissions("mixed"),
        vec![PluginPermission::Process, PluginPermission::FilesystemRead]
    );
    assert!(
        registry
            .check_permissions("mixed", &[PluginPermission::Process])
            .is_ok()
    );
    assert!(
        registry
            .check_permissions(
                "mixed",
                &[PluginPermission::FilesystemRead, PluginPermission::Process],
            )
            .is_ok()
    );
    assert!(registry.check_permissions("mixed", &[]).is_ok());

    let err = registry
        .check_permissions(
            "mixed",
            &[PluginPermission::Secrets, PluginPermission::Network],
        )
        .unwrap_err();
    assert!(err.contains("`secrets`"), "{err}");
    assert!(err.contains("`network`"), "{err}");
    assert!(!err.contains("`process`"), "{err}");

    // Unknown plugins resolve to no permissions, and checking them is an error.
    assert!(
        registry
            .permissions("definitely-not-installed-9f3a")
            .is_empty()
    );
    let err = registry
        .check_permissions("definitely-not-installed-9f3a", &[])
        .unwrap_err();
    assert!(err.contains("plugin not found"), "{err}");
}

#[test]
fn validate_warns_about_unknown_permissions_and_components() {
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path());
    let source = tempfile::tempdir().unwrap();
    write_plugin(
        source.path(),
        "odd",
        r#"{"name":"odd","version":"1.0.0","description":"d","entry":"main.sh",
                "permissions":["fs.read","telepathy"],
                "components":{"tools":["run"],"nonsense":["thing"],"UI":[]}}"#,
    );
    std::fs::write(source.path().join("odd/main.sh"), "#!/bin/sh\n").unwrap();
    let issues = registry.validate(&source.path().join("odd")).unwrap();
    let joined = issues.join("\n");
    assert!(
        joined.contains("warning: unknown permission `telepathy`"),
        "{joined}"
    );
    assert!(
        joined.contains("warning: unknown component type `nonsense`"),
        "{joined}"
    );
    // Known spellings — including a component type in the wrong case — are not reported.
    assert!(!joined.contains("fs.read"), "{joined}");
    assert!(!joined.contains("`tools`"), "{joined}");
    assert!(!joined.contains("`UI`"), "{joined}");
    // Warnings only: nothing here would block an install.
    assert!(
        issues.iter().all(|issue| issue.starts_with("warning:")),
        "{issues:?}"
    );
}

// ---- lifecycle -------------------------------------------------------

fn plugin_source(root: &Path, name: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(dir.join("plugin.json"), body).unwrap();
    std::fs::write(dir.join("main.sh"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::write(dir.join("lib/helper.txt"), "helper").unwrap();
}

#[test]
fn install_uninstall_round_trip() {
    let source = tempfile::tempdir().unwrap();
    plugin_source(
        source.path(),
        "src",
        r#"{"id":"acme","name":"acme","version":"1.0.0","entry":"main.sh"}"#,
    );
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path());

    let id = registry
        .install(&source.path().join("src"))
        .expect("install works");
    assert_eq!(id, "acme");
    assert!(installed.path().join("acme/plugin.json").is_file());
    assert!(installed.path().join("acme/main.sh").is_file());
    assert!(installed.path().join("acme/lib/helper.txt").is_file());

    let rescanned = PluginRegistry::scan(installed.path());
    assert_eq!(rescanned.installed(), 1);
    assert_eq!(rescanned.loaded(), 1);

    rescanned.uninstall("acme").expect("uninstall works");
    assert!(!installed.path().join("acme").exists());
    assert!(rescanned.uninstall("acme").is_err());
    assert_eq!(PluginRegistry::scan(installed.path()).installed(), 0);
}

#[test]
fn install_refuses_to_overwrite() {
    let source = tempfile::tempdir().unwrap();
    plugin_source(
        source.path(),
        "src",
        r#"{"id":"acme","name":"acme","version":"1.0.0","entry":"main.sh"}"#,
    );
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path());
    registry.install(&source.path().join("src")).unwrap();
    let err = registry.install(&source.path().join("src")).unwrap_err();
    assert!(err.contains("already installed"), "{err}");
    assert!(err.contains("uninstall"), "{err}");
}

#[test]
fn install_rejects_a_directory_without_a_manifest() {
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join("empty")).unwrap();
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path());
    let err = registry.install(&source.path().join("empty")).unwrap_err();
    assert!(err.contains("no plugin.json"), "{err}");
}

#[test]
fn install_rejects_a_missing_entry_point() {
    let source = tempfile::tempdir().unwrap();
    write_plugin(
        source.path(),
        "src",
        r#"{"id":"acme","name":"acme","version":"1.0.0","entry":"nope.sh"}"#,
    );
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path());
    let err = registry.install(&source.path().join("src")).unwrap_err();
    assert!(err.contains("entry point"), "{err}");
}

#[test]
fn enable_and_disable_rewrite_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    write_plugin(
        dir.path(),
        "p",
        r#"{"name":"p","version":"1.0.0","entry":"main.sh","enabled":true}"#,
    );
    std::fs::write(dir.path().join("p/main.sh"), "#!/bin/sh\n").unwrap();
    let registry = PluginRegistry::scan(dir.path());
    assert_eq!(registry.loaded(), 1);

    registry.disable("p").expect("disable works");
    let manifest = PluginManifest::from_dir(&dir.path().join("p")).unwrap();
    assert!(!manifest.enabled);
    assert_eq!(PluginRegistry::scan(dir.path()).loaded(), 0);

    registry.enable("p").expect("enable works");
    let manifest = PluginManifest::from_dir(&dir.path().join("p")).unwrap();
    assert!(manifest.enabled);
    assert_eq!(PluginRegistry::scan(dir.path()).loaded(), 1);

    // Unknown ids are an error, and no temp file is left behind.
    assert!(registry.enable("nope").is_err());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("p"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn inspect_summarises_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    write_plugin(
        dir.path(),
        "acme",
        r#"{"id":"acme","name":"acme","version":"1.2.3","description":"does things",
                "author":"acme","license":"MIT","entry":"main.sh",
                "components":{"tools":["run"],"skills":["s1"]},
                "dependencies":[{"name":"core","version_req":"^1.0.0"}],
                "permissions":["fs.write"]}"#,
    );
    std::fs::write(dir.path().join("acme/main.sh"), "#!/bin/sh\n").unwrap();
    let summary = PluginRegistry::scan(dir.path()).inspect("acme").unwrap();
    for needle in [
        "id: acme",
        "name: acme",
        "version: 1.2.3",
        "description: does things",
        "author: acme",
        "license: MIT",
        "entry: main.sh",
        "enabled: true",
        "components: skills: s1; tools: run",
        "dependencies: core ^1.0.0",
        "permissions: fs.write",
    ] {
        assert!(
            summary.contains(needle),
            "missing `{needle}` in:\n{summary}"
        );
    }
    assert!(PluginRegistry::scan(dir.path()).inspect("nope").is_err());
}

#[test]
fn validate_reports_manifest_problems() {
    let installed = tempfile::tempdir().unwrap();
    write_plugin(
        installed.path(),
        "core",
        r#"{"name":"core","version":"1.0.0","entry":"main.sh"}"#,
    );
    std::fs::write(installed.path().join("core/main.sh"), "#!/bin/sh\n").unwrap();
    let registry = PluginRegistry::scan(installed.path());

    let source = tempfile::tempdir().unwrap();
    write_plugin(
        source.path(),
        "bad",
        r#"{"name":"bad","version":"1.0","entry":"missing.sh",
                "min_vikram_version":"9.9.9",
                "dependencies":[{"name":"ghost","version_req":"^1.0.0"}]}"#,
    );
    let issues = registry.validate(&source.path().join("bad")).unwrap();
    let joined = issues.join("\n");
    assert!(joined.contains("is not semver"), "{joined}");
    assert!(joined.contains("does not exist"), "{joined}");
    assert!(joined.contains("requires vikram >= 9.9.9"), "{joined}");
    assert!(
        joined.contains("dependency `ghost` is not installed"),
        "{joined}"
    );
    assert!(issues.iter().any(|i| i.starts_with("warning:")));
    assert!(!issues.is_empty());

    // A clean manifest against a satisfied dependency has nothing to report.
    write_plugin(
        source.path(),
        "good",
        r#"{"name":"good","version":"2.0.0","description":"d","entry":"main.sh",
                "min_vikram_version":"0.0.1",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
    );
    std::fs::write(source.path().join("good/main.sh"), "#!/bin/sh\n").unwrap();
    let clean = registry.validate(&source.path().join("good")).unwrap();
    assert!(clean.is_empty(), "{clean:?}");

    // A missing manifest is a hard error, not a warning list.
    assert!(registry.validate(&source.path().join("absent")).is_err());
}

#[test]
fn lockfile_is_sorted_and_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    write_plugin(
        dir.path(),
        "zulu",
        r#"{"name":"zulu","version":"1.0.0","entry":"main.sh"}"#,
    );
    std::fs::write(dir.path().join("zulu/main.sh"), "#!/bin/sh\n").unwrap();
    write_plugin(
        dir.path(),
        "alpha",
        r#"{"id":"alpha","name":"alpha","version":"2.0.0","entry":"main.sh"}"#,
    );
    std::fs::write(dir.path().join("alpha/main.sh"), "#!/bin/sh\n").unwrap();
    let registry = PluginRegistry::scan(dir.path());
    let path = registry.lockfile().unwrap();
    assert_eq!(path, dir.path().join(".lock"));
    let first = std::fs::read_to_string(&path).unwrap();
    registry.lockfile().unwrap();
    let second = std::fs::read_to_string(&path).unwrap();
    assert_eq!(first, second);
    let value: serde_json::Value = serde_json::from_str(&first).unwrap();
    let entries = value["plugins"].as_array().unwrap();
    assert_eq!(entries[0]["id"], "alpha");
    assert_eq!(entries[0]["version"], "2.0.0");
    assert_eq!(entries[1]["id"], "zulu");
    // The lockfile is not itself a plugin directory.
    assert_eq!(PluginRegistry::scan(dir.path()).installed(), 2);
}

// ---- sha256 ----------------------------------------------------------

#[test]
fn sha256_matches_the_published_vectors() {
    // NIST / RFC 6234 vectors, cross-checked with `sha256sum`.
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b"The quick brown fox jumps over the lazy dog"),
        "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
    );
    // Two-block message: padding spills into a second block.
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    // The 112-byte NIST vector: two blocks of message plus padding. The `\` continuation keeps
    // the literal free of the whitespace the source indentation would otherwise add.
    const LONG: &str = "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmn\
                            hijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
    assert_eq!(
        sha256_hex(LONG.as_bytes()),
        "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
    );
    // Padding boundaries: 55 bytes fits the length field in the same block, 56 needs a second.
    assert_eq!(
        sha256_hex(&[b'x'; 55]),
        "d5e285683cd4efc02d021a5c62014694958901005d6f71e89e0989fac77e4072"
    );
    assert_eq!(
        sha256_hex(&[b'x'; 56]),
        "04c26261370ee7541549d16dee320c723e3fd14671e66a099afe0a377c16888e"
    );
    assert_eq!(
        sha256_hex(&[b'x'; 64]),
        "7ce100971f64e7001e8fe5a51973ecdfe1ced42befe7ee8d5fd6219506b5393c"
    );
    // A megabyte of `a`: many blocks, and the length no longer fits a small integer.
    assert_eq!(
        sha256_hex(&vec![b'a'; 1_000_000]),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
    // The byte form and the hex form agree, and the digest is 32 bytes.
    assert_eq!(sha256(b"abc").len(), 32);
    assert_eq!(sha256(b"abc")[0], 0xba);
    assert_eq!(sha256_hex(b"abc").len(), 64);
}

#[test]
fn sha256_is_sensitive_to_a_single_bit() {
    assert_ne!(sha256_hex(b"plugin-a"), sha256_hex(b"plugin-b"));
    assert_ne!(sha256_hex(b"abc"), sha256_hex(b"abd"));
    assert_eq!(sha256_hex(b"abc"), sha256_hex(b"abc"));
}

// ---- integrity -------------------------------------------------------

/// Write `plugin.sha256` covering the `plugin.json` that is on disk right now.
fn write_hash(dir: &Path) {
    let body = std::fs::read(dir.join("plugin.json")).unwrap();
    std::fs::write(
        dir.join(PLUGIN_HASH_FILE),
        format!("{}\n", sha256_hex(&body)),
    )
    .unwrap();
}

const SIGNED_MANIFEST: &str = r#"{"id":"signed","name":"signed","version":"1.0.0",
        "description":"d","entry":"main.sh"}"#;

#[test]
fn a_matching_hash_installs_and_validates_cleanly() {
    let source = tempfile::tempdir().unwrap();
    plugin_source(source.path(), "signed", SIGNED_MANIFEST);
    let dir = source.path().join("signed");
    write_hash(&dir);

    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path());
    let issues = registry.validate(&dir).unwrap();
    assert!(issues.is_empty(), "{issues:?}");
    let integrity = registry.validate_integrity(&dir).unwrap();
    assert!(integrity.is_empty(), "{integrity:?}");
    assert_eq!(registry.install(&dir).unwrap(), "signed");
    // The hash file travels with the plugin so a later inspect can re-check it.
    assert!(installed.path().join("signed/plugin.sha256").is_file());
    let summary = PluginRegistry::scan(installed.path())
        .inspect("signed")
        .unwrap();
    assert!(
        summary.contains("integrity: plugin.sha256 matches"),
        "{summary}"
    );
}

#[test]
fn a_tampered_manifest_is_refused_by_validate_and_install() {
    let source = tempfile::tempdir().unwrap();
    plugin_source(source.path(), "signed", SIGNED_MANIFEST);
    let dir = source.path().join("signed");
    write_hash(&dir);
    // Change the manifest after the hash was taken.
    std::fs::write(
        dir.join("plugin.json"),
        SIGNED_MANIFEST.replace(r#""version":"1.0.0""#, r#""version":"9.9.9""#),
    )
    .unwrap();

    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path());

    let issues = registry.validate(&dir).unwrap();
    let joined = issues.join("\n");
    assert!(
        joined.contains(
            "integrity check failed for `signed`: plugin.json hash does not match plugin.sha256"
        ),
        "{joined}"
    );

    let err = registry.install(&dir).unwrap_err();
    assert!(
        err.contains(
            "integrity check failed for `signed`: plugin.json hash does not match plugin.sha256"
        ),
        "{err}"
    );
    // Nothing was copied: a failed check must not leave a half-installed plugin behind.
    assert!(!installed.path().join("signed").exists());

    // The integrity-only view reports the same failure as an error.
    let integrity = registry.validate_integrity(&dir).unwrap();
    assert!(
        integrity
            .iter()
            .any(|issue| issue.starts_with("error: integrity check failed")),
        "{integrity:?}"
    );
}

#[test]
fn a_missing_hash_warns_without_blocking() {
    let source = tempfile::tempdir().unwrap();
    plugin_source(source.path(), "unsigned", SIGNED_MANIFEST);
    let dir = source.path().join("unsigned");
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path());

    assert_eq!(
        registry.validate_integrity(&dir).unwrap(),
        vec!["warning: no plugin.sha256 present (integrity not verified)".to_string()]
    );
    // No hash is not an error: `validate` is quiet and `install` still works.
    assert!(registry.validate(&dir).unwrap().is_empty());
    assert_eq!(registry.install(&dir).unwrap(), "signed");
}

#[test]
fn inspect_reports_signature_presence_without_claiming_verification() {
    let dir = tempfile::tempdir().unwrap();
    plugin_source(dir.path(), "sigged", SIGNED_MANIFEST);
    std::fs::write(
        dir.path().join("sigged/plugin.sig"),
        "-----BEGIN SIGNATURE-----\nnot-a-real-signature\n",
    )
    .unwrap();
    let summary = PluginRegistry::scan(dir.path()).inspect("sigged").unwrap();
    assert!(
        summary.contains("signature: present (not cryptographically verified)"),
        "{summary}"
    );
    assert!(
        summary.contains("integrity: not verified (no plugin.sha256)"),
        "{summary}"
    );
    assert!(!summary.contains("signature verified"), "{summary}");

    // No signature file, and an empty one, both read as absent.
    plugin_source(dir.path(), "bare", SIGNED_MANIFEST);
    let bare = PluginRegistry::scan(dir.path()).inspect("bare").unwrap();
    assert!(bare.contains("signature: (none)"), "{bare}");
    std::fs::write(dir.path().join("bare/plugin.sig"), "   \n").unwrap();
    let empty_sig = PluginRegistry::scan(dir.path()).inspect("bare").unwrap();
    assert!(empty_sig.contains("signature: (none)"), "{empty_sig}");
}

// ---- dependency auto-install -----------------------------------------

#[test]
fn installing_a_plugin_installs_its_dependencies_first() {
    let sources = tempfile::tempdir().unwrap();
    plugin_source(
        sources.path(),
        "core",
        r#"{"id":"core","name":"core","version":"1.2.0","entry":"main.sh"}"#,
    );
    plugin_source(
        sources.path(),
        "app",
        r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
    );
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());

    let order = registry
        .install_with_dependencies(&sources.path().join("app"))
        .expect("dependencies resolve from the registry");
    assert_eq!(order, vec!["core".to_string(), "app".to_string()]);
    assert!(installed.path().join("core/plugin.json").is_file());
    assert!(installed.path().join("app/plugin.json").is_file());
    assert!(installed.path().join("core/main.sh").is_file());
    assert_eq!(PluginRegistry::scan(installed.path()).installed(), 2);
    // The dependency graph is now consistent.
    assert!(
        PluginRegistry::scan(installed.path())
            .resolve_dependencies()
            .is_empty()
    );
}

#[test]
fn install_takes_the_dependency_path_when_dependencies_are_declared() {
    let sources = tempfile::tempdir().unwrap();
    plugin_source(
        sources.path(),
        "core",
        r#"{"id":"core","name":"core","version":"1.4.0","entry":"main.sh"}"#,
    );
    plugin_source(
        sources.path(),
        "app",
        r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
    );
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());

    // `install` keeps its signature and still returns the plugin's own id.
    assert_eq!(
        registry.install(&sources.path().join("app")).unwrap(),
        "app"
    );
    assert!(installed.path().join("core/plugin.json").is_file());
    assert!(installed.path().join("app/plugin.json").is_file());
}

#[test]
fn a_transitive_chain_installs_depth_first() {
    let sources = tempfile::tempdir().unwrap();
    plugin_source(
        sources.path(),
        "base",
        r#"{"id":"base","name":"base","version":"0.1.0","entry":"main.sh"}"#,
    );
    plugin_source(
        sources.path(),
        "mid",
        r#"{"id":"mid","name":"mid","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"base","version_req":"^0.1.0"}]}"#,
    );
    plugin_source(
        sources.path(),
        "app",
        r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"mid","version_req":"^1.0.0"}]}"#,
    );
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());
    let order = registry
        .install_with_dependencies(&sources.path().join("app"))
        .unwrap();
    assert_eq!(
        order,
        vec!["base".to_string(), "mid".to_string(), "app".to_string()]
    );
}

#[test]
fn a_shared_dependency_is_installed_once() {
    let sources = tempfile::tempdir().unwrap();
    plugin_source(
        sources.path(),
        "d",
        r#"{"id":"d","name":"d","version":"1.0.0","entry":"main.sh"}"#,
    );
    for leaf in ["b", "c"] {
        plugin_source(
            sources.path(),
            leaf,
            &format!(
                r#"{{"id":"{leaf}","name":"{leaf}","version":"1.0.0","entry":"main.sh",
                        "dependencies":[{{"name":"d","version_req":"^1.0.0"}}]}}"#
            ),
        );
    }
    plugin_source(
        sources.path(),
        "a",
        r#"{"id":"a","name":"a","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"b","version_req":"^1.0.0"},
                                {"name":"c","version_req":"^1.0.0"}]}"#,
    );
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());
    let order = registry
        .install_with_dependencies(&sources.path().join("a"))
        .unwrap();
    // `d` is reached twice but installed once, and never re-copied over itself.
    assert_eq!(
        order,
        vec![
            "d".to_string(),
            "b".to_string(),
            "c".to_string(),
            "a".to_string()
        ]
    );
}

#[test]
fn a_sibling_directory_is_searched_before_the_registry() {
    let sources = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    plugin_source(
        sources.path(),
        "core",
        r#"{"id":"core","name":"core","version":"1.1.0","entry":"main.sh"}"#,
    );
    plugin_source(
        elsewhere.path(),
        "core",
        r#"{"id":"core","name":"core","version":"1.0.0","entry":"main.sh"}"#,
    );
    plugin_source(
        sources.path(),
        "app",
        r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
    );
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path()).with_registry_dir(elsewhere.path());
    registry
        .install_with_dependencies(&sources.path().join("app"))
        .unwrap();
    // The sibling copy (1.1.0), not the registry copy (1.0.0), is what got installed.
    let rescanned = PluginRegistry::scan(installed.path());
    assert_eq!(
        rescanned.installed_version("core").as_deref(),
        Some("1.1.0")
    );
}

#[test]
fn a_missing_dependency_names_itself_and_the_places_searched() {
    let sources = tempfile::tempdir().unwrap();
    let registry_dir = tempfile::tempdir().unwrap();
    plugin_source(
        sources.path(),
        "app",
        r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"ghost","version_req":"^1.0.0"}]}"#,
    );
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path()).with_registry_dir(registry_dir.path());

    let err = registry
        .install_with_dependencies(&sources.path().join("app"))
        .unwrap_err();
    assert!(err.contains("missing dependency `ghost`"), "{err}");
    assert!(err.contains("^1.0.0"), "{err}");
    assert!(err.contains("`app`"), "{err}");
    // Both search locations are named, so the operator can put the plugin where it is looked
    // for instead of guessing.
    assert!(
        err.contains(&sources.path().join("ghost").display().to_string()),
        "{err}"
    );
    assert!(
        err.contains(&registry_dir.path().join("ghost").display().to_string()),
        "{err}"
    );
    // A failed resolution installs nothing at all.
    assert_eq!(PluginRegistry::scan(installed.path()).installed(), 0);
    assert!(!installed.path().join("app").exists());
}

#[test]
fn a_dependency_cycle_is_refused() {
    let sources = tempfile::tempdir().unwrap();
    plugin_source(
        sources.path(),
        "alpha",
        r#"{"id":"alpha","name":"alpha","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"beta","version_req":"^1.0.0"}]}"#,
    );
    plugin_source(
        sources.path(),
        "beta",
        r#"{"id":"beta","name":"beta","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"alpha","version_req":"^1.0.0"}]}"#,
    );
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());
    let err = registry
        .install_with_dependencies(&sources.path().join("alpha"))
        .unwrap_err();
    assert!(err.contains("dependency cycle detected"), "{err}");
    assert!(err.contains("`alpha` -> `beta` -> `alpha`"), "{err}");
    assert_eq!(PluginRegistry::scan(installed.path()).installed(), 0);
}

#[test]
fn a_self_dependency_is_a_cycle() {
    let sources = tempfile::tempdir().unwrap();
    plugin_source(
        sources.path(),
        "navel",
        r#"{"id":"navel","name":"navel","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"navel","version_req":"^1.0.0"}]}"#,
    );
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());
    let err = registry
        .install_with_dependencies(&sources.path().join("navel"))
        .unwrap_err();
    assert!(err.contains("dependency cycle detected"), "{err}");
}

#[test]
fn an_installed_but_unsatisfying_dependency_is_refused_not_replaced() {
    let sources = tempfile::tempdir().unwrap();
    let installed = tempfile::tempdir().unwrap();
    plugin_source(
        installed.path(),
        "core",
        r#"{"id":"core","name":"core","version":"2.0.0","entry":"main.sh"}"#,
    );
    plugin_source(
        sources.path(),
        "app",
        r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
    );
    let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());
    let err = registry
        .install_with_dependencies(&sources.path().join("app"))
        .unwrap_err();
    assert!(err.contains("is installed at `2.0.0`"), "{err}");
    assert!(err.contains("^1.0.0"), "{err}");
    // The installed plugin is left exactly as it was.
    assert_eq!(
        PluginRegistry::scan(installed.path())
            .installed_version("core")
            .as_deref(),
        Some("2.0.0")
    );
    assert!(!installed.path().join("app").exists());
}

// ---- registry --------------------------------------------------------

#[test]
fn registry_index_lists_local_plugins_sorted_by_id() {
    let marketplace = tempfile::tempdir().unwrap();
    plugin_source(
        marketplace.path(),
        "zeta",
        r#"{"id":"zeta","name":"zeta","version":"0.9.0","entry":"main.sh"}"#,
    );
    plugin_source(
        marketplace.path(),
        "acme",
        r#"{"id":"acme.tools","name":"acme","version":"2.0.0","entry":"main.sh"}"#,
    );
    // Neither of these is a plugin, and neither may appear in the index.
    std::fs::create_dir_all(marketplace.path().join("not-a-plugin")).unwrap();
    std::fs::write(marketplace.path().join("loose.json"), "{}").unwrap();
    std::fs::create_dir_all(marketplace.path().join("broken")).unwrap();
    std::fs::write(marketplace.path().join("broken/plugin.json"), "{oops").unwrap();

    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path()).with_registry_dir(marketplace.path());
    assert_eq!(
        registry.registry_index().unwrap(),
        vec![
            (
                "acme.tools".to_string(),
                "acme".to_string(),
                "2.0.0".to_string()
            ),
            ("zeta".to_string(), "zeta".to_string(), "0.9.0".to_string()),
        ]
    );
}

#[test]
fn a_missing_registry_is_an_empty_index() {
    let installed = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::scan(installed.path())
        .with_registry_dir(installed.path().join("no-such-registry"));
    assert!(registry.registry_index().unwrap().is_empty());
}

#[test]
fn the_default_registry_lives_under_the_vikram_home() {
    let installed = tempfile::tempdir().unwrap();
    assert_eq!(
        PluginRegistry::scan(installed.path()).registry_root(),
        registry_dir()
    );
    assert!(
        registry_dir().ends_with("plugin-registry"),
        "{}",
        registry_dir().display()
    );
}
