//! `vikram dist` — export and import a reproducible bundle of user-modifiable state.
//!
//! A bundle is a `.tar.gz` holding `MANIFEST.json` plus copies of the files an operator
//! actually edits: `config.json`, `gateway.json`, `transforms.json`, installed plugins,
//! user skills (with their `.vikram` state), and entity context directories. It never
//! contains `.env`, the store database, sessions, pairing state, logs, or the plugin
//! source registry — credentials stay on the machine that owns them, and `config.json`
//! is scanned for literal secret fields before it is written.
//!
//! Import verifies every manifest entry against its SHA-256 before touching the live
//! home, then installs each kind through the same path its own command uses: plugins via
//! `PluginRegistry::install`, skills via the skills `Installer`, entities via the
//! agent-pack spec, and the JSON state files directly — `config.json` is backed up
//! first, `gateway.json` toggles merge without clobbering local ones, and
//! `transforms.json` is restored with a reminder that `vikram transforms apply` is
//! still needed per plugin.
//!
//! Signing: `dist export --sign-key` (or `VIKRAM_DIST_KEY`) stamps the manifest with
//! an HMAC-SHA256 over its canonical JSON, while `--sign-key-ed25519` (or
//! `VIKRAM_DIST_ED25519_KEY`) stamps an `ed25519:` signature that verifies against a
//! public key alone — importers need not hold the signing secret. The HMAC scheme is
//! symmetric, so it proves the bundle came from someone holding the key, not from a
//! specific identity. `dist import` verifies a carried signature whenever the matching
//! key is available and refuses a signed bundle when none is, unless `--insecure` is
//! given.

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};

use clap::Subcommand;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use vikram_runtime::config::{config_path, plugins_dir, vikram_home};
use vikram_runtime::plugins::{PluginManifest, PluginRegistry, sha256, sha256_hex};
use vikram_skills::{InstallSource, Installer, Manager};
use vikram_store::{EntityKind, EntityRow};

use crate::archive;
use crate::gateway_config::{self, GatewayConfig, PlatformToggle};
use crate::module_sdk;
use crate::transforms::TRANSFORMS_FILE;

/// The bundle format this build reads and writes. A bundle declaring anything else is
/// refused rather than guessed at.
const FORMAT_VERSION: &str = "1";

/// Manifest signature schemes and the env vars both commands fall back to for keys.
const SIGN_ALGORITHM: &str = "hmac-sha256";
const SIGN_ALGORITHM_ED25519: &str = "ed25519";
const SIGN_ENV: &str = "VIKRAM_DIST_KEY";
const SIGN_ED25519_ENV: &str = "VIKRAM_DIST_ED25519_KEY";

/// JSON object keys that must never travel in a bundle, even inside `config.json`.
/// `api_key_env`-style fields are names, not values, and do not match.
const SECRET_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "api_secret",
    "secret",
    "client_secret",
    "access_token",
    "auth_token",
    "token",
    "password",
    "passwd",
    "private_key",
    "session_token",
];

#[derive(Debug, Subcommand)]
pub enum DistAction {
    /// Export user-modifiable state into a `.tar.gz` bundle.
    Export {
        /// Output archive path (default: `vikram-dist-<ms>.tar.gz` here).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Include `plugins/.backups` (rollback copies kept by plugin updates).
        #[arg(long)]
        include_backups: bool,
        /// Sign the manifest with this HMAC key (falls back to VIKRAM_DIST_KEY).
        #[arg(long)]
        sign_key: Option<String>,
        /// Sign the manifest with this Ed25519 secret key seed, 64 hex chars
        /// (falls back to VIKRAM_DIST_ED25519_KEY; mutually exclusive with --sign-key).
        #[arg(long)]
        sign_key_ed25519: Option<String>,
    },
    /// Install a bundle produced by `vikram dist export`.
    Import {
        /// The `.tar.gz` bundle to install.
        file: PathBuf,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Install even when entry checksums do not match the manifest.
        #[arg(long)]
        force: bool,
        /// Key used to verify a signed bundle (falls back to VIKRAM_DIST_KEY).
        #[arg(long)]
        verify_key: Option<String>,
        /// Ed25519 public key, 64 hex chars, used to verify an `ed25519:` signed
        /// bundle (falls back to VIKRAM_DIST_ED25519_KEY).
        #[arg(long)]
        verify_key_ed25519: Option<String>,
        /// Accept a signed bundle without verifying its signature.
        #[arg(long)]
        insecure: bool,
    },
    /// List a bundle's contents without installing anything.
    List {
        /// The `.tar.gz` bundle to inspect.
        file: PathBuf,
    },
}

pub fn run(action: DistAction) -> Result<(), String> {
    match action {
        DistAction::Export {
            out,
            include_backups,
            sign_key,
            sign_key_ed25519,
        } => export(
            out.as_deref(),
            include_backups,
            sign_key.as_deref(),
            sign_key_ed25519.as_deref(),
        ),
        DistAction::Import {
            file,
            yes,
            force,
            verify_key,
            verify_key_ed25519,
            insecure,
        } => import(
            &file,
            yes,
            force,
            verify_key.as_deref(),
            verify_key_ed25519.as_deref(),
            insecure,
        ),
        DistAction::List { file } => list(&file),
    }
}

// ---- manifest ---------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManifestEntry {
    /// Path inside the bundle, `/`-separated, relative to the bundle root.
    path: String,
    sha256: String,
    /// config | gateway | transforms | plugin | skill | skill-state | entity
    kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    version: String,
    exported_ms: u64,
    entries: Vec<ManifestEntry>,
    requirements: Requirements,
    /// `hmac-sha256:<hex>` or `ed25519:<hex>` over the manifest's canonical JSON;
    /// absent on unsigned bundles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Requirements {
    #[serde(default)]
    vikram_version: String,
}

// ---- signing ----------------------------------------------------------------

/// The key argument wins; an empty one falls through to its env var.
fn key_arg(arg: Option<&str>, env: &str) -> Option<String> {
    arg.map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            std::env::var(env)
                .ok()
                .map(|key| key.trim().to_owned())
                .filter(|key| !key.is_empty())
        })
}

fn dist_key(arg: Option<&str>) -> Option<String> {
    key_arg(arg, SIGN_ENV)
}

fn dist_ed25519_key(arg: Option<&str>) -> Option<String> {
    key_arg(arg, SIGN_ED25519_ENV)
}

