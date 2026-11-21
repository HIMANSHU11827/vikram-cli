//! Plugin-declared file transforms: `vikram transforms`.
//!
//! A plugin manifest may declare `transforms` — file operations applied explicitly with
//! `vikram transforms apply <id>`, tracked in `$VIKRAM_HOME/transforms.json`, and reverted with
//! `vikram transforms rollback <id>`. v1 allows writes only under `skills/`, the one managed root
//! the runtime actually consumes (skill packs are discovered from `$VIKRAM_HOME/skills`), so an
//! applied transform is live on the next `vikram skills` scan — no rebuild needed. Rust source
//! hot-modification is not possible; transforms never touch the workspace or crate sources.
//!
//! Ownership is deny-first: a transform may create or replace a file only when the path does not
//! exist, or when `transforms.json` records that this same plugin already owns it. A path owned
//! by a *different* plugin, or an existing file nobody owns, is a conflict and refuses the whole
//! apply before anything is written. `vikram transforms apply --force` is the explicit operator
//! override: the applying plugin takes ownership, the displaced owner's record for the path is
//! removed, and the handoff is logged in `transforms.json` under `superseded` — never silently.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use clap::Subcommand;
use serde::{Deserialize, Serialize};
use vikram_runtime::config::vikram_home;
use vikram_runtime::plugins::{PluginManifest, PluginRegistry, TransformOp, sha256_hex};

/// The state file recording what each plugin's transforms wrote.
pub const TRANSFORMS_FILE: &str = "transforms.json";

/// Managed roots a transform path may live under, relative to the VIKRAM home. `skills/` is the
/// only root v1 sanctions: it is the directory skill discovery actually scans.
const ALLOWED_ROOTS: &[&str] = &["skills"];

/// Prior content is retained for rollback up to this size; larger pre-existing files are refused
/// at apply time rather than silently applied without a way back.
const MAX_STORED_BEFORE: u64 = 1024 * 1024;

#[derive(Debug, Subcommand)]
pub enum TransformsAction {
    /// List installed plugins that declare transforms and their apply status.
    List,
    /// Show one plugin's transforms and per-file status.
    Status { id: String },
    /// Apply a plugin's declared transforms.
    Apply {
        id: String,
        /// Take ownership of paths another plugin owns; the handoff is recorded as superseded.
        #[arg(long)]
        force: bool,
    },
    /// Revert everything a plugin's transforms wrote.
    Rollback { id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct AppliedOp {
    op: String,
    /// Path relative to the VIKRAM home, exactly as declared.
    path: String,
    sha256_before: Option<String>,
    sha256_after: String,
    existed_before: bool,
    /// Base64 of the file's prior content when `existed_before` and the file fit the cap.
    before_content_b64: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TransformRecord {
    #[serde(default)]
    ops: Vec<AppliedOp>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TransformState {
    #[serde(default)]
    plugins: BTreeMap<String, TransformRecord>,
    /// Ownership handoffs made by `apply --force`: the displaced owner's op for a path, kept so
    /// the transfer is auditable rather than silently dropped.
    #[serde(default)]
    superseded: Vec<SupersededOp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SupersededOp {
    /// Managed-root-relative path that changed hands.
    path: String,
    /// Plugin that previously owned the path.
    from: String,
    /// Plugin that took ownership.
    to: String,
    /// The displaced owner's last recorded op for the path.
    op: AppliedOp,
}

pub fn run(action: TransformsAction) -> Result<(), String> {
    match action {
        TransformsAction::List => list(),
        TransformsAction::Status { id } => status(&id),
        TransformsAction::Apply { id, force } => {
            apply(&id, force)?;
            Ok(())
        }
        TransformsAction::Rollback { id } => {
            rollback(&id)?;
            Ok(())
        }
    }
}

// ---- state ---------------------------------------------------------------

fn state_path() -> PathBuf {
    vikram_home().join(TRANSFORMS_FILE)
}

fn load_state() -> Result<TransformState, String> {
    let path = state_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TransformState::default());
        }
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    serde_json::from_str(&raw).map_err(|e| format!("invalid {}: {e}", path.display()))
}

fn save_state(state: &TransformState) -> Result<(), String> {
    let path = state_path();
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(state).map_err(|e| e.to_string())?
    );
    let tmp = parent.join(format!(".{TRANSFORMS_FILE}.tmp"));
    std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", path.display())
    })
}

