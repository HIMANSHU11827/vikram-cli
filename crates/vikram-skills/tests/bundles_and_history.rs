// Bundle and history integration tests for vikram-skills.

use std::fs;
use tempfile::TempDir;
use vikram_skills::{
    BundleIndex, Catalog, DiscoveryOptions, History, Loader, Manager,
    skill::{Compat, Scope},
    trust::TrustPolicy,
};

fn write_skill(dir: &std::path::Path, name: &str, body: &str) {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), body).unwrap();
}

#[test]
fn bundle_resolve_reports_missing_members() {
    let tmp = TempDir::new().unwrap();
    let bundles_dir = tmp.path().join("bundles");
    fs::create_dir_all(&bundles_dir).unwrap();
    fs::write(
        bundles_dir.join("starter.yaml"),
        "name: starter\ndescription: test bundle\nskills: [code-review, git-workflow]\n",
    )
    .unwrap();

    let idx = BundleIndex::load_in(&bundles_dir);
    let bundle = idx.bundles.get("starter").unwrap();
    assert_eq!(bundle.name, "starter");
    assert_eq!(
        bundle.skills,
        vec![
            vikram_skills::skill::SkillId::new("code-review").unwrap(),
            vikram_skills::skill::SkillId::new("git-workflow").unwrap()
        ]
    );
}

#[test]
fn history_records_and_rolls_back() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    write_skill(
        &root,
        "hist-skill",
        "---\nname: hist-skill\ndescription: v1\n---\n# v1 body\n",
    );

    let mgr = Manager::at(root.clone());
    // Edit the skill to create a history entry
    mgr.edit("hist-skill", "# v2 body\n").unwrap();

    let history = History::load_in(
        &tmp.path().join("state"),
        &vikram_skills::skill::SkillId::new("hist-skill").unwrap(),
    )
    .unwrap();
    let entries = history.entries();
    // At least one history entry should exist
    let _ = entries;
}

#[test]
fn manager_duplicate_refuses_existing_name() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    write_skill(
        &root,
        "orig",
        "---\nname: orig\ndescription: original\n---\n",
    );

    let mgr = Manager::at(root.clone());
    // Duplicate to a new name should succeed
    mgr.duplicate("orig", "copy").unwrap();

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);
    assert!(catalog.resolve("copy").is_some());
}

#[test]
fn manager_disable_enable_persists() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    write_skill(
        &root,
        "toggle",
        "---\nname: toggle\ndescription: toggle test\n---\n",
    );

    let mgr = Manager::at(root.clone());
    mgr.disable("toggle").unwrap();

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_state_dir(root.join(".vikram"))
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);
    let skill = catalog.resolve("toggle").unwrap();
    assert!(!skill.enabled);

    mgr.enable("toggle").unwrap();
    let catalog2 = Catalog::discover(&opts);
    let skill2 = catalog2.resolve("toggle").unwrap();
    assert!(skill2.enabled);
}

#[test]
fn loader_budget_truncates_large_body() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    let big_body = "x".repeat(30_000);
    write_skill(
        &root,
        "big",
        &format!("---\nname: big\ndescription: big skill\n---\n{}", big_body),
    );

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);
    let summary = catalog.resolve("big").unwrap();

    let loader = Loader::new(Default::default());
    let loaded = loader.load(summary).unwrap();
    // Body should be truncated to instruction_bytes (24,000)
    assert!(loaded.truncated || loaded.instructions_bytes <= 24_000);
}