/// RFC 2104 HMAC over the runtime's self-contained SHA-256 (64-byte block).
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = [0x36u8; BLOCK];
    let mut outer = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        inner[i] ^= k[i];
        outer[i] ^= k[i];
    }
    let mut buf = Vec::with_capacity(BLOCK + message.len());
    buf.extend_from_slice(&inner);
    buf.extend_from_slice(message);
    let inner_hash = sha256(&buf);
    let mut buf = Vec::with_capacity(BLOCK + 32);
    buf.extend_from_slice(&outer);
    buf.extend_from_slice(&inner_hash);
    sha256(&buf)
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn hex_decode(input: &str, expected: usize) -> Result<Vec<u8>, String> {
    let input = input.trim();
    if input.len() != expected * 2 || !input.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "expected {expected} bytes as {} hex chars",
            expected * 2
        ));
    }
    (0..expected)
        .map(|i| u8::from_str_radix(&input[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// Canonical bytes the signature covers: the manifest serialized with `signature`
/// absent, so signing and verifying agree on exactly the same input.
fn canonical_manifest(manifest: &Manifest) -> Result<Vec<u8>, String> {
    let mut unsigned = manifest.clone();
    unsigned.signature = None;
    serde_json::to_vec(&unsigned).map_err(|e| e.to_string())
}

fn sign_manifest(manifest: &Manifest, key: &str) -> Result<String, String> {
    let canonical = canonical_manifest(manifest)?;
    Ok(format!(
        "{SIGN_ALGORITHM}:{}",
        hex_lower(&hmac_sha256(key.as_bytes(), &canonical))
    ))
}

/// Ed25519 over the same canonical bytes; the secret key is a 32-byte seed in hex.
fn sign_manifest_ed25519(manifest: &Manifest, seed_hex: &str) -> Result<String, String> {
    let seed = hex_decode(seed_hex, 32).map_err(|e| format!("invalid Ed25519 signing key: {e}"))?;
    let mut seed_bytes = [0u8; 32];
    seed_bytes.copy_from_slice(&seed);
    let key = SigningKey::from_bytes(&seed_bytes);
    let canonical = canonical_manifest(manifest)?;
    let signature: Signature = key.sign(&canonical);
    Ok(format!(
        "{SIGN_ALGORITHM_ED25519}:{}",
        hex_lower(&signature.to_bytes())
    ))
}

const SIGNATURE_MISMATCH: &str = "manifest signature does not match the verification key; the bundle was tampered with or signed with a different key";

fn verify_ed25519(
    canonical: &[u8],
    public_key_hex: &str,
    expected_hex: &str,
) -> Result<(), String> {
    let public = hex_decode(public_key_hex, 32)
        .map_err(|e| format!("invalid Ed25519 verification key: {e}"))?;
    let mut public_bytes = [0u8; 32];
    public_bytes.copy_from_slice(&public);
    let verifying_key = VerifyingKey::from_bytes(&public_bytes)
        .map_err(|_| "Ed25519 verification key is not a valid public key".to_owned())?;
    let signature_bytes =
        hex_decode(expected_hex, 64).map_err(|e| format!("malformed Ed25519 signature: {e}"))?;
    let mut signature_array = [0u8; 64];
    signature_array.copy_from_slice(&signature_bytes);
    let signature = Signature::from_bytes(&signature_array);
    verifying_key
        .verify(canonical, &signature)
        .map_err(|_| SIGNATURE_MISMATCH.to_owned())
}

fn verify_signature(
    manifest: &Manifest,
    key: Option<&str>,
    ed25519_key: Option<&str>,
) -> Result<(), String> {
    let Some(signature) = &manifest.signature else {
        return Ok(());
    };
    let Some((algorithm, expected)) = signature.split_once(':') else {
        return Err("manifest signature is malformed".to_owned());
    };
    let canonical = canonical_manifest(manifest)?;
    match algorithm {
        SIGN_ALGORITHM => {
            let key = key.ok_or_else(|| {
                "bundle is signed with hmac-sha256 but no verification key was given".to_owned()
            })?;
            let actual = hex_lower(&hmac_sha256(key.as_bytes(), &canonical));
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(SIGNATURE_MISMATCH.to_owned());
            }
        }
        SIGN_ALGORITHM_ED25519 => {
            let key = ed25519_key.ok_or_else(|| {
                "bundle is signed with ed25519 but no Ed25519 verification key was given".to_owned()
            })?;
            verify_ed25519(&canonical, key, expected)?;
        }
        _ => {
            return Err(format!(
                "unsupported manifest signature algorithm `{algorithm}`"
            ));
        }
    }
    Ok(())
}

// ---- export -----------------------------------------------------------------

fn export(
    out: Option<&Path>,
    include_backups: bool,
    sign_key: Option<&str>,
    sign_key_ed25519: Option<&str>,
) -> Result<(), String> {
    let home = vikram_home();
    if !home.is_dir() {
        return Err(format!("no VIKRAM home at {}", home.display()));
    }
    let exported_ms = vikram_runtime::types::now_ms();
    let tmp = tempfile::tempdir().map_err(|e| format!("cannot create a temp dir: {e}"))?;
    let bundle = tmp.path().join("bundle");
    std::fs::create_dir_all(&bundle)
        .map_err(|e| format!("cannot create {}: {e}", bundle.display()))?;

    let mut notes: Vec<String> = Vec::new();

    for name in ["config.json", "gateway.json", TRANSFORMS_FILE] {
        let src = home.join(name);
        if !src.is_file() {
            continue;
        }
        let bytes = if name == "config.json" {
            let (bytes, redacted) = sanitize_config(&src)?;
            if redacted > 0 {
                notes.push(format!(
                    "config.json: redacted {redacted} literal secret field(s)"
                ));
            }
            bytes
        } else {
            std::fs::read(&src).map_err(|e| format!("cannot read {}: {e}", src.display()))?
        };
        write_bundle_file(&bundle, Path::new(name), &bytes)?;
    }

    let plugins = home.join("plugins");
    if plugins.is_dir() {
        copy_tree(&plugins, &bundle.join("plugins"), &|path| {
            !include_backups && path.file_name().is_some_and(|name| name == ".backups")
        })?;
        if !include_backups && plugins.join(".backups").is_dir() {
            notes.push(
                "plugins/.backups excluded (rollback copies; pass --include-backups to keep)"
                    .to_owned(),
            );
        }
    }

    let skills = home.join("skills");
    if skills.is_dir() {
        copy_tree(&skills, &bundle.join("skills"), &|_| false)?;
    }

    export_entities(&home, &bundle)?;

    let entries = manifest_entries(&bundle)?;
    let mut manifest = Manifest {
        version: FORMAT_VERSION.to_owned(),
        exported_ms,
        entries,
        requirements: Requirements {
            vikram_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        signature: None,
    };
    let hmac_key = dist_key(sign_key);
    let ed25519_key = dist_ed25519_key(sign_key_ed25519);
    if hmac_key.is_some() && ed25519_key.is_some() {
        return Err(
            "choose one signing scheme: --sign-key (hmac-sha256) or --sign-key-ed25519 (ed25519)"
                .to_owned(),
        );
    }
    if let Some(key) = ed25519_key {
        manifest.signature = Some(sign_manifest_ed25519(&manifest, &key)?);
    } else if let Some(key) = hmac_key {
        manifest.signature = Some(sign_manifest(&manifest, &key)?);
    }
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?
    );
    std::fs::write(bundle.join("MANIFEST.json"), body)
        .map_err(|e| format!("cannot write MANIFEST.json: {e}"))?;

    let out = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("vikram-dist-{exported_ms}.tar.gz")));
    pack_bundle(tmp.path(), &out)?;

    let counts = kind_counts(&manifest.entries);
    println!(
        "exported {} file(s) to {}",
        manifest.entries.len(),
        out.display()
    );
    for (kind, count) in counts {
        println!("  {kind}: {count}");
    }
    for note in &notes {
        println!("note: {note}");
    }
    println!(
        "excluded: .env and every credential, {}, sessions, pairing, logs",
        {
            let db = vikram_runtime::config::database_path();
            db.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "vikram.db".to_owned())
        }
    );
    for entry in unexported_top_level(&home) {
        println!("  skipped {entry}");
    }
    Ok(())
}