// ---- plugin lookup --------------------------------------------------------

/// Resolve `id` (manifest id or directory name) to its installed plugin directory.
fn plugin_dir(id: &str) -> Result<PathBuf, String> {
    let registry = PluginRegistry::from_default_dir();
    registry
        .plugins()
        .iter()
        .filter(|p| p.installed)
        .find(|p| {
            p.name == id
                || std::path::Path::new(&p.path)
                    .file_name()
                    .map(|n| n == id)
                    .unwrap_or(false)
                || PluginManifest::from_dir(&p.path)
                    .map(|m| m.id() == id)
                    .unwrap_or(false)
        })
        .map(|p| p.path.clone())
        .ok_or_else(|| format!("plugin not found: {id}"))
}

fn declared_ops(id: &str) -> Result<(String, Vec<TransformOp>), String> {
    let dir = plugin_dir(id)?;
    let manifest = PluginManifest::from_dir(&dir)
        .map_err(|e| format!("cannot read the manifest of plugin `{id}`: {e}"))?;
    let canonical = manifest.id().to_owned();
    Ok((canonical, manifest.transforms))
}

// ---- path policy ------------------------------------------------------------

/// Resolve a declared transform path to an absolute target under an allowed root.
///
/// Only `Normal` components are accepted — no absolute paths, no `..`, no prefix — and the first
/// component must be one of [`ALLOWED_ROOTS`]. `skills/.vikram` (the skills state directory) is
/// refused outright.
fn resolve_target(rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    let path = Path::new(rel);
    let mut components = path.components();
    let first = components
        .next()
        .ok_or_else(|| "transform path is empty".to_string())?;
    let Component::Normal(root) = first else {
        return Err(format!(
            "transform path `{rel}` must be relative with no `.`/`..`/root components"
        ));
    };
    let root = root.to_string_lossy();
    if !ALLOWED_ROOTS.contains(&root.as_ref()) {
        return Err(format!(
            "transform path `{rel}` is outside the allowed roots ({})",
            ALLOWED_ROOTS.join(", ")
        ));
    }
    let mut parts: Vec<String> = vec![root.into_owned()];
    for part in components {
        match part {
            Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
            _ => {
                return Err(format!(
                    "transform path `{rel}` must stay inside the managed roots"
                ));
            }
        }
    }
    if parts.len() > 1 && parts[1] == ".vikram" {
        return Err(format!(
            "transform path `{rel}` targets the skills state directory"
        ));
    }
    if parts.len() < 2 {
        return Err(format!(
            "transform path `{rel}` names a directory, not a file"
        ));
    }
    Ok(vikram_home().join(parts.iter().collect::<PathBuf>()))
}

fn op_bytes(op: &TransformOp) -> Result<Vec<u8>, String> {
    match (&op.content, &op.content_b64) {
        (Some(_), Some(_)) => Err(format!(
            "transform `{}` sets both `content` and `content_b64`",
            op.path
        )),
        (None, None) => Err(format!(
            "transform `{}` sets neither `content` nor `content_b64`",
            op.path
        )),
        (Some(content), None) => Ok(content.as_bytes().to_vec()),
        (None, Some(b64)) => base64_decode(b64)
            .map_err(|e| format!("transform `{}` has invalid content_b64: {e}", op.path)),
    }
}

// ---- apply ------------------------------------------------------------------

