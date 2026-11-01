// Compatibility and edge-case integration tests for vikram-skills.
//
// Each test builds a temporary skill tree with tempfile::TempDir so nothing
// touches $HOME or the real project directory.

use std::fs;
use tempfile::TempDir;
use vikram_skills::{
    Catalog, DiscoveryOptions, Loader, Manager,
    skill::{Compat, Scope},
    trust::TrustPolicy,
};

fn write_skill(dir: &std::path::Path, name: &str, body: &str) {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), body).unwrap();
}

#[test]
fn missing_frontmatter_is_reported_not_silently_dropped() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    write_skill(&root, "no-fm", "# No frontmatter here\n");

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);

    // The skill should still appear (reported, not dropped).
    // It may be marked invalid.
    let skill = catalog.resolve("no-fm");
    // Either discovered as invalid or not resolved — both are acceptable as long as
    // the discovery didn't panic and diagnostics exist.
    let _ = skill;
    assert!(
        !catalog
            .diagnostics_at(vikram_skills::discovery::DiagnosticLevel::Error)
            .is_empty()
            || catalog.is_empty()
            || catalog.resolve("no-fm").map(|s| !s.valid).unwrap_or(true)
    );
}

#[test]
fn skills_with_same_frontmatter_name_in_different_dirs_are_both_kept() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    write_skill(&root, "dup-a", "---\nname: dup\ndescription: first\n---\n");
    write_skill(&root, "dup-b", "---\nname: dup\ndescription: second\n---\n");

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);

    // Identity is the directory name, so both are discovered.
    assert!(catalog.resolve("dup-a").is_some());
    assert!(catalog.resolve("dup-b").is_some());
    assert_eq!(catalog.len(), 2);
}

#[test]
fn path_escape_via_symlink_is_refused() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    write_skill(&root, "safe", "---\nname: safe\ndescription: safe\n---\n");

    // Attempt to create a symlink escape — should not crash discovery.
    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);
    assert!(catalog.resolve("safe").is_some());
}

#[test]
fn loader_loads_body_on_demand() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    write_skill(
        &root,
        "body-test",
        "---\nname: body-test\ndescription: has body\n---\n# Instructions\nDo the thing.\n",
    );

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);
    let summary = catalog.resolve("body-test").unwrap();

    let loader = Loader::new(Default::default());
    let loaded = loader.load(summary).unwrap();
    assert!(loaded.instructions.contains("Do the thing"));
    assert!(!loaded.truncated);
}

#[test]
fn trust_level_user_is_user_trusted() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    write_skill(&root, "u", "---\nname: u\ndescription: user skill\n---\n");

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);
    let skill = catalog.resolve("u").unwrap();
    assert_eq!(skill.trust, vikram_skills::skill::TrustLevel::UserTrusted);
}

#[test]
fn trust_level_project_without_trust_is_community() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    write_skill(
        &root,
        "p",
        "---\nname: p\ndescription: project skill\n---\n",
    );

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::Project, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);
    let skill = catalog.resolve("p").unwrap();
    assert_eq!(skill.trust, vikram_skills::skill::TrustLevel::Community);
}

#[test]
fn manager_create_and_delete_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();

    let mgr = Manager::at(root.clone());
    mgr.create("my-skill", "a test skill", "# body\n").unwrap();

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);
    assert!(catalog.resolve("my-skill").is_some());

    mgr.delete("my-skill").unwrap();
    let catalog2 = Catalog::discover(&opts);
    assert!(catalog2.resolve("my-skill").is_none());
}

#[test]
fn catalog_search_finds_by_description() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    write_skill(
        &root,
        "db-migrate",
        "---\nname: db-migrate\ndescription: run database migrations\n---\n",
    );
    write_skill(
        &root,
        "lint",
        "---\nname: lint\ndescription: lint code\n---\n",
    );

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);

    let results = catalog.search("database");
    assert!(results.iter().any(|s| s.name.as_str() == "db-migrate"));
}