/// Write `bytes` at `rel` inside the staging bundle, creating parents.
fn write_bundle_file(bundle: &Path, rel: &Path, bytes: &[u8]) -> Result<(), String> {
    let target = bundle.join(rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(&target, bytes).map_err(|e| format!("cannot write {}: {e}", target.display()))
}

/// Copy `src`'s tree into `dst`, skipping any path `skip` accepts. Returns files copied.
fn copy_tree(src: &Path, dst: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<usize, String> {
    let mut copied = 0usize;
    copy_tree_into(src, dst, skip, &mut copied)?;
    Ok(copied)
}

fn copy_tree_into(
    src: &Path,
    dst: &Path,
    skip: &dyn Fn(&Path) -> bool,
    copied: &mut usize,
) -> Result<(), String> {
    for entry in std::fs::read_dir(src)
        .map_err(|e| format!("cannot read {}: {e}", src.display()))?
        .flatten()
    {
        let from = entry.path();
        if skip(&from) {
            continue;
        }
        let name = from
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        let to = dst.join(&name);
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", from.display()))?;
        if kind.is_dir() {
            std::fs::create_dir_all(&to)
                .map_err(|e| format!("cannot create {}: {e}", to.display()))?;
            copy_tree_into(&from, &to, skip, copied)?;
        } else if kind.is_file() || kind.is_symlink() {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
            std::fs::copy(&from, &to)
                .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))?;
            *copied += 1;
        }
    }
    Ok(())
}

/// Top-level home entries the bundle does not carry, so the operator sees the boundary.
fn unexported_top_level(home: &Path) -> Vec<String> {
    const EXPORTED: &[&str] = &[
        "config.json",
        "gateway.json",
        TRANSFORMS_FILE,
        "plugins",
        "skills",
        "agents",
        "bots",
        "main-agents",
    ];
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !EXPORTED.contains(&name.as_str()) {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

/// Stage `entities/<kind>/<slug>/` for every entity context directory, with an
/// `entity.json` manifest generated from the store row (or the slug when the row is
/// gone) so `dist import` can reinstall it through the agent-pack path.
fn export_entities(home: &Path, bundle: &Path) -> Result<(), String> {
    let kinds = [EntityKind::MainAgent, EntityKind::Agent, EntityKind::Bot];
    let mut rows: Vec<EntityRow> = Vec::new();
    let mut store_opened = false;
    for kind in kinds {
        let dir = home.join(kind.folder());
        if !dir.is_dir() {
            continue;
        }
        if !store_opened {
            if let Ok(store) = crate::open_store() {
                rows = store.list_entities().unwrap_or_default();
            }
            store_opened = true;
        }
        let mut slugs: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        slugs.sort();
        for slug_dir in slugs {
            let slug = slug_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if slug.is_empty() {
                continue;
            }
            let dest = bundle.join("entities").join(kind.as_str()).join(&slug);
            copy_tree(&slug_dir, &dest, &|_| false)?;
            let row = rows.iter().find(|row| row.kind == kind && row.slug == slug);
            let manifest = entity_manifest(kind, &slug, row);
            write_bundle_file(&dest, Path::new("entity.json"), &manifest)?;
        }
    }
    Ok(())
}

fn entity_manifest(kind: EntityKind, slug: &str, row: Option<&EntityRow>) -> Vec<u8> {
    let pick = |f: fn(&EntityRow) -> &str| row.map(f).unwrap_or("");
    let value = serde_json::json!({
        "kind": kind.as_str(),
        "name": row.map(|r| r.name.as_str()).unwrap_or(slug),
        "slug": slug,
        "title": pick(|r| r.title.as_str()),
        "role": pick(|r| r.role.as_str()),
        "specialization": pick(|r| r.specialization.as_str()),
        "description": pick(|r| r.description.as_str()),
    });
    let mut body = serde_json::to_vec_pretty(&value).expect("entity manifest is serializable");
    body.push(b'\n');
    body
}

/// Parse `config.json` and replace the value of every known secret-shaped key with a
/// placeholder. The file must be valid JSON: an unparseable config is refused rather
/// than exported raw, because a hand-edited file could hold a literal key.
fn sanitize_config(src: &Path) -> Result<(Vec<u8>, usize), String> {
    let raw =
        std::fs::read_to_string(src).map_err(|e| format!("cannot read {}: {e}", src.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "{} is not valid JSON ({e}); refusing to export it unredacted",
            src.display()
        )
    })?;
    let mut redacted = 0usize;
    redact_secrets(&mut value, &mut redacted);
    let mut body = serde_json::to_vec_pretty(&value).expect("config is serializable");
    body.push(b'\n');
    Ok((body, redacted))
}

fn redact_secrets(value: &mut serde_json::Value, redacted: &mut usize) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                let secret = SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str());
                if secret && matches!(item, serde_json::Value::String(s) if !s.is_empty()) {
                    *item = serde_json::Value::String("<redacted>".to_owned());
                    *redacted += 1;
                } else {
                    redact_secrets(item, redacted);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_secrets(item, redacted);
            }
        }
        _ => {}
    }
}

/// Hash every regular file under `bundle` (except `MANIFEST.json`, written afterwards)
/// into manifest entries, sorted by path for reproducibility.
fn manifest_entries(bundle: &Path) -> Result<Vec<ManifestEntry>, String> {
    let mut files = Vec::new();
    collect_files(bundle, bundle, &mut files)?;
    files.sort();
    let mut entries = Vec::with_capacity(files.len());
    for rel in files {
        let bytes = std::fs::read(bundle.join(&rel))
            .map_err(|e| format!("cannot read {}: {e}", rel.display()))?;
        entries.push(ManifestEntry {
            path: rel.to_string_lossy().replace('\\', "/"),
            sha256: sha256_hex(&bytes),
            kind: kind_for(&rel).to_owned(),
        });
    }
    Ok(entries)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .flatten()
    {
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        if rel == Path::new("MANIFEST.json") {
            continue;
        }
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if kind.is_dir() {
            collect_files(root, &path, out)?;
        } else if kind.is_file() {
            out.push(rel);
        }
    }
    Ok(())
}

fn kind_for(rel: &Path) -> &'static str {
    let mut parts = rel.components().filter_map(|c| match c {
        Component::Normal(s) => Some(s.to_string_lossy()),
        _ => None,
    });
    match parts.next().as_deref() {
        Some("config.json") => "config",
        Some("gateway.json") => "gateway",
        Some(TRANSFORMS_FILE) => "transforms",
        Some("plugins") => "plugin",
        Some("skills") => match parts.next().as_deref() {
            Some(".vikram") => "skill-state",
            _ => "skill",
        },
        Some("entities") => "entity",
        _ => "file",
    }
}

fn kind_counts(entries: &[ManifestEntry]) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in entries {
        *counts.entry(entry.kind.clone()).or_default() += 1;
    }
    counts.into_iter().collect()
}

/// `tar` with the same deterministic flags `vikram module pack` uses.
fn pack_bundle(staging: &Path, out: &Path) -> Result<(), String> {
    let status = std::process::Command::new("tar")
        .args([
            "--sort=name",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--use-compress-program=gzip -n",
            "-cf",
        ])
        .arg(out)
        .arg("-C")
        .arg(staging)
        .arg("bundle")
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "packing needs the `tar` executable, which was not found".to_owned()
            } else {
                format!("cannot run tar: {e}")
            }
        })?;
    if !status.success() {
        return Err(format!(
            "tar exited with {status} while writing {}",
            out.display()
        ));
    }
    Ok(())
}

// ---- list -------------------------------------------------------------------