/// Apply every transform `id` declares. Validates all ops before writing anything, so a
/// conflict or bad path refuses the whole apply with the filesystem untouched. With `force`,
/// paths owned by other plugins are taken over: the displaced owner's record for each path is
/// removed and the handoff appended to `state.superseded`.
pub fn apply(id: &str, force: bool) -> Result<usize, String> {
    let (canonical, ops) = declared_ops(id)?;
    if ops.is_empty() {
        println!("{id}: declares no transforms");
        return Ok(0);
    }
    let mut state = load_state()?;
    let prior = state.plugins.get(&canonical).cloned().unwrap_or_default();

    // Pass 1: resolve, decode and check every target before touching the disk.
    struct Prepared {
        op: TransformOp,
        target: PathBuf,
        bytes: Vec<u8>,
        existed_before: bool,
        before_content_b64: Option<String>,
        sha256_before: Option<String>,
        /// Carried over from a previous apply of the same path so rollback still returns to the
        /// original pre-transform state, not to this plugin's own earlier output.
        carried: Option<AppliedOp>,
        /// `(previous owner, displaced op)` when `--force` takes a path another plugin owns.
        taken_from: Option<(String, AppliedOp)>,
    }
    let mut prepared = Vec::with_capacity(ops.len());
    for op in &ops {
        let kind = op.op.trim();
        if kind != "add_file" && kind != "write_file" {
            return Err(format!(
                "plugin `{id}` declares unknown transform op `{}` for `{}`",
                op.op, op.path
            ));
        }
        let target = resolve_target(&op.path)?;
        let bytes = op_bytes(op)?;
        let rel = op.path.trim();
        let carried = prior.ops.iter().find(|p| p.path == rel).cloned();
        let foreign = state
            .plugins
            .iter()
            .find(|(other, record)| {
                *other != &canonical && record.ops.iter().any(|p| p.path == rel)
            })
            .and_then(|(other, record)| {
                record
                    .ops
                    .iter()
                    .find(|p| p.path == rel)
                    .map(|displaced| ((*other).clone(), displaced.clone()))
            });
        let existed_before = target.is_file();
        if carried.is_none() {
            if let Some((owner, _)) = &foreign {
                if !force {
                    return Err(format!(
                        "refusing to transform `{rel}`: {} is owned by plugin `{owner}`; \
                         re-run with --force to take ownership",
                        target.display()
                    ));
                }
            } else if existed_before && !force {
                return Err(format!(
                    "refusing to transform `{rel}`: {} already exists and is not owned by a \
                     plugin; remove it or choose another path (or re-run with --force)",
                    target.display()
                ));
            }
        }
        if kind == "add_file" && existed_before && carried.is_none() && !force {
            return Err(format!(
                "refusing to transform `{rel}`: {} already exists (add_file requires a new path)",
                target.display()
            ));
        }
        let taken_from = if carried.is_none() { foreign } else { None };
        // A pre-existing file we are allowed to replace must be small enough to restore.
        let (before_content_b64, sha256_before) = if existed_before && carried.is_none() {
            let data = std::fs::read(&target)
                .map_err(|e| format!("cannot read {}: {e}", target.display()))?;
            if data.len() as u64 > MAX_STORED_BEFORE {
                return Err(format!(
                    "refusing to transform `{}`: {} is {} bytes, over the {}-byte rollback cap",
                    op.path,
                    target.display(),
                    data.len(),
                    MAX_STORED_BEFORE
                ));
            }
            (Some(base64_encode(&data)), Some(sha256_hex(&data)))
        } else {
            (None, None)
        };
        prepared.push(Prepared {
            op: op.clone(),
            target,
            bytes,
            existed_before,
            before_content_b64,
            sha256_before,
            carried,
            taken_from,
        });
    }

    // Pass 2: write. A failure here leaves the state file untouched, so a partial write is at
    // worst an untracked file the next apply reports as a conflict.
    let handoffs: Vec<(String, AppliedOp)> = prepared
        .iter()
        .filter_map(|item| item.taken_from.clone())
        .collect();
    let mut records = Vec::with_capacity(prepared.len());
    for item in prepared {
        if let Some(parent) = item.target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        write_atomic_bytes(&item.target, &item.bytes)?;
        let record = match item.carried {
            Some(carried) => AppliedOp {
                op: item.op.op.trim().to_owned(),
                path: item.op.path.trim().to_owned(),
                sha256_before: carried.sha256_before,
                sha256_after: sha256_hex(&item.bytes),
                existed_before: carried.existed_before,
                before_content_b64: carried.before_content_b64,
            },
            None => AppliedOp {
                op: item.op.op.trim().to_owned(),
                path: item.op.path.trim().to_owned(),
                sha256_before: item.sha256_before,
                sha256_after: sha256_hex(&item.bytes),
                existed_before: item.existed_before,
                before_content_b64: item.before_content_b64,
            },
        };
        records.push(record);
    }

    // Ownership handoffs: each path this apply took from another plugin leaves that plugin's
    // record, and the displaced op is preserved under `superseded` so the transfer is auditable.
    for (from, displaced) in handoffs {
        let mut drop_owner = false;
        if let Some(record) = state.plugins.get_mut(&from) {
            record.ops.retain(|p| p.path != displaced.path);
            drop_owner = record.ops.is_empty();
        }
        if drop_owner {
            state.plugins.remove(&from);
        }
        state.superseded.push(SupersededOp {
            path: displaced.path.clone(),
            from: from.clone(),
            to: canonical.clone(),
            op: displaced.clone(),
        });
        println!(
            "took ownership of {} from {from} (recorded as superseded)",
            displaced.path
        );
    }

    state
        .plugins
        .insert(canonical.clone(), TransformRecord { ops: records });
    save_state(&state)?;
    println!("applied {} transform(s) for {canonical}", ops.len());
    Ok(ops.len())
}

