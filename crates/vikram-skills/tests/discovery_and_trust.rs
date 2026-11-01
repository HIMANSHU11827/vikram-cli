// Integration tests for vikram-skills — public API only.
//
// Each test builds a temporary skill tree with tempfile::TempDir so nothing
// touches $HOME or the real project directory.

use std::fs;
use tempfile::TempDir;
use vikram_skills::{
    Catalog, DiscoveryOptions,
    skill::{Compat, Scope},
    trust::TrustPolicy,
};

fn write_skill(dir: &std::path::Path, name: &str, body: &str) {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), body).unwrap();
}

#[test]
fn discovers_a_skill_from_a_user_root() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    write_skill(
        &root,
        "hello",
        "---\nname: hello\ndescription: says hello\n---\n# hi\n",
    );

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);

    let skill = catalog.resolve("hello").expect("hello must be discovered");
    assert_eq!(skill.name.as_str(), "hello");
    assert_eq!(skill.description, "says hello");
    assert!(skill.valid);
    assert!(skill.enabled);
    assert_eq!(catalog.len(), 1);
}

#[test]
fn name_dir_mismatch_is_a_warning_not_an_error() {
    // R4: a skill whose `name` differs from its directory still loads.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    // directory is "wrong-dir", frontmatter says "right-name"
    let skill_dir = root.join("wrong-dir");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: right-name\ndescription: mismatched\n---\n",
    )
    .unwrap();

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);

    let skill = catalog
        .resolve("wrong-dir")
        .expect("skill loads despite mismatch");
    assert!(skill.valid);
    let warnings: Vec<_> = catalog.diagnostics_at(vikram_skills::DiagnosticLevel::Warning);
    assert!(
        warnings.iter().any(|d| d.message.contains("name")),
        "expected a name-mismatch warning, got: {warnings:?}"
    );
}

#[test]
fn two_skills_with_the_same_name_are_both_kept() {
    let tmp = TempDir::new().unwrap();
    let user_root = tmp.path().join("user");
    let project_root = tmp.path().join("project");
    fs::create_dir_all(&user_root).unwrap();
    fs::create_dir_all(&project_root).unwrap();
    write_skill(
        &user_root,
        "dup",
        "---\nname: dup\ndescription: from user\n---\n",
    );
    write_skill(
        &project_root,
        "dup",
        "---\nname: dup\ndescription: from project\n---\n",
    );

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, user_root.clone())
        .with_root(Scope::Project, Compat::Native, project_root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);

    // Both are present in the listing.
    let conflicts = catalog.conflicts();
    assert!(
        !conflicts.is_empty(),
        "conflicts() must report the duplicate"
    );
}

#[test]
fn missing_frontmatter_is_reported_not_silently_dropped() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    let skill_dir = root.join("broken");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), "# no frontmatter here\n").unwrap();

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());
    let catalog = Catalog::discover(&opts);

    // The skill must not appear as a valid entry.
    let skill = catalog.resolve("broken").expect("entry exists");
    assert!(!skill.valid, "skill with no frontmatter must be invalid");
    let errors = catalog.diagnostics_at(vikram_skills::DiagnosticLevel::Error);
    assert!(
        !errors.is_empty(),
        "missing frontmatter must be an error diagnostic"
    );
}

#[test]
fn trust_policy_level_for_user_scope_is_user_trusted() {
    let policy = TrustPolicy::default();
    let level = policy.level_for(Scope::User, Compat::Native, std::path::Path::new("/tmp/x"));
    assert_eq!(level, vikram_skills::skill::TrustLevel::UserTrusted);
}

#[test]
fn deny_overrides_project_trusted() {
    let policy = TrustPolicy {
        project_trusted: true,
        deny: vec!["evil".into()],
        ..TrustPolicy::default()
    };
    let level = policy.level_for(
        Scope::Project,
        Compat::Native,
        std::path::Path::new("/tmp/evil"),
    );
    assert_eq!(level, vikram_skills::skill::TrustLevel::Untrusted);
}

#[test]
fn project_trusted_grants_project_trusted_level() {
    let policy = TrustPolicy {
        project_trusted: true,
        ..TrustPolicy::default()
    };
    let level = policy.level_for(
        Scope::Project,
        Compat::Native,
        std::path::Path::new("/tmp/proj"),
    );
    assert_eq!(level, vikram_skills::skill::TrustLevel::ProjectTrusted);
}

#[test]
fn external_scope_is_untrusted_community() {
    let policy = TrustPolicy::default();
    let level = policy.level_for(
        Scope::External,
        Compat::Native,
        std::path::Path::new("/tmp/x"),
    );
    assert_eq!(level, vikram_skills::skill::TrustLevel::Community);
}