fn list(file: &Path) -> Result<(), String> {
    let manifest = read_manifest(file)?;
    println!(
        "format:    {} (vikram {})",
        manifest.version, manifest.requirements.vikram_version
    );
    println!("exported:  {} ms", manifest.exported_ms);
    println!(
        "signature: {}",
        manifest
            .signature
            .as_deref()
            .map(|s| s.split(':').next().unwrap_or(s))
            .unwrap_or("none")
    );
    println!("entries:   {}", manifest.entries.len());
    for entry in &manifest.entries {
        println!(
            "{:<12} {:<64} {}",
            entry.kind,
            entry.path,
            &entry.sha256[..entry.sha256.len().min(12)]
        );
    }
    Ok(())
}

// ---- import -----------------------------------------------------------------

fn import(
    file: &Path,
    yes: bool,
    force: bool,
    verify_key: Option<&str>,
    verify_key_ed25519: Option<&str>,
    insecure: bool,
) -> Result<(), String> {
    if !archive::looks_like_archive(file) {
        return Err(format!("{} is not a `.tar.gz` dist bundle", file.display()));
    }
    let extracted = archive::extract_module_dir(file)?;
    let root = extracted.path();
    let manifest = read_manifest_from(root)?;

    if manifest.version != FORMAT_VERSION {
        return Err(format!(
            "unsupported bundle format version `{}` (this build reads `{FORMAT_VERSION}`)",
            manifest.version
        ));
    }
    let current = env!("CARGO_PKG_VERSION");
    if !manifest.requirements.vikram_version.is_empty()
        && major(&manifest.requirements.vikram_version) != major(current)
    {
        eprintln!(
            "warning: bundle exported by vikram {}, this is {current}",
            manifest.requirements.vikram_version
        );
    }

    // A carried signature is verified before anything else; it is never bypassed by
    // --force, only by the explicit --insecure opt-out. The key needed depends on the
    // signature's algorithm tag.
    if manifest.signature.is_some() {
        let algorithm = manifest
            .signature
            .as_deref()
            .and_then(|signature| signature.split_once(':'))
            .map(|(algorithm, _)| algorithm);
        let hmac_key = dist_key(verify_key);
        let ed25519_key = dist_ed25519_key(verify_key_ed25519);
        let have_key = if algorithm == Some(SIGN_ALGORITHM_ED25519) {
            ed25519_key.is_some()
        } else {
            hmac_key.is_some()
        };
        if have_key {
            verify_signature(&manifest, hmac_key.as_deref(), ed25519_key.as_deref())?;
        } else if insecure {
            eprintln!(
                "warning: --insecure: importing a signed bundle without verifying its signature"
            );
        } else {
            return Err(
                "bundle is signed but no verification key was given; pass --verify-key/--verify-key-ed25519 or set VIKRAM_DIST_KEY/VIKRAM_DIST_ED25519_KEY (use --insecure to accept the risk)"
                    .to_owned(),
            );
        }
    }

    let problems = verify_bundle(root, &manifest);
    if !problems.is_empty() {
        if !force {
            return Err(format!(
                "bundle failed verification ({} problem(s)); nothing was installed:\n{}",
                problems.len(),
                problems.join("\n")
            ));
        }
        eprintln!(
            "warning: --force: installing despite {} verification problem(s)",
            problems.len()
        );
        for problem in &problems {
            eprintln!("  {problem}");
        }
    }

    let plan = ImportPlan::build(root, &manifest)?;
    plan.print_plan();
    if !yes {
        confirm(&format!(
            "install {} unit(s) from {}? [y/N] ",
            plan.units(),
            file.display()
        ))?;
    }
    plan.apply()
}

fn major(version: &str) -> &str {
    version.split('.').next().unwrap_or(version)
}

fn read_manifest(file: &Path) -> Result<Manifest, String> {
    if !archive::looks_like_archive(file) {
        return Err(format!("{} is not a `.tar.gz` dist bundle", file.display()));
    }
    let extracted = archive::extract_module_dir(file)?;
    read_manifest_from(extracted.path())
}

fn read_manifest_from(root: &Path) -> Result<Manifest, String> {
    let path = root.join("MANIFEST.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("{} is not a vikram dist bundle: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("invalid MANIFEST.json: {e}"))
}

/// A manifest path must stay inside the bundle: relative, no `..`, no prefix, no `\`.
fn safe_rel(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() || path.contains('\\') || path.contains('\0') {
        return Err(format!("unsafe bundle path `{path}`"));
    }
    let rel = Path::new(path);
    if rel.is_absolute() {
        return Err(format!("unsafe bundle path `{path}`: absolute"));
    }
    let mut out = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => out.push(part),
            _ => return Err(format!("unsafe bundle path `{path}`")),
        }
    }
    if out.as_os_str().is_empty() {
        return Err("empty bundle path".to_owned());
    }
    Ok(out)
}

/// Check every manifest entry exists with the recorded SHA-256, and that no file in the
/// bundle went unlisted. Returns human-readable problems; empty means verified.
fn verify_bundle(root: &Path, manifest: &Manifest) -> Vec<String> {
    let mut problems = Vec::new();
    let mut listed: BTreeSet<String> = BTreeSet::new();
    for entry in &manifest.entries {
        let rel = match safe_rel(&entry.path) {
            Ok(rel) => rel,
            Err(err) => {
                problems.push(err);
                continue;
            }
        };
        listed.insert(entry.path.clone());
        let target = root.join(&rel);
        if !target.is_file() {
            problems.push(format!("missing {}", entry.path));
            continue;
        }
        let bytes = match std::fs::read(&target) {
            Ok(bytes) => bytes,
            Err(err) => {
                problems.push(format!("cannot read {}: {err}", entry.path));
                continue;
            }
        };
        let actual = sha256_hex(&bytes);
        if actual != entry.sha256 {
            problems.push(format!(
                "checksum mismatch {}: manifest {}, actual {}",
                entry.path, entry.sha256, actual
            ));
        }
    }
    // Anything on disk the manifest does not name is smuggled content.
    let mut on_disk = Vec::new();
    if collect_files(root, root, &mut on_disk).is_ok() {
        for rel in on_disk {
            let key = rel.to_string_lossy().replace('\\', "/");
            if !listed.contains(&key) {
                problems.push(format!("unlisted file in bundle: {key}"));
            }
        }
    }
    problems
}

/// What import will do, grouped into installable units plus the singleton state files.
struct ImportPlan {
    root: PathBuf,
    /// `(rel dir under root, display label)` for plugins/skills/entities, install order:
    /// plugins, skills, entities.
    plugins: Vec<PathBuf>,
    skills: Vec<PathBuf>,
    entities: Vec<PathBuf>,
    /// `skills/.vikram/**` files copied into place verbatim.
    skill_state: Vec<PathBuf>,
    config: Option<PathBuf>,
    gateway: Option<PathBuf>,
    transforms: Option<PathBuf>,
}