// ---- status / list -----------------------------------------------------------

fn file_status(record: Option<&AppliedOp>, target: &Path) -> &'static str {
    match record {
        None => "pending",
        Some(applied) => match std::fs::read(target) {
            Ok(data) if sha256_hex(&data) == applied.sha256_after => "applied",
            _ => "stale",
        },
    }
}

pub fn status(id: &str) -> Result<(), String> {
    let (canonical, ops) = declared_ops(id)?;
    let state = load_state()?;
    let record = state.plugins.get(&canonical);
    if ops.is_empty() {
        println!("{id}: declares no transforms");
        return Ok(());
    }
    println!("{canonical}:");
    for op in &ops {
        let target = resolve_target(&op.path)?;
        let applied = record.and_then(|r| r.ops.iter().find(|p| p.path == op.path.trim()));
        println!(
            "  {:<10} {:<48} {}",
            op.op,
            op.path,
            file_status(applied, &target)
        );
    }
    Ok(())
}

fn list() -> Result<(), String> {
    let registry = PluginRegistry::from_default_dir();
    let state = load_state()?;
    let mut rows: Vec<(String, usize, usize)> = Vec::new();
    for plugin in registry.plugins().iter().filter(|p| p.installed) {
        let Ok(manifest) = PluginManifest::from_dir(&plugin.path) else {
            continue;
        };
        if manifest.transforms.is_empty() {
            continue;
        }
        let canonical = manifest.id().to_owned();
        let declared = manifest.transforms.len();
        let applied = state
            .plugins
            .get(&canonical)
            .map(|r| r.ops.len())
            .unwrap_or(0);
        rows.push((canonical, declared, applied));
    }
    if rows.is_empty() {
        println!("no installed plugins declare transforms");
        return Ok(());
    }
    println!("{:<24} {:<10} APPLIED", "PLUGIN", "DECLARED");
    for (id, declared, applied) in rows {
        println!("{id:<24} {declared:<10} {applied}");
    }
    Ok(())
}

// ---- rollback -----------------------------------------------------------------

