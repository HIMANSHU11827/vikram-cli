// Simple benchmark for vikram-skills. Run with: cargo run --example bench -p vikram-skills
//
// Uses std::time::Instant (no external dependencies).

use std::fs;
use std::time::Instant;
use tempfile::TempDir;
use vikram_skills::{
    Catalog, DiscoveryOptions, Loader,
    skill::{Compat, Scope},
    trust::TrustPolicy,
};

fn write_skill(dir: &std::path::Path, name: &str) {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            "---\nname: {}\ndescription: benchmark skill {}\n---\n# Body\n",
            name, name
        ),
    )
    .unwrap();
}

fn main() {
    // Setup: 100 skills
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    fs::create_dir_all(&root).unwrap();
    for i in 0..100 {
        write_skill(&root, &format!("skill-{:03}", i));
    }

    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::User, Compat::Native, root.clone())
        .with_trust(TrustPolicy::default());

    // Benchmark: discovery
    let start = Instant::now();
    for _ in 0..10 {
        let _ = Catalog::discover(&opts);
    }
    let discovery_ms = start.elapsed().as_millis() / 10;
    println!(
        "Discovery (100 skills, avg of 10 runs): {} ms",
        discovery_ms
    );

    // Benchmark: catalog listing
    let catalog = Catalog::discover(&opts);
    let loader = Loader::new(Default::default());
    let summaries: Vec<_> = catalog.list().to_vec();
    let start = Instant::now();
    for _ in 0..1000 {
        let _ = loader.catalog_listing(&summaries);
    }
    let listing_us = start.elapsed().as_micros() / 1000;
    println!(
        "Catalog listing (100 skills, avg of 1000 runs): {} us",
        listing_us
    );

    // Benchmark: frontmatter parse (via discovery, which calls parse internally)
    // Skipped: parse_at is pub(crate). Discovery benchmark covers it.

    println!("\nAll benchmarks complete.");
}