impl ImportPlan {
    fn build(root: &Path, manifest: &Manifest) -> Result<Self, String> {
        let mut plugins = BTreeSet::new();
        let mut skills = BTreeSet::new();
        let mut entities = BTreeSet::new();
        let mut skill_state = BTreeSet::new();
        let mut config = None;
        let mut gateway = None;
        let mut transforms = None;
        for entry in &manifest.entries {
            let rel = safe_rel(&entry.path)?;
            let comps: Vec<String> = rel
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect();
            match comps.first().map(String::as_str) {
                Some("plugins") if comps.len() >= 2 => {
                    plugins.insert(comps[..2].iter().collect::<PathBuf>());
                }
                Some("skills") if comps.len() >= 2 => {
                    if comps[1] == ".vikram" {
                        skill_state.insert(rel.clone());
                    } else {
                        skills.insert(comps[..2].iter().collect::<PathBuf>());
                    }
                }
                Some("entities") if comps.len() >= 3 => {
                    entities.insert(comps[..3].iter().collect::<PathBuf>());
                }
                Some("config.json") => config = Some(rel.clone()),
                Some("gateway.json") => gateway = Some(rel.clone()),
                Some(TRANSFORMS_FILE) => transforms = Some(rel.clone()),
                _ => {}
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            plugins: plugins.into_iter().collect(),
            skills: skills.into_iter().collect(),
            entities: entities.into_iter().collect(),
            skill_state: skill_state.into_iter().collect(),
            config,
            gateway,
            transforms,
        })
    }

    fn units(&self) -> usize {
        self.plugins.len()
            + self.skills.len()
            + self.entities.len()
            + self.skill_state.len()
            + self.config.iter().count()
            + self.gateway.iter().count()
            + self.transforms.iter().count()
    }

    fn print_plan(&self) {
        println!("bundle contents:");
        for dir in &self.plugins {
            println!("  plugin:   {}", dir.display());
        }
        for dir in &self.skills {
            println!("  skill:    {}", dir.display());
        }
        for dir in &self.entities {
            println!("  entity:   {}", dir.display());
        }
        for file in &self.skill_state {
            println!("  skill-db: {}", file.display());
        }
        for (label, file) in [
            ("config", &self.config),
            ("gateway", &self.gateway),
            ("transforms", &self.transforms),
        ] {
            if file.is_some() {
                println!("  {label}:");
            }
        }
    }

    fn apply(self) -> Result<(), String> {
        let home = vikram_home();
        vikram_runtime::config::ensure_home().map_err(|e| e.to_string())?;
        let mut failures: Vec<String> = Vec::new();
        let mut done = 0usize;

        let registry = PluginRegistry::from_default_dir();
        for rel in &self.plugins {
            let dir = self.root.join(rel);
            match install_plugin(&registry, &dir) {
                Ok(line) => {
                    println!("{line}");
                    done += 1;
                }
                Err(err) => failures.push(format!("{}: {err}", rel.display())),
            }
        }

        let installer = Installer {
            manager: Manager::new(),
            allow_network: false,
        };
        for rel in &self.skills {
            let dir = self.root.join(rel);
            match installer.install(&InstallSource::Directory(dir.clone())) {
                Ok(outcome) => {
                    println!(
                        "installed skill {} at {}",
                        outcome.name,
                        outcome.path.display()
                    );
                    done += 1;
                }
                Err(err) => failures.push(format!("{}: {err}", rel.display())),
            }
        }

        let mut store = None;
        for rel in &self.entities {
            if store.is_none() {
                match crate::open_store() {
                    Ok(opened) => store = Some(opened),
                    Err(err) => {
                        failures.push(format!("store: {err}"));
                        break;
                    }
                }
            }
            let dir = self.root.join(rel);
            match install_entity(store.as_ref().unwrap(), &dir) {
                Ok(line) => {
                    println!("{line}");
                    done += 1;
                }
                Err(err) => failures.push(format!("{}: {err}", rel.display())),
            }
        }

        for rel in &self.skill_state {
            let src = self.root.join(rel);
            // rel is `skills/.vikram/<rest>`; the destination keeps the same tail.
            let tail = rel.components().skip(2).collect::<PathBuf>();
            let dst = home.join("skills").join(".vikram").join(tail);
            match copy_one(&src, &dst) {
                Ok(()) => done += 1,
                Err(err) => failures.push(format!("{}: {err}", rel.display())),
            }
        }

        if let Some(rel) = &self.config {
            match install_config(&self.root.join(rel)) {
                Ok(()) => {
                    println!("installed config.json (previous copy backed up)");
                    done += 1;
                }
                Err(err) => failures.push(format!("config.json: {err}")),
            }
        }
        if let Some(rel) = &self.gateway {
            match merge_gateway(&self.root.join(rel)) {
                Ok(conflicts) => {
                    if conflicts.is_empty() {
                        println!("merged gateway.json");
                    } else {
                        println!(
                            "merged gateway.json; kept local toggles for: {}",
                            conflicts.join(", ")
                        );
                    }
                    done += 1;
                }
                Err(err) => failures.push(format!("gateway.json: {err}")),
            }
        }
        if let Some(rel) = &self.transforms {
            match install_transforms(&self.root.join(rel)) {
                Ok(()) => {
                    println!(
                        "restored {TRANSFORMS_FILE}; run `vikram transforms apply <plugin>` per plugin to re-apply"
                    );
                    done += 1;
                }
                Err(err) => failures.push(format!("{TRANSFORMS_FILE}: {err}")),
            }
        }

        println!("{done} unit(s) applied");
        if !failures.is_empty() {
            return Err(format!(
                "{} unit(s) failed:\n{}",
                failures.len(),
                failures.join("\n")
            ));
        }
        Ok(())
    }
}

fn install_plugin(registry: &PluginRegistry, dir: &Path) -> Result<String, String> {
    let manifest =
        PluginManifest::from_dir(dir).map_err(|err| format!("not a plugin directory: {err}"))?;
    let id = manifest.id().to_owned();
    let target = plugins_dir().join(&id);
    if target.join("plugin.json").is_file() {
        return Ok(format!("skipped plugin {id}: already installed"));
    }
    let installed = registry.install(dir)?;
    Ok(format!("installed plugin {installed}"))
}

fn install_entity(store: &vikram_store::Store, dir: &Path) -> Result<String, String> {
    let spec = module_sdk::agent_pack(dir)?;
    let already = store
        .list_entities()
        .map_err(|e| e.to_string())?
        .iter()
        .any(|row| row.kind == spec.kind && row.slug == spec.slug);
    if already {
        return Ok(format!(
            "skipped entity {} ({}): already installed",
            spec.slug,
            spec.kind.as_str()
        ));
    }
    let entity_dir = vikram_home().join(spec.kind.folder()).join(&spec.slug);
    std::fs::create_dir_all(&entity_dir)
        .map_err(|e| format!("cannot create {}: {e}", entity_dir.display()))?;
    // Copy the whole directory — context files plus anything else the operator added —
    // except the generated `entity.json`, which is install metadata, not context.
    for entry in std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .flatten()
    {
        let from = entry.path();
        if !entry.file_type().map(|k| k.is_file()).unwrap_or(false)
            || from.file_name().is_some_and(|n| n == "entity.json")
        {
            continue;
        }
        std::fs::copy(&from, entity_dir.join(from.file_name().unwrap()))
            .map_err(|e| format!("cannot install {}: {e}", from.display()))?;
    }
    let now = vikram_runtime::types::now_ms();
    let row = EntityRow {
        id: vikram_contracts::EntityId::generate(),
        owner_user: None,
        kind: spec.kind,
        name: spec.name.clone(),
        slug: spec.slug.clone(),
        title: spec.title.clone(),
        role: if spec.role.is_empty() {
            spec.kind.as_str().to_owned()
        } else {
            spec.role.clone()
        },
        specialization: spec.specialization.clone(),
        description: if spec.description.is_empty() {
            if spec.title.is_empty() {
                format!("{} ({})", spec.name, spec.kind.as_str())
            } else {
                format!("{} — {}", spec.name, spec.title)
            }
        } else {
            spec.description.clone()
        },
        icon: None,
        model: None,
        profile_id: None,
        status: "active".to_owned(),
        created_ms: now,
        updated_ms: now,
    };
    store.save_entity(&row).map_err(|e| e.to_string())?;
    Ok(format!(
        "installed entity {} ({}) at {}",
        row.id,
        spec.kind.as_str(),
        entity_dir.display()
    ))
}