/// Revert every file `id`'s transforms wrote. Files the transforms created are removed; files
/// that existed before are restored from retained content. A file modified since apply is left
/// alone and reported, and its record is kept so a later rollback can retry.
pub fn rollback(id: &str) -> Result<usize, String> {
    let (canonical, _) = declared_ops(id)?;
    let mut state = load_state()?;
    let Some(record) = state.plugins.get(&canonical).cloned() else {
        println!("{canonical}: no applied transforms");
        return Ok(0);
    };
    let mut reverted = 0usize;
    let mut kept: Vec<AppliedOp> = Vec::new();
    for applied in record.ops.iter().rev() {
        let target = match resolve_target(&applied.path) {
            Ok(target) => target,
            Err(err) => {
                eprintln!("warning: cannot resolve `{}`: {err}", applied.path);
                kept.push(applied.clone());
                continue;
            }
        };
        let current = std::fs::read(&target).ok();
        if applied.existed_before {
            // Only restore when the file still holds what the transform wrote.
            if let Some(data) = &current
                && sha256_hex(data) != applied.sha256_after
            {
                eprintln!(
                    "warning: {} was modified after apply; leaving it and keeping the record",
                    target.display()
                );
                kept.push(applied.clone());
                continue;
            }
            let Some(b64) = &applied.before_content_b64 else {
                eprintln!(
                    "warning: {} existed before apply but its prior content was not retained; \
                     leaving it and keeping the record",
                    target.display()
                );
                kept.push(applied.clone());
                continue;
            };
            let bytes = match base64_decode(b64) {
                Ok(bytes) => bytes,
                Err(err) => {
                    eprintln!(
                        "warning: cannot decode retained content for {}: {err}",
                        applied.path
                    );
                    kept.push(applied.clone());
                    continue;
                }
            };
            if let Err(err) = write_atomic_bytes(&target, &bytes) {
                eprintln!("warning: cannot restore {}: {err}", target.display());
                kept.push(applied.clone());
                continue;
            }
        } else {
            match current {
                Some(data) if sha256_hex(&data) != applied.sha256_after => {
                    eprintln!(
                        "warning: {} was modified after apply; leaving it and keeping the record",
                        target.display()
                    );
                    kept.push(applied.clone());
                    continue;
                }
                Some(_) => {
                    if let Err(err) = std::fs::remove_file(&target) {
                        eprintln!("warning: cannot remove {}: {err}", target.display());
                        kept.push(applied.clone());
                        continue;
                    }
                    prune_empty_dirs(&target);
                }
                None => {}
            }
        }
        reverted += 1;
    }
    if kept.is_empty() {
        state.plugins.remove(&canonical);
    } else {
        state
            .plugins
            .insert(canonical.clone(), TransformRecord { ops: kept });
    }
    save_state(&state)?;
    println!("rolled back {reverted} transform(s) for {canonical}");
    Ok(reverted)
}

/// Remove now-empty parent directories up to (but not including) the managed root, so a rolled
/// back `skills/foo/SKILL.md` does not leave `skills/foo/` behind.
fn prune_empty_dirs(target: &Path) {
    let mut dir = target.parent();
    while let Some(path) = dir {
        if !path.starts_with(vikram_home().join("skills")) || path == vikram_home() {
            break;
        }
        if std::fs::remove_dir(path).is_err() {
            break;
        }
        dir = path.parent();
    }
}

// ---- file + encoding helpers ----------------------------------------------------

fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "transform".to_string());
    let tmp = parent.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", path.display())
    })
}

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub(crate) fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut table = [255u8; 256];
    for (i, c) in B64_ALPHABET.iter().enumerate() {
        table[*c as usize] = i as u8;
    }
    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !clean.len().is_multiple_of(4) {
        return Err("length is not a multiple of 4".to_string());
    }
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    for chunk in clean.chunks(4) {
        let pad = chunk.iter().filter(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && !chunk[3].eq(&b'=')) {
            return Err("misplaced padding".to_string());
        }
        let mut vals = [0u8; 4];
        for (i, &b) in chunk.iter().enumerate() {
            if b == b'=' {
                vals[i] = 0;
            } else {
                let v = table[b as usize];
                if v == 255 {
                    return Err(format!("invalid character `{}`", b as char));
                }
                vals[i] = v;
            }
        }
        let n = ((vals[0] as u32) << 18)
            | ((vals[1] as u32) << 12)
            | ((vals[2] as u32) << 6)
            | vals[3] as u32;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct HomeGuard {
        dir: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let guard = vikram_runtime::config::lock_security_env();
            unsafe { std::env::set_var("VIKRAM_HOME", dir.path()) };
            Self { dir, _guard: guard }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // `_guard` is still held here, so no other env test can observe the removal.
            unsafe { std::env::remove_var("VIKRAM_HOME") };
        }
    }

    fn install_plugin(home: &Path, id: &str, transforms: &str) -> PathBuf {
        let dir = home.join("plugins").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("plugin.json"),
            format!(
                r#"{{"name": "{id}", "version": "1.0.0", "entry": "run.sh", "enabled": true, "transforms": {transforms}}}"#
            ),
        )
        .unwrap();
        fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
        dir
    }

    #[test]
    fn base64_round_trips() {
        for data in [b"".as_slice(), b"a", b"ab", b"abc", b"hello world"] {
            assert_eq!(base64_decode(&base64_encode(data)).unwrap(), data);
        }
        assert!(base64_decode("a").is_err());
        assert!(base64_decode("!!!!").is_err());
    }

    #[test]
    fn rejects_paths_outside_allowed_roots() {
        let _home = HomeGuard::new();
        assert!(resolve_target("skills/foo/SKILL.md").is_ok());
        for bad in [
            "plugins/x/plugin.json",
            "../escape.txt",
            "/etc/passwd",
            "skills/../plugins/x",
            "skills/.vikram/disabled.json",
            "skills",
            "",
        ] {
            assert!(resolve_target(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn apply_status_rollback_lifecycle() {
        let home = HomeGuard::new();
        let manifest = r##"[{"op":"add_file","path":"skills/demo-pack/SKILL.md","content":"# demo"},{"op":"write_file","path":"skills/demo-pack/extra.txt","content_b64":"aGk="}]"##;
        install_plugin(home.dir.path(), "demo", manifest);

        assert_eq!(apply("demo", false).unwrap(), 2);
        let skill = home.dir.path().join("skills/demo-pack/SKILL.md");
        assert_eq!(fs::read_to_string(&skill).unwrap(), "# demo");
        assert_eq!(
            fs::read_to_string(home.dir.path().join("skills/demo-pack/extra.txt")).unwrap(),
            "hi"
        );

        status("demo").unwrap();
        assert_eq!(rollback("demo").unwrap(), 2);
        assert!(!skill.exists());
        assert!(!home.dir.path().join("skills/demo-pack").exists());
        assert!(load_state().unwrap().plugins.is_empty());
    }

    #[test]
    fn refuses_to_overwrite_unowned_files() {
        let home = HomeGuard::new();
        let manifest = r#"[{"op":"write_file","path":"skills/mine.txt","content":"new"}]"#;
        install_plugin(home.dir.path(), "clash", manifest);
        let target = home.dir.path().join("skills/mine.txt");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "user data").unwrap();

        let err = apply("clash", false).unwrap_err();
        assert!(err.contains("not owned"), "{err}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "user data");
        assert!(load_state().unwrap().plugins.is_empty());
    }

    #[test]
    fn reapply_updates_owned_files_and_rollback_removes_them() {
        let home = HomeGuard::new();
        install_plugin(
            home.dir.path(),
            "owner",
            r#"[{"op":"add_file","path":"skills/shared.txt","content":"v1"}]"#,
        );
        assert_eq!(apply("owner", false).unwrap(), 1);
        install_plugin(
            home.dir.path(),
            "owner",
            r#"[{"op":"write_file","path":"skills/shared.txt","content":"v2"}]"#,
        );
        let target = home.dir.path().join("skills/shared.txt");
        assert_eq!(apply("owner", false).unwrap(), 1);
        assert_eq!(fs::read_to_string(&target).unwrap(), "v2");
        assert_eq!(rollback("owner").unwrap(), 1);
        assert!(!target.exists());
    }

    #[test]
    fn rollback_restores_retained_prior_content() {
        let home = HomeGuard::new();
        install_plugin(home.dir.path(), "patcher", "[]");
        let target = home.dir.path().join("skills/existing.txt");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "plugin output").unwrap();
        // A record as a modify-in-place apply would leave it: the file existed, its prior
        // content was retained, and the current bytes are what the transform wrote.
        let state = TransformState {
            plugins: BTreeMap::from([(
                "patcher".to_string(),
                TransformRecord {
                    ops: vec![AppliedOp {
                        op: "write_file".to_string(),
                        path: "skills/existing.txt".to_string(),
                        sha256_before: Some(sha256_hex(b"before")),
                        sha256_after: sha256_hex(b"plugin output"),
                        existed_before: true,
                        before_content_b64: Some(base64_encode(b"before")),
                    }],
                },
            )]),
            superseded: Vec::new(),
        };
        save_state(&state).unwrap();

        assert_eq!(rollback("patcher").unwrap(), 1);
        assert_eq!(fs::read_to_string(&target).unwrap(), "before");
        assert!(load_state().unwrap().plugins.is_empty());
    }

    #[test]
    fn rollback_refuses_modified_files() {
        let home = HomeGuard::new();
        install_plugin(
            home.dir.path(),
            "demo",
            r#"[{"op":"add_file","path":"skills/x/f.txt","content":"plugin"}]"#,
        );
        apply("demo", false).unwrap();
        let target = home.dir.path().join("skills/x/f.txt");
        fs::write(&target, "user edit").unwrap();

        assert_eq!(rollback("demo").unwrap(), 0);
        assert_eq!(fs::read_to_string(&target).unwrap(), "user edit");
        assert!(load_state().unwrap().plugins.contains_key("demo"));
    }

    #[test]
    fn foreign_owned_path_refuses_without_force() {
        let home = HomeGuard::new();
        install_plugin(
            home.dir.path(),
            "first",
            r#"[{"op":"write_file","path":"skills/shared.txt","content":"first"}]"#,
        );
        install_plugin(
            home.dir.path(),
            "second",
            r#"[{"op":"write_file","path":"skills/shared.txt","content":"second"}]"#,
        );
        assert_eq!(apply("first", false).unwrap(), 1);

        let err = apply("second", false).unwrap_err();
        assert!(err.contains("owned by plugin `first`"), "{err}");
        let target = home.dir.path().join("skills/shared.txt");
        assert_eq!(fs::read_to_string(&target).unwrap(), "first");
        let state = load_state().unwrap();
        assert!(state.plugins.contains_key("first"));
        assert!(!state.plugins.contains_key("second"));
        assert!(state.superseded.is_empty());
    }

    #[test]
    fn force_takes_ownership_and_records_handoff() {
        let home = HomeGuard::new();
        install_plugin(
            home.dir.path(),
            "first",
            r#"[{"op":"write_file","path":"skills/shared.txt","content":"first"}]"#,
        );
        install_plugin(
            home.dir.path(),
            "second",
            r#"[{"op":"write_file","path":"skills/shared.txt","content":"second"}]"#,
        );
        assert_eq!(apply("first", false).unwrap(), 1);

        assert_eq!(apply("second", true).unwrap(), 1);
        let target = home.dir.path().join("skills/shared.txt");
        assert_eq!(fs::read_to_string(&target).unwrap(), "second");

        let state = load_state().unwrap();
        // The displaced owner's record for the path is gone; its plugin entry is dropped once
        // empty, and the handoff is preserved under `superseded`.
        assert!(!state.plugins.contains_key("first"));
        let second_ops = &state.plugins.get("second").unwrap().ops;
        assert_eq!(second_ops.len(), 1);
        assert_eq!(second_ops[0].path, "skills/shared.txt");
        assert_eq!(state.superseded.len(), 1);
        let handoff = &state.superseded[0];
        assert_eq!(handoff.path, "skills/shared.txt");
        assert_eq!(handoff.from, "first");
        assert_eq!(handoff.to, "second");
        assert_eq!(handoff.op.sha256_after, sha256_hex(b"first"));

        // The new owner can roll back; the file returns to the displaced owner's last bytes.
        assert_eq!(rollback("second").unwrap(), 1);
        assert_eq!(fs::read_to_string(&target).unwrap(), "first");
    }

    #[test]
    fn force_adopts_unowned_existing_file() {
        let home = HomeGuard::new();
        install_plugin(
            home.dir.path(),
            "adopter",
            r#"[{"op":"write_file","path":"skills/mine.txt","content":"new"}]"#,
        );
        let target = home.dir.path().join("skills/mine.txt");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "user data").unwrap();

        assert!(apply("adopter", false).is_err());
        assert_eq!(apply("adopter", true).unwrap(), 1);
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(rollback("adopter").unwrap(), 1);
        assert_eq!(fs::read_to_string(&target).unwrap(), "user data");
    }

    #[test]
    fn unknown_op_and_missing_content_are_refused() {
        let home = HomeGuard::new();
        install_plugin(
            home.dir.path(),
            "bad",
            r#"[{"op":"delete_file","path":"skills/x","content":"x"}]"#,
        );
        assert!(
            apply("bad", false)
                .unwrap_err()
                .contains("unknown transform op")
        );
        install_plugin(
            home.dir.path(),
            "bad2",
            r#"[{"op":"add_file","path":"skills/x"}]"#,
        );
        assert!(apply("bad2", false).unwrap_err().contains("neither"));
    }
}