fn copy_one(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::copy(src, dst)
        .map_err(|e| format!("cannot copy {} to {}: {e}", src.display(), dst.display()))?;
    Ok(())
}

/// Replace `config.json` with the bundle's copy, keeping the previous file as
/// `config.json.bak-<ms>` first.
fn install_config(src: &Path) -> Result<(), String> {
    let dst = config_path();
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    if dst.is_file() {
        let backup = dst.with_file_name(format!(
            "config.json.bak-{}",
            vikram_runtime::types::now_ms()
        ));
        std::fs::copy(&dst, &backup).map_err(|e| {
            format!(
                "cannot back up {} to {}: {e}",
                dst.display(),
                backup.display()
            )
        })?;
    }
    std::fs::copy(src, &dst)
        .map_err(|e| format!("cannot install {} to {}: {e}", src.display(), dst.display()))?;
    Ok(())
}

/// Merge the bundle's platform toggles into the local `gateway.json`: platforms the
/// bundle does not mention stay untouched, and a local toggle that disagrees with the
/// bundle's wins — the conflict is reported, never silently overwritten. Returns the
/// names kept local.
fn merge_gateway(src: &Path) -> Result<Vec<String>, String> {
    let raw =
        std::fs::read_to_string(src).map_err(|e| format!("cannot read {}: {e}", src.display()))?;
    let imported: GatewayConfig =
        serde_json::from_str(&raw).map_err(|e| format!("invalid gateway.json: {e}"))?;
    let mut local = gateway_config::load()?;
    let mut conflicts = Vec::new();
    for (name, toggle) in imported.platforms {
        match local.platforms.get(&name) {
            Some(existing) if existing.enabled != toggle.enabled => {
                conflicts.push(name);
            }
            Some(_) => {}
            None => {
                local.platforms.insert(
                    name,
                    PlatformToggle {
                        enabled: toggle.enabled,
                    },
                );
            }
        }
    }
    local.save()?;
    Ok(conflicts)
}

/// Restore `transforms.json` verbatim (backing up an existing one) so the apply records
/// survive; the transforms themselves still have to be re-applied per plugin.
fn install_transforms(src: &Path) -> Result<(), String> {
    let raw =
        std::fs::read_to_string(src).map_err(|e| format!("cannot read {}: {e}", src.display()))?;
    // It must at least be JSON; the runtime re-validates on every transforms command.
    serde_json::from_str::<serde_json::Value>(&raw)
        .map_err(|e| format!("invalid {TRANSFORMS_FILE}: {e}"))?;
    let dst = vikram_home().join(TRANSFORMS_FILE);
    if dst.is_file() {
        let backup = dst.with_file_name(format!(
            "{TRANSFORMS_FILE}.bak-{}",
            vikram_runtime::types::now_ms()
        ));
        std::fs::copy(&dst, &backup).map_err(|e| {
            format!(
                "cannot back up {} to {}: {e}",
                dst.display(),
                backup.display()
            )
        })?;
    }
    std::fs::copy(src, &dst)
        .map_err(|e| format!("cannot install {} to {}: {e}", src.display(), dst.display()))?;
    Ok(())
}

pub(crate) fn confirm(question: &str) -> Result<(), String> {
    use std::io::Write;
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return Err("refusing to continue without --yes (stdin is not a terminal)".to_owned());
    }
    eprint!("{question}");
    std::io::stderr().flush().map_err(|err| err.to_string())?;
    let mut answer = String::new();
    stdin
        .read_line(&mut answer)
        .map_err(|err| format!("cannot read the answer: {err}"))?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err("aborted".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn list_tar(archive: &Path) -> Vec<String> {
        let out = Command::new("tar")
            .args(["-tzf"])
            .arg(archive)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Populate a home with one of everything the bundle carries.
    fn populate(home: &Path) {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(
            home.join("config.json"),
            r#"{"base_url":"https://example/v1","model":"m","api_key_env":"K","api_key":"sk-literal"}"#,
        )
        .unwrap();
        std::fs::write(
            home.join("gateway.json"),
            r#"{"platforms":{"telegram":{"enabled":true},"discord":{"enabled":false}}}"#,
        )
        .unwrap();
        std::fs::write(home.join("transforms.json"), r#"{"plugins":{}}"#).unwrap();
        std::fs::write(home.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(home.join("vikram.db"), "db").unwrap();
        std::fs::create_dir_all(home.join("pairing")).unwrap();
        std::fs::write(home.join("pairing/p.json"), "{}").unwrap();

        let plugin = home.join("plugins/acme");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("plugin.json"),
            r#"{"id":"acme","name":"acme","version":"1.0.0"}"#,
        )
        .unwrap();
        let backup = home.join("plugins/.backups/acme");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(backup.join("plugin.json"), "{}").unwrap();

        let skill = home.join("skills/greet");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: greet\ndescription: says hi\n---\nhello\n",
        )
        .unwrap();
        let skill_state = home.join("skills/.vikram");
        std::fs::create_dir_all(&skill_state).unwrap();
        std::fs::write(skill_state.join("disabled.json"), "[]").unwrap();

        for folder in ["agents", "bots", "main-agents"] {
            let dir = home.join(folder).join("helper");
            std::fs::create_dir_all(&dir).unwrap();
            for file in ["SOUL.md", "INSTRUCTIONS.md", "MEMORY.md", "TOOLS.md"] {
                std::fs::write(dir.join(file), format!("# {file}\n")).unwrap();
            }
        }
    }

    #[test]
    fn export_produces_a_verified_bundle_and_never_carries_secrets() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        let home = tmp.path().join("home");
        unsafe { std::env::set_var("VIKRAM_HOME", &home) };
        populate(&home);

        let out = tmp.path().join("dist.tar.gz");
        export(Some(&out), false, None, None).unwrap();
        assert!(out.is_file());

        let names = list_tar(&out);
        let joined = names.join("\n");
        assert!(joined.contains("MANIFEST.json"), "{joined}");
        assert!(joined.contains("plugins/acme/plugin.json"), "{joined}");
        assert!(joined.contains("skills/greet/SKILL.md"), "{joined}");
        assert!(
            joined.contains("entities/agent/helper/entity.json"),
            "{joined}"
        );
        assert!(joined.contains("skills/.vikram/disabled.json"), "{joined}");
        assert!(!joined.contains(".env"), "{joined}");
        assert!(!joined.contains("vikram.db"), "{joined}");
        assert!(!joined.contains("pairing"), "{joined}");
        assert!(!joined.contains(".backups"), "{joined}");

        let extracted = archive::extract_module_dir(&out).unwrap();
        let manifest = read_manifest_from(extracted.path()).unwrap();
        assert_eq!(manifest.version, FORMAT_VERSION);
        assert!(verify_bundle(extracted.path(), &manifest).is_empty());

        // The literal key in config.json was redacted before it was hashed in.
        let config = std::fs::read_to_string(extracted.path().join("config.json")).unwrap();
        assert!(config.contains("<redacted>"), "{config}");
        assert!(!config.contains("sk-literal"), "{config}");
        assert!(config.contains("api_key_env"), "{config}");

        // --include-backups keeps the rollback copies.
        let out2 = tmp.path().join("dist2.tar.gz");
        export(Some(&out2), true, None, None).unwrap();
        assert!(list_tar(&out2).join("\n").contains(".backups"));
    }

    #[test]
    fn import_refuses_a_bundle_with_a_corrupted_entry() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        let home = tmp.path().join("home");
        unsafe { std::env::set_var("VIKRAM_HOME", &home) };
        populate(&home);
        let bundle = tmp.path().join("dist.tar.gz");
        export(Some(&bundle), false, None, None).unwrap();

        // Repack with one file tampered: extract, edit, re-archive under the same shape.
        let stage = tmp.path().join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let status = Command::new("tar")
            .args(["-xzf"])
            .arg(&bundle)
            .arg("-C")
            .arg(&stage)
            .status()
            .unwrap();
        assert!(status.success());
        let tampered = stage.join("bundle/plugins/acme/plugin.json");
        std::fs::write(
            &tampered,
            r#"{"id":"acme","name":"acme","version":"9.9.9"}"#,
        )
        .unwrap();
        let corrupt = tmp.path().join("corrupt.tar.gz");
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(&corrupt)
            .arg("-C")
            .arg(&stage)
            .arg("bundle")
            .status()
            .unwrap();
        assert!(status.success());

        let target = tmp.path().join("target");
        unsafe { std::env::set_var("VIKRAM_HOME", &target) };
        let err = import(&corrupt, true, false, None, None, false).unwrap_err();
        assert!(err.contains("failed verification"), "{err}");
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!target.join("plugins/acme/plugin.json").exists());
        // --force gets past verification and reports the unit outcome honestly.
        let result = import(&corrupt, true, true, None, None, false);
        assert!(result.is_ok() || result.unwrap_err().contains("unit(s) failed"));
    }

    #[test]
    fn import_installs_every_kind_and_backs_up_config() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        let source = tmp.path().join("source");
        unsafe { std::env::set_var("VIKRAM_HOME", &source) };
        populate(&source);
        // populate() leaves a placeholder vikram.db; the store needs a real one.
        std::fs::remove_file(source.join("vikram.db")).unwrap();
        // Give the bot entity a store row so its metadata round-trips.
        {
            let store = crate::open_store().unwrap();
            let now = vikram_runtime::types::now_ms();
            store
                .save_entity(&EntityRow {
                    id: vikram_contracts::EntityId::generate(),
                    owner_user: None,
                    kind: EntityKind::Bot,
                    name: "Helper Bot".to_owned(),
                    slug: "helper".to_owned(),
                    title: "Helper".to_owned(),
                    role: "bot".to_owned(),
                    specialization: "ops".to_owned(),
                    description: "does things".to_owned(),
                    icon: None,
                    model: None,
                    profile_id: None,
                    status: "active".to_owned(),
                    created_ms: now,
                    updated_ms: now,
                })
                .unwrap();
        }
        let bundle = tmp.path().join("dist.tar.gz");
        export(Some(&bundle), false, None, None).unwrap();

        // Fresh target home with its own config and a conflicting gateway toggle.
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("config.json"), r#"{"model":"old"}"#).unwrap();
        std::fs::write(
            target.join("gateway.json"),
            r#"{"platforms":{"telegram":{"enabled":false},"irc":{"enabled":true}}}"#,
        )
        .unwrap();
        unsafe { std::env::set_var("VIKRAM_HOME", &target) };

        import(&bundle, true, false, None, None, false).unwrap();

        assert!(target.join("plugins/acme/plugin.json").is_file());
        assert!(target.join("skills/greet/SKILL.md").is_file());
        assert!(target.join("skills/.vikram/disabled.json").is_file());
        assert!(target.join("bots/helper/SOUL.md").is_file());
        assert!(target.join("agents/helper/MEMORY.md").is_file());
        assert!(target.join("main-agents/helper/TOOLS.md").is_file());
        // The entity row was re-registered with the exported metadata.
        let store = crate::open_store().unwrap();
        let entities = store.list_entities().unwrap();
        let bot = entities
            .iter()
            .find(|row| row.kind == EntityKind::Bot && row.slug == "helper")
            .expect("bot entity row");
        assert_eq!(bot.name, "Helper Bot");
        assert_eq!(bot.specialization, "ops");

        // config.json replaced, previous kept as a timestamped backup.
        let backups: Vec<_> = std::fs::read_dir(&target)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("config.json.bak-"))
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        let installed = std::fs::read_to_string(target.join("config.json")).unwrap();
        assert!(installed.contains("<redacted>"), "{installed}");
        assert!(!installed.contains("sk-literal"));

        // gateway.json merged: telegram keeps the local `false` (conflict), discord and
        // irc survive from their respective sides.
        let gateway: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(target.join("gateway.json")).unwrap())
                .unwrap();
        let platforms = &gateway["platforms"];
        assert_eq!(platforms["telegram"]["enabled"], false);
        assert_eq!(platforms["discord"]["enabled"], false);
        assert_eq!(platforms["irc"]["enabled"], true);

        assert!(target.join(TRANSFORMS_FILE).is_file());
    }

    fn extract_archive(archive: &Path, dest: &Path) {
        let status = Command::new("tar")
            .args(["-xzf"])
            .arg(archive)
            .arg("-C")
            .arg(dest)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn repack_bundle(stage: &Path, out: &Path) {
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(out)
            .arg("-C")
            .arg(stage)
            .arg("bundle")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn signed_bundle_verifies_and_unsigned_still_imports_with_a_key() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        let home = tmp.path().join("home");
        unsafe { std::env::set_var("VIKRAM_HOME", &home) };
        populate(&home);
        std::fs::remove_file(home.join("vikram.db")).unwrap();

        let signed = tmp.path().join("signed.tar.gz");
        export(Some(&signed), false, Some("correct horse"), None).unwrap();
        let extracted = archive::extract_module_dir(&signed).unwrap();
        let manifest = read_manifest_from(extracted.path()).unwrap();
        let signature = manifest.signature.as_deref().expect("manifest signed");
        assert!(signature.starts_with("hmac-sha256:"), "{signature}");

        // An unsigned bundle ignores a supplied verify key and imports as before.
        let plain = tmp.path().join("plain.tar.gz");
        export(Some(&plain), false, None, None).unwrap();
        let target_plain = tmp.path().join("target-plain");
        unsafe { std::env::set_var("VIKRAM_HOME", &target_plain) };
        import(&plain, true, false, Some("correct horse"), None, false).unwrap();
        assert!(target_plain.join("plugins/acme/plugin.json").is_file());

        // The signed bundle verifies with the right key.
        let target = tmp.path().join("target");
        unsafe { std::env::set_var("VIKRAM_HOME", &target) };
        import(&signed, true, false, Some("correct horse"), None, false).unwrap();
        assert!(target.join("plugins/acme/plugin.json").is_file());
        assert!(target.join("skills/greet/SKILL.md").is_file());
    }

    #[test]
    fn signed_bundle_refuses_without_a_key_or_with_the_wrong_key() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        let home = tmp.path().join("home");
        unsafe { std::env::set_var("VIKRAM_HOME", &home) };
        populate(&home);
        std::fs::remove_file(home.join("vikram.db")).unwrap();

        let bundle = tmp.path().join("signed.tar.gz");
        export(Some(&bundle), false, Some("correct horse"), None).unwrap();

        let target = tmp.path().join("target");
        unsafe { std::env::set_var("VIKRAM_HOME", &target) };

        let err = import(&bundle, true, false, None, None, false).unwrap_err();
        assert!(err.contains("signed"), "{err}");
        assert!(!target.join("plugins/acme/plugin.json").exists());

        let err = import(&bundle, true, false, Some("wrong key"), None, false).unwrap_err();
        assert!(err.contains("signature"), "{err}");
        assert!(!target.join("plugins/acme/plugin.json").exists());

        // --force does not bypass a signature failure either.
        let err = import(&bundle, true, true, Some("wrong key"), None, false).unwrap_err();
        assert!(err.contains("signature"), "{err}");

        // The explicit opt-out imports it anyway.
        import(&bundle, true, false, None, None, true).unwrap();
        assert!(target.join("plugins/acme/plugin.json").is_file());
    }

    #[test]
    fn tampered_manifest_fails_signature_verification() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        let home = tmp.path().join("home");
        unsafe { std::env::set_var("VIKRAM_HOME", &home) };
        populate(&home);
        std::fs::remove_file(home.join("vikram.db")).unwrap();

        let bundle = tmp.path().join("signed.tar.gz");
        export(Some(&bundle), false, Some("correct horse"), None).unwrap();

        let stage = tmp.path().join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        extract_archive(&bundle, &stage);
        // Rewrite the manifest's exported_ms: entries still check out, the signature does not.
        let manifest_path = stage.join("bundle/MANIFEST.json");
        let mut manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        manifest["exported_ms"] = serde_json::json!(manifest["exported_ms"].as_u64().unwrap() + 1);
        std::fs::write(
            &manifest_path,
            format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap()),
        )
        .unwrap();
        let tampered = tmp.path().join("tampered.tar.gz");
        repack_bundle(&stage, &tampered);

        let target = tmp.path().join("target");
        unsafe { std::env::set_var("VIKRAM_HOME", &target) };
        let err = import(&tampered, true, false, Some("correct horse"), None, false).unwrap_err();
        assert!(err.contains("signature"), "{err}");
        assert!(!target.join("plugins/acme/plugin.json").exists());
    }

    const ED25519_SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const ED25519_OTHER_SEED: &str =
        "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb";

    fn ed25519_public_hex(seed_hex: &str) -> String {
        let seed = hex_decode(seed_hex, 32).unwrap();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&seed);
        hex_lower(&SigningKey::from_bytes(&bytes).verifying_key().to_bytes())
    }

    fn unsigned_manifest() -> Manifest {
        Manifest {
            version: FORMAT_VERSION.to_owned(),
            exported_ms: 1,
            entries: Vec::new(),
            requirements: Requirements {
                vikram_version: String::new(),
            },
            signature: None,
        }
    }

    #[test]
    fn ed25519_signature_round_trips_and_rejects_tampering() {
        let public_hex = ed25519_public_hex(ED25519_SEED);
        let mut manifest = unsigned_manifest();
        manifest.signature = Some(sign_manifest_ed25519(&manifest, ED25519_SEED).unwrap());
        assert!(
            manifest
                .signature
                .as_deref()
                .unwrap()
                .starts_with("ed25519:")
        );

        verify_signature(&manifest, None, Some(&public_hex)).unwrap();

        // A tampered manifest fails even with the right public key.
        let mut tampered = manifest.clone();
        tampered.exported_ms += 1;
        let err = verify_signature(&tampered, None, Some(&public_hex)).unwrap_err();
        assert!(err.contains("signature"), "{err}");

        // A different keypair's public key does not verify.
        let other_hex = ed25519_public_hex(ED25519_OTHER_SEED);
        let err = verify_signature(&manifest, None, Some(&other_hex)).unwrap_err();
        assert!(err.contains("signature"), "{err}");

        // The HMAC key cannot stand in for the Ed25519 key.
        let err = verify_signature(&manifest, Some("correct horse"), None).unwrap_err();
        assert!(err.contains("no Ed25519 verification key"), "{err}");

        // Malformed key material is refused before verification.
        let err = verify_signature(&manifest, None, Some("not-hex")).unwrap_err();
        assert!(err.contains("invalid Ed25519 verification key"), "{err}");
        let err = sign_manifest_ed25519(&manifest, "deadbeef").unwrap_err();
        assert!(err.contains("invalid Ed25519 signing key"), "{err}");
    }

    #[test]
    fn hmac_signature_still_verifies_and_unknown_algorithms_are_refused() {
        let mut manifest = unsigned_manifest();
        manifest.signature = Some(sign_manifest(&manifest, "correct horse").unwrap());
        assert!(
            manifest
                .signature
                .as_deref()
                .unwrap()
                .starts_with("hmac-sha256:")
        );
        verify_signature(&manifest, Some("correct horse"), None).unwrap();
        let err = verify_signature(&manifest, Some("wrong key"), None).unwrap_err();
        assert!(err.contains("signature"), "{err}");

        // An Ed25519 signature does not verify when only the HMAC key is supplied.
        let public_hex = ed25519_public_hex(ED25519_SEED);
        let mut ed_manifest = unsigned_manifest();
        ed_manifest.signature = Some(sign_manifest_ed25519(&ed_manifest, ED25519_SEED).unwrap());
        verify_signature(&ed_manifest, None, Some(&public_hex)).unwrap();
        let err = verify_signature(&ed_manifest, Some("correct horse"), None).unwrap_err();
        assert!(err.contains("no Ed25519 verification key"), "{err}");

        manifest.signature = Some("rsa-sha256:deadbeef".to_owned());
        let err = verify_signature(&manifest, Some("correct horse"), None).unwrap_err();
        assert!(
            err.contains("unsupported manifest signature algorithm"),
            "{err}"
        );
    }

    #[test]
    fn ed25519_signed_bundle_exports_and_imports_with_the_public_key() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        let home = tmp.path().join("home");
        unsafe { std::env::set_var("VIKRAM_HOME", &home) };
        populate(&home);
        std::fs::remove_file(home.join("vikram.db")).unwrap();

        let public_hex = ed25519_public_hex(ED25519_SEED);
        let bundle = tmp.path().join("ed25519.tar.gz");
        export(Some(&bundle), false, None, Some(ED25519_SEED)).unwrap();
        let extracted = archive::extract_module_dir(&bundle).unwrap();
        let manifest = read_manifest_from(extracted.path()).unwrap();
        assert!(
            manifest
                .signature
                .as_deref()
                .expect("manifest signed")
                .starts_with("ed25519:")
        );

        // No key: refused unless --insecure.
        let target = tmp.path().join("target");
        unsafe { std::env::set_var("VIKRAM_HOME", &target) };
        let err = import(&bundle, true, false, None, None, false).unwrap_err();
        assert!(err.contains("signed"), "{err}");

        // The public key alone verifies — the seed never leaves the signer.
        import(&bundle, true, false, None, Some(&public_hex), false).unwrap();
        assert!(target.join("plugins/acme/plugin.json").is_file());

        // A wrong public key fails signature verification.
        let other_hex = ed25519_public_hex(ED25519_OTHER_SEED);
        let err = import(&bundle, true, false, None, Some(&other_hex), false).unwrap_err();
        assert!(err.contains("signature"), "{err}");
    }
}
