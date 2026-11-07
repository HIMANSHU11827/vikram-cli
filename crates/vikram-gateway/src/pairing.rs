//! Code-based DM pairing (module 6 of the foundation).
//!
//! Port of hermes-agent `gateway/pairing.py`. An unknown sender asks for a pairing code; the
//! operator approves that code out of band and the sender is authorized from then on. This module
//! owns the grant record — it never decides access itself. [`authorize_source`] consults it
//! through [`PairingVerdict`] at step 6, after the env allowlists have failed.
//!
//! Why the store is shaped the way it is (each point is a security property, not an accident):
//!
//! - Codes come from a 32-character unambiguous alphabet (no `0/O`, no `1/I`) and only a salted
//!   SHA-256 of the code is written. The salt is 16 fresh bytes per code, so a stolen file cannot
//!   be replayed and cannot be brute-forced off a rainbow table.
//! - Codes expire after an hour, one sender may ask once per ten minutes, and a platform holds at
//!   most three pending requests, so a hostile client cannot flood the operator's approval queue.
//! - Five failed approvals lock the platform for an hour. The counter tracks *consecutive*
//!   failures and is cleared by a success, so isolated typos never accumulate into a lockout.
//! - State is split per platform (`<platform>-pending.json`, `<platform>-approved.json`) plus two
//!   shared files (`_rate_limits.json`, `_declined.json`), all written atomically (temp file in the
//!   same directory, `fsync`, `rename`) and `0600` on Unix. A crash leaves the previous file intact.
//! - Every read-modify-write runs under `<dir>/.lock`, an advisory exclusive `flock` with a ten
//!   second deadline. The dashboard builds a store per request and the CLI is a separate process,
//!   so an in-process mutex alone would lose updates; the kernel drops the lock when a holder dies,
//!   which is why there is no stale-lock cleanup to steal.
//!
//! Two deliberate departures from the Python, both forced by the crate:
//!
//! - The pairing directory is a constructor argument. hermes resolves it from `HERMES_HOME` and the
//!   active profile; profile/env resolution belongs to the caller (see the adapter layer), so this
//!   store stays a pure function of the directory it is given.
//! - Platform names are canonicalised through [`Platform`], which lowercases and trims. Hermes used
//!   the raw string, so `Telegram` and `telegram` were two stores there; adapters always emit the
//!   canonical name, and one store per platform is the safer reading.
//!
//! The silent-tolerance rules are kept: a missing, unreadable or non-object JSON file reads as
//! `{}` and malformed legacy entries are skipped rather than crashing an in-place upgrade.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::access::{PairingVerdict, whatsapp_auth_aliases};
use crate::event::{Platform, platform_names};
use crate::routing::canonical_whatsapp_identifier;

/// Unambiguous alphabet: `0/O` and `1/I` are excluded so a code can be read aloud.
pub const ALPHABET: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// Characters per pairing code.
pub const CODE_LENGTH: usize = 8;
/// Characters in a `list_pending` request id (hex of eight random bytes).
pub const REQUEST_ID_LENGTH: usize = 16;

/// Codes expire after one hour.
pub const CODE_TTL_SECONDS: f64 = 3600.0;
/// One request per sender per ten minutes.
pub const RATE_LIMIT_SECONDS: f64 = 600.0;
/// Lockout duration once the failure budget is spent.
pub const LOCKOUT_SECONDS: f64 = 3600.0;
/// Pending codes a single platform may hold.
pub const MAX_PENDING_PER_PLATFORM: usize = 3;
/// Consecutive failed approvals before the platform locks out.
pub const MAX_FAILED_ATTEMPTS: u32 = 5;
/// One polite decline per sender per day.
pub const DECLINE_DEDUPE_SECONDS: f64 = 24.0 * 3600.0;

/// Suffix of a platform's pending-request file.
const PENDING_SUFFIX: &str = "-pending.json";
/// Suffix of a platform's approved-user file.
const APPROVED_SUFFIX: &str = "-approved.json";
/// Shared rate-limit / lockout state file.
const RATE_LIMIT_FILE: &str = "_rate_limits.json";
/// Shared decline-stamp file.
const DECLINE_FILE: &str = "_declined.json";
/// Advisory lock file inside the pairing directory (a file, per the Python's `flock`).
const LOCK_FILE: &str = ".lock";

/// How long a writer waits for `<dir>/.lock` before failing.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval while another writer holds the lock.
const LOCK_RETRY: Duration = Duration::from_millis(50);

/// Owner read/write only. Codes are hashes, but the approved list is still operator data.
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;

/// Current wall-clock time in seconds, as a float like Python's `time.time()` (file values are
/// compared and stored as floats).
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// Everything that can go wrong mutating the pairing store.
#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    /// Filesystem failure; `path` is the file (or directory) being operated on.
    #[error("pairing store i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `platform` was empty or contained a path separator.
    #[error("invalid pairing platform: {0:?}")]
    InvalidPlatform(String),
    /// Another process held `<dir>/.lock` past the deadline.
    #[error("timed out after {timeout_secs}s waiting for the pairing store lock ({path})")]
    LockTimeout { path: PathBuf, timeout_secs: u64 },
    /// The OS refused to supply entropy for a code or salt.
    #[error("pairing store entropy failure: {0}")]
    Entropy(String),
}

/// The user a successful approval admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub user_id: String,
    pub user_name: String,
}

/// One row of [`PairingStore::list_approved`].
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovedUser {
    pub platform: String,
    pub user_id: String,
    pub user_name: String,
    /// Approval timestamp, when the entry carries one.
    pub approved_at: Option<f64>,
}

/// One row of [`PairingStore::list_pending`]. The code itself is never returned — only the
/// server-side `request_id` that [`PairingStore::approve_request`] accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRequest {
    pub platform: String,
    /// Empty for legacy pre-hash entries, which have no usable id.
    pub request_id: String,
    pub user_id: String,
    pub user_name: String,
    pub age_minutes: i64,
}

/// Parsed approved lists, keyed by platform, with the file identity they were read under.
type ApprovedCache = Mutex<BTreeMap<String, (FileFingerprint, Map<String, Value>)>>;

/// The per-platform pairing store rooted at one directory.
///
/// Files: `<platform>-pending.json`, `<platform>-approved.json`, `_rate_limits.json`,
/// `_declined.json`. Every mutation is serialized across threads and processes by
/// `<dir>/.lock` and published atomically.
#[derive(Debug)]
pub struct PairingStore {
    dir: PathBuf,
    /// Last parsed approved list per platform, keyed by the file's identity so a rewrite is seen
    /// immediately. Read-only fast path for [`Self::is_approved`], which runs on the event loop.
    approved_cache: ApprovedCache,
}

impl PairingStore {
    /// Open (creating when absent) the store rooted at `dir`.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, PairingError> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|source| PairingError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(Self {
            dir,
            approved_cache: Mutex::new(BTreeMap::new()),
        })
    }

    /// Directory this store owns.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    // ----- Approved users -----

    /// Whether `user_id` is approved on `platform`, under any alias form.
    ///
    /// Never takes the file lock: hermes runs this on the gateway event loop under the process
    /// mutex, and waiting up to ten seconds behind a CLI process would stall every chat.
    pub fn is_approved(&self, platform: &str, user_id: &str) -> bool {
        let Ok(platform) = platform_key(platform) else {
            return false;
        };
        let approved = self.load_approved(&platform);
        !matching_ids(&platform, &approved, user_id).is_empty()
    }

    /// Approved users, optionally limited to one platform.
    pub fn list_approved(&self, platform: Option<&str>) -> Result<Vec<ApprovedUser>, PairingError> {
        let mut users = Vec::new();
        for platform in self.platforms(platform, APPROVED_SUFFIX)? {
            for (user_id, info) in load_json(&self.approved_path(&platform)) {
                users.push(ApprovedUser {
                    platform: platform.clone(),
                    user_id,
                    user_name: info
                        .get("user_name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    approved_at: info.get("approved_at").and_then(Value::as_f64),
                });
            }
        }
        Ok(users)
    }

    /// Remove `user_id` (and every alias-equivalent entry) from the approved list.
    ///
    /// Alias matching matters: approve mirrors a normalized phone while revoke is often given a
    /// JID, and an exact delete would leave the sender authorized.
    pub fn revoke(&self, platform: &str, user_id: &str) -> Result<bool, PairingError> {
        let platform = platform_key(platform)?;
        self.locked(|| {
            let path = self.approved_path(&platform);
            let mut approved = load_json(&path);
            let ids = matching_ids(&platform, &approved, user_id);
            if ids.is_empty() {
                return Ok(false);
            }
            for id in ids {
                approved.remove(&id);
            }
            save_json(&path, &approved)?;
            self.forget_approved(&platform);
            Ok(true)
        })
    }

    // ----- Pending codes -----

    /// Issue a pairing code for `user_id`, or `None` when the platform is locked out, the sender
    /// asked too recently, or the platform already holds [`MAX_PENDING_PER_PLATFORM`] requests.
    ///
    /// Only the salted hash is persisted; the returned code is the only copy.
    pub fn generate_code(
        &self,
        platform: &str,
        user_id: &str,
        user_name: &str,
    ) -> Result<Option<String>, PairingError> {
        let platform = platform_key(platform)?;
        self.locked(|| {
            self.cleanup_expired_locked(&platform)?;
            let normalized_user_id = normalize_user_id(&platform, user_id);
            if self.is_locked_out(&platform) || self.is_rate_limited(&platform, user_id) {
                return Ok(None);
            }
            let path = self.pending_path(&platform);
            let mut pending = load_json(&path);
            if pending.len() >= MAX_PENDING_PER_PLATFORM {
                return Ok(None);
            }
            let code = random_code()?;
            let salt = random_bytes(16)?;
            let mut entry = Map::new();
            entry.insert("hash".to_string(), Value::String(hash_code(&code, &salt)));
            entry.insert("salt".to_string(), Value::String(hex_encode(&salt)));
            entry.insert("user_id".to_string(), Value::String(normalized_user_id));
            entry.insert(
                "user_name".to_string(),
                Value::String(user_name.to_string()),
            );
            entry.insert("created_at".to_string(), Value::from(now_secs()));
            // Keyed by a random entry id, never by the code itself.
            pending.insert(random_hex(8)?, Value::Object(entry));
            save_json(&path, &pending)?;
            self.record_rate_limit(&platform, user_id)?;
            Ok(Some(code))
        })
    }

    /// Approve the pending request that `code` belongs to, admitting its user.
    ///
    /// `None` means the code is unknown or expired — or the platform is locked out, which is
    /// checked first so an already-issued valid code cannot be used to walk around a lockout.
    /// Whitespace between characters is ignored, matching chat clients that insert visual spacing.
    pub fn approve_code(
        &self,
        platform: &str,
        code: &str,
    ) -> Result<Option<Approval>, PairingError> {
        let platform = platform_key(platform)?;
        self.locked(|| {
            self.cleanup_expired_locked(&platform)?;
            let code: String = code
                .chars()
                .filter(|c| !c.is_whitespace())
                .flat_map(char::to_uppercase)
                .collect();
            if self.is_locked_out(&platform) {
                return Ok(None);
            }
            let pending = load_json(&self.pending_path(&platform));
            let mut matched = None;
            for (entry_id, entry) in &pending {
                if !is_hashed_entry(entry) {
                    continue;
                }
                let Some(salt) = entry
                    .get("salt")
                    .and_then(Value::as_str)
                    .and_then(hex_decode)
                else {
                    continue;
                };
                let Some(stored) = entry.get("hash").and_then(Value::as_str) else {
                    continue;
                };
                if constant_time_eq(&hash_code(&code, &salt), stored) {
                    matched = Some(entry_id.clone());
                    break;
                }
            }
            let Some(entry_id) = matched else {
                self.record_failed_attempt(&platform)?;
                return Ok(None);
            };
            let entry = pending.get(&entry_id).cloned().unwrap_or(Value::Null);
            self.finish_approval(&platform, pending, &entry_id, &entry)
                .map(Some)
        })
    }

    /// True when `value` is shaped like a [`Self::list_pending`] request id.
    ///
    /// Callers that accept either a code or an id can dispatch on this: codes are eight uppercase
    /// characters, ids are sixteen hex characters.
    pub fn looks_like_request_id(value: &str) -> bool {
        let value = value.trim();
        value.len() == REQUEST_ID_LENGTH && value.bytes().all(|b| b.is_ascii_hexdigit())
    }

    /// Approve a pending request by its server-side id, for admin surfaces that must never reveal
    /// the code that was sent to the sender.
    ///
    /// Neither counts toward nor is gated by the brute-force lockout: an id is only obtainable by
    /// an authenticated operator, so a stale id means "that row expired", not an attack.
    pub fn approve_request(
        &self,
        platform: &str,
        request_id: &str,
    ) -> Result<Option<Approval>, PairingError> {
        let platform = platform_key(platform)?;
        self.locked(|| {
            self.cleanup_expired_locked(&platform)?;
            let request_id = request_id.trim().to_lowercase();
            if request_id.is_empty() {
                return Ok(None);
            }
            let pending = load_json(&self.pending_path(&platform));
            let matched = pending.iter().find_map(|(entry_id, entry)| {
                (is_hashed_entry(entry) && constant_time_eq(&entry_id.to_lowercase(), &request_id))
                    .then(|| entry_id.clone())
            });
            let Some(entry_id) = matched else {
                return Ok(None);
            };
            let entry = pending.get(&entry_id).cloned().unwrap_or(Value::Null);
            self.finish_approval(&platform, pending, &entry_id, &entry)
                .map(Some)
        })
    }

    /// Pending requests, optionally limited to one platform. Expired requests are pruned first.
    pub fn list_pending(
        &self,
        platform: Option<&str>,
    ) -> Result<Vec<PendingRequest>, PairingError> {
        let mut results = Vec::new();
        self.locked(|| {
            let now = now_secs();
            for platform in self.platforms(platform, PENDING_SUFFIX)? {
                self.cleanup_expired_locked(&platform)?;
                for (entry_id, info) in load_json(&self.pending_path(&platform)) {
                    let Some(created_at) = entry_created_at(&info) else {
                        continue;
                    };
                    let modern = info.get("hash").and_then(Value::as_str).is_some()
                        && info.get("salt").and_then(Value::as_str).is_some();
                    results.push(PendingRequest {
                        platform: platform.clone(),
                        request_id: if modern { entry_id } else { String::new() },
                        user_id: info
                            .get("user_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        user_name: info
                            .get("user_name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        age_minutes: ((now - created_at) / 60.0) as i64,
                    });
                }
            }
            Ok(())
        })?;
        Ok(results)
    }

    /// Drop every pending request, optionally on one platform only. Returns how many were dropped.
    pub fn clear_pending(&self, platform: Option<&str>) -> Result<usize, PairingError> {
        self.locked(|| {
            let mut count = 0;
            for platform in self.platforms(platform, PENDING_SUFFIX)? {
                let path = self.pending_path(&platform);
                count += load_json(&path).len();
                save_json(&path, &Map::new())?;
            }
            Ok(count)
        })
    }

    /// Drop pending requests of `platform` that are past [`CODE_TTL_SECONDS`].
    ///
    /// Entries with no numeric `created_at` (legacy or malformed) count as expired, so an
    /// in-place upgrade cannot leave a stale approval target behind.
    pub fn cleanup_expired(&self, platform: &str) -> Result<(), PairingError> {
        let platform = platform_key(platform)?;
        self.locked(|| self.cleanup_expired_locked(&platform))
    }

    // ----- Rate limiting, lockout, declines -----

    /// Whether `platform` is currently locked out after too many failed approvals.
    pub fn is_locked_out(&self, platform: &str) -> bool {
        let Ok(platform) = platform_key(platform) else {
            return false;
        };
        let now = now_secs();
        lockout_at(&load_json(&self.rate_limit_path()), &platform).is_some_and(|until| now < until)
    }

    /// Whether this sender (under any alias) was sent a polite decline within
    /// [`DECLINE_DEDUPE_SECONDS`].
    pub fn has_recent_decline(&self, platform: &str, user_id: &str) -> bool {
        let Ok(platform) = platform_key(platform) else {
            return false;
        };
        let stamps = load_json(&self.decline_path());
        let now = now_secs();
        user_id_aliases(&platform, user_id)
            .into_iter()
            .any(|alias| {
                number_at(&stamps, &scoped_key(&platform, &alias))
                    .is_some_and(|stamped| (now - stamped) < DECLINE_DEDUPE_SECONDS)
            })
    }

    /// Stamp this sender (all aliases) as declined and prune expired stamps.
    ///
    /// Callers stamp *before* sending, so a delivery failure cannot turn into a decline storm on
    /// the sender's next message.
    pub fn record_decline(&self, platform: &str, user_id: &str) -> Result<(), PairingError> {
        let platform = platform_key(platform)?;
        self.locked(|| {
            let path = self.decline_path();
            let now = now_secs();
            let mut stamps = Map::new();
            for (key, value) in load_json(&path) {
                if value
                    .as_f64()
                    .is_some_and(|stamped| (now - stamped) < DECLINE_DEDUPE_SECONDS)
                {
                    stamps.insert(key, value);
                }
            }
            for alias in user_id_aliases(&platform, user_id) {
                stamps.insert(scoped_key(&platform, &alias), Value::from(now));
            }
            save_json(&path, &stamps)
        })
    }

    // ----- Internals -----

    fn pending_path(&self, platform: &str) -> PathBuf {
        self.dir.join(format!("{platform}{PENDING_SUFFIX}"))
    }

    fn approved_path(&self, platform: &str) -> PathBuf {
        self.dir.join(format!("{platform}{APPROVED_SUFFIX}"))
    }

    fn rate_limit_path(&self) -> PathBuf {
        self.dir.join(RATE_LIMIT_FILE)
    }

    fn decline_path(&self) -> PathBuf {
        self.dir.join(DECLINE_FILE)
    }

    /// Run `op` under the cross-process lock, then the in-process one.
    ///
    /// Order matters and mirrors hermes: the file lock is taken first so a caller waiting on
    /// another process never blocks [`Self::is_approved`], which only takes the process mutex.
    fn locked<T>(&self, op: impl FnOnce() -> Result<T, PairingError>) -> Result<T, PairingError> {
        let _lock = StoreLock::acquire(&self.dir)?;
        op()
    }

    fn cleanup_expired_locked(&self, platform: &str) -> Result<(), PairingError> {
        let path = self.pending_path(platform);
        let pending = load_json(&path);
        let stored = pending.len();
        let now = now_secs();
        let live: Map<String, Value> = pending
            .into_iter()
            .filter(|(_, info)| {
                entry_created_at(info).is_some_and(|created| (now - created) <= CODE_TTL_SECONDS)
            })
            .collect();
        if live.len() != stored {
            save_json(&path, &live)?;
        }
        Ok(())
    }

    fn finish_approval(
        &self,
        platform: &str,
        mut pending: Map<String, Value>,
        entry_id: &str,
        entry: &Value,
    ) -> Result<Approval, PairingError> {
        pending.remove(entry_id);
        save_json(&self.pending_path(platform), &pending)?;
        // A successful approval proves legitimacy: a persisted brute-force streak must not carry
        // over, or isolated typos would accumulate into a spurious lockout.
        self.reset_failed_attempts(platform)?;
        let approval = Approval {
            user_id: string_at(entry, "user_id"),
            user_name: string_at(entry, "user_name"),
        };
        self.approve_user(platform, &approval.user_id, &approval.user_name)?;
        Ok(approval)
    }

    fn approve_user(
        &self,
        platform: &str,
        user_id: &str,
        user_name: &str,
    ) -> Result<(), PairingError> {
        let path = self.approved_path(platform);
        let mut approved = load_json(&path);
        let normalized_user_id = normalize_user_id(platform, user_id);
        for approved_user_id in matching_ids(platform, &approved, &normalized_user_id) {
            approved.remove(&approved_user_id);
        }
        let mut info = Map::new();
        info.insert(
            "user_name".to_string(),
            Value::String(user_name.to_string()),
        );
        info.insert("approved_at".to_string(), Value::from(now_secs()));
        approved.insert(normalized_user_id, Value::Object(info));
        save_json(&path, &approved)?;
        self.forget_approved(platform);
        Ok(())
    }

    fn load_approved(&self, platform: &str) -> Map<String, Value> {
        let path = self.approved_path(platform);
        let fingerprint = fs::metadata(&path)
            .ok()
            .map(|meta| FileFingerprint::of(&meta));
        if let Some(fingerprint) = fingerprint
            && let Ok(cache) = self.approved_cache.lock()
            && let Some((cached, data)) = cache.get(platform)
            && *cached == fingerprint
        {
            return data.clone();
        }
        let data = load_json(&path);
        if let Ok(mut cache) = self.approved_cache.lock() {
            match fingerprint {
                Some(fingerprint) => {
                    cache.insert(platform.to_string(), (fingerprint, data.clone()));
                }
                None => {
                    cache.remove(platform);
                }
            }
        }
        data
    }

    fn forget_approved(&self, platform: &str) {
        if let Ok(mut cache) = self.approved_cache.lock() {
            cache.remove(platform);
        }
    }

    fn is_rate_limited(&self, platform: &str, user_id: &str) -> bool {
        let limits = load_json(&self.rate_limit_path());
        let threshold = now_secs() - RATE_LIMIT_SECONDS;
        user_id_aliases(platform, user_id).into_iter().any(|alias| {
            number_at(&limits, &scoped_key(platform, &alias)).unwrap_or(0.0) > threshold
        })
    }

    fn record_rate_limit(&self, platform: &str, user_id: &str) -> Result<(), PairingError> {
        let path = self.rate_limit_path();
        let mut limits = load_json(&path);
        let now = now_secs();
        for alias in user_id_aliases(platform, user_id) {
            limits.insert(scoped_key(platform, &alias), Value::from(now));
        }
        save_json(&path, &limits)
    }

    fn record_failed_attempt(&self, platform: &str) -> Result<(), PairingError> {
        let path = self.rate_limit_path();
        let mut limits = load_json(&path);
        let failures_key = format!("_failures:{platform}");
        let failures = number_at(&limits, &failures_key).unwrap_or(0.0) as u32 + 1;
        limits.insert(failures_key, Value::from(failures));
        if failures >= MAX_FAILED_ATTEMPTS {
            let lockout_key = format!("_lockout:{platform}");
            limits.insert(lockout_key, Value::from(now_secs() + LOCKOUT_SECONDS));
            limits.insert(format!("_failures:{platform}"), Value::from(0));
        }
        save_json(&path, &limits)
    }

    fn reset_failed_attempts(&self, platform: &str) -> Result<(), PairingError> {
        let path = self.rate_limit_path();
        let mut limits = load_json(&path);
        let failures_key = format!("_failures:{platform}");
        if number_at(&limits, &failures_key).unwrap_or(0.0) != 0.0 {
            limits.insert(failures_key, Value::from(0));
            save_json(&path, &limits)?;
        }
        Ok(())
    }

    fn platforms(&self, platform: Option<&str>, suffix: &str) -> Result<Vec<String>, PairingError> {
        match platform {
            Some(platform) => Ok(vec![platform_key(platform)?]),
            None => self.all_platforms(suffix),
        }
    }

    /// Platforms that own a `<platform><suffix>` file. `_`-prefixed files hold shared state.
    fn all_platforms(&self, suffix: &str) -> Result<Vec<String>, PairingError> {
        let entries = fs::read_dir(&self.dir).map_err(|source| PairingError::Io {
            path: self.dir.clone(),
            source,
        })?;
        let mut platforms: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let stem = name.strip_suffix(suffix)?;
                (!stem.is_empty() && !stem.starts_with('_')).then(|| stem.to_string())
            })
            .collect();
        platforms.sort();
        platforms.dedup();
        Ok(platforms)
    }
}

/// The authorizer consults the store through this seam.
impl PairingVerdict for PairingStore {
    fn is_approved(&self, platform: &str, user_id: &str) -> bool {
        PairingStore::is_approved(self, platform, user_id)
    }
}

/// Identity of a file version, so [`PairingStore::is_approved`] can skip re-parsing an unchanged
/// approved list. The inode changes on every atomic rename, so a rewrite is always observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified_ns: u128,
    inode: u64,
    device: u64,
}

impl FileFingerprint {
    fn of(meta: &fs::Metadata) -> Self {
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                len: meta.len(),
                modified_ns,
                inode: meta.ino(),
                device: meta.dev(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                len: meta.len(),
                modified_ns,
                inode: 0,
                device: 0,
            }
        }
    }
}

/// Advisory exclusive lock over `<dir>/.lock`, released when the handle drops.
///
/// `flock` dies with its holder, so a crashed process can never wedge the store; that is also why
/// there is no stale-lock stealing. Contention is polled rather than blocked so the deadline is
/// ours to enforce.
struct StoreLock {
    path: PathBuf,
    file: File,
}

impl StoreLock {
    fn acquire(dir: &Path) -> Result<Self, PairingError> {
        let path = dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| PairingError::Io {
                path: path.clone(),
                source,
            })?;
        force_private_mode(&file, &path)?;

        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(LOCK_RETRY);
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(PairingError::LockTimeout {
                        path,
                        timeout_secs: LOCK_TIMEOUT.as_secs(),
                    });
                }
                Err(std::fs::TryLockError::Error(source)) => {
                    return Err(PairingError::Io { path, source });
                }
            }
        }
        stamp_holder(&file);
        Ok(Self { path, file })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Best effort: clear the holder stamp, then close so the kernel drops the lock.
        let _ = self.file.set_len(0);
        let _ = &self.path;
    }
}

/// Best-effort pid stamp so a timed-out waiter can name the holder.
fn stamp_holder(file: &File) {
    if file.set_len(0).is_ok() {
        let stamp = format!("{}\n", std::process::id());
        let mut handle = file;
        let _ = handle.write_all(stamp.as_bytes());
    }
}

fn platform_key(raw: &str) -> Result<String, PairingError> {
    let platform = Platform::new(raw).ok_or_else(|| PairingError::InvalidPlatform(raw.into()))?;
    if platform
        .as_str()
        .chars()
        .any(|c| c == '/' || c == '\\' || c == '\0')
    {
        return Err(PairingError::InvalidPlatform(raw.into()));
    }
    Ok(platform.as_str().to_string())
}

fn uses_whatsapp_identity(platform: &str) -> bool {
    matches!(
        platform,
        platform_names::WHATSAPP | platform_names::WHATSAPP_CLOUD
    )
}

/// Persisted / compared form of a user id. WhatsApp phone forms collapse to bare digits.
fn normalize_user_id(platform: &str, user_id: &str) -> String {
    let raw = user_id.trim();
    if uses_whatsapp_identity(platform) {
        let normalized = canonical_whatsapp_identifier(raw);
        if !normalized.is_empty() {
            return normalized;
        }
    }
    raw.to_string()
}

/// Every id that names the same principal, for authorization and allowlist matching.
fn user_id_aliases(platform: &str, user_id: &str) -> HashSet<String> {
    let raw = user_id.trim();
    if raw.is_empty() {
        return HashSet::new();
    }
    let mut aliases = HashSet::new();
    aliases.insert(raw.to_string());
    let normalized = normalize_user_id(platform, raw);
    if !normalized.is_empty() {
        aliases.insert(normalized);
    }
    if uses_whatsapp_identity(platform) {
        aliases.extend(whatsapp_auth_aliases(raw));
    }
    aliases.remove("");
    aliases
}

fn user_ids_match(platform: &str, left: &str, right: &str) -> bool {
    let left_aliases = user_id_aliases(platform, left);
    !left_aliases.is_empty() && !left_aliases.is_disjoint(&user_id_aliases(platform, right))
}

fn matching_ids(platform: &str, approved: &Map<String, Value>, user_id: &str) -> Vec<String> {
    approved
        .keys()
        .filter(|approved_user_id| user_ids_match(platform, approved_user_id, user_id))
        .cloned()
        .collect()
}

/// Key under which limit/decline stamps are stored.
fn scoped_key(platform: &str, alias: &str) -> String {
    format!("{platform}:{alias}")
}

fn lockout_at(limits: &Map<String, Value>, platform: &str) -> Option<f64> {
    number_at(limits, &format!("_lockout:{platform}"))
}

fn number_at(map: &Map<String, Value>, key: &str) -> Option<f64> {
    map.get(key).and_then(Value::as_f64)
}

fn string_at(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn is_hashed_entry(entry: &Value) -> bool {
    entry.get("salt").is_some() && entry.get("hash").is_some()
}

fn entry_created_at(info: &Value) -> Option<f64> {
    info.get("created_at").and_then(Value::as_f64)
}

/// Salted SHA-256 of a code, hex encoded: `sha256(salt ‖ utf8(code))`, exactly as hermes.
fn hash_code(code: &str, salt: &[u8]) -> String {
    let mut message = Vec::with_capacity(salt.len() + code.len());
    message.extend_from_slice(salt);
    message.extend_from_slice(code.as_bytes());
    sha256::hex_digest(&message)
}

/// Length-checked, branch-free comparison (`secrets.compare_digest`).
fn constant_time_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(value: &str) -> Option<Vec<u8>> {
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

fn random_bytes(len: usize) -> Result<Vec<u8>, PairingError> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(|err| PairingError::Entropy(err.to_string()))?;
    Ok(bytes)
}

/// Eight characters from [`ALPHABET`]. A byte masked to five bits indexes the 32-char alphabet
/// exactly, so no rejection sampling is needed and every character is equally likely.
fn random_code() -> Result<String, PairingError> {
    let bytes = random_bytes(CODE_LENGTH)?;
    Ok(bytes
        .iter()
        .map(|byte| ALPHABET.as_bytes()[usize::from(*byte & 0x1f)] as char)
        .collect())
}

fn random_hex(bytes: usize) -> Result<String, PairingError> {
    random_bytes(bytes).map(|random| hex_encode(&random))
}

/// Read a JSON object; `{}` when the file is missing, unreadable, malformed, or not an object.
///
/// This tolerance is load-bearing: a half-written legacy file must never take the gateway down,
/// it must only read as "nobody is approved".
fn load_json(path: &Path) -> Map<String, Value> {
    let Ok(raw) = fs::read(path) else {
        return Map::new();
    };
    let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw);
    let Ok(text) = std::str::from_utf8(raw) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Publish `data` atomically: temp file in the same directory, `fsync`, `rename`, `0600`.
fn save_json(path: &Path, data: &Map<String, Value>) -> Result<(), PairingError> {
    let json = serde_json::to_string_pretty(data).map_err(|err| PairingError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(err),
    })?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| PairingError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let temp = temp_path(path);
    let write = (|| -> Result<(), PairingError> {
        let mut handle = create_private(&temp).map_err(|source| PairingError::Io {
            path: temp.clone(),
            source,
        })?;
        handle
            .write_all(json.as_bytes())
            .map_err(|source| PairingError::Io {
                path: temp.clone(),
                source,
            })?;
        handle.flush().map_err(|source| PairingError::Io {
            path: temp.clone(),
            source,
        })?;
        handle.sync_all().map_err(|source| PairingError::Io {
            path: temp.clone(),
            source,
        })?;
        force_private_mode(&handle, &temp)?;
        drop(handle);
        fs::rename(&temp, path).map_err(|source| PairingError::Io {
            path: path.to_path_buf(),
            source,
        })
    })();
    if write.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write
}

fn create_private(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(FILE_MODE);
    }
    options.open(path)
}

/// Force `0600` even if the process umask would have cleared bits at creation.
fn force_private_mode(handle: &File, path: &Path) -> Result<(), PairingError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        handle
            .set_permissions(fs::Permissions::from_mode(FILE_MODE))
            .map_err(|source| PairingError::Io {
                path: path.to_path_buf(),
                source,
            })?;
    }
    #[cfg(not(unix))]
    {
        let _ = (handle, path);
    }
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pairing".to_string());
    path.with_file_name(format!(".{stem}_{}-{counter}.tmp", std::process::id()))
}

/// SHA-256 (FIPS 180-4).
///
/// Implemented here rather than pulled in as a dependency: the crate's platform adapters already
/// build HMAC-SHA-256 on top of [`sha256::digest`], and the vector tests below pin it to the
/// standard.
pub(crate) mod sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    const H0: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    /// Hex digest of `data`.
    pub(crate) fn hex_digest(data: &[u8]) -> String {
        digest(data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Raw 32-byte digest of `data`.
    pub(crate) fn digest(data: &[u8]) -> [u8; 32] {
        let mut message = data.to_vec();
        let bit_len = (data.len() as u64).wrapping_mul(8);
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&bit_len.to_be_bytes());

        let mut state = H0;
        for block in message.as_chunks::<64>().0 {
            let mut schedule = [0u32; 64];
            for (word, chunk) in schedule.iter_mut().zip(block.as_chunks::<4>().0) {
                *word = u32::from_be_bytes(*chunk);
            }
            for idx in 16..64 {
                let s0 = schedule[idx - 15].rotate_right(7)
                    ^ schedule[idx - 15].rotate_right(18)
                    ^ (schedule[idx - 15] >> 3);
                let s1 = schedule[idx - 2].rotate_right(17)
                    ^ schedule[idx - 2].rotate_right(19)
                    ^ (schedule[idx - 2] >> 10);
                schedule[idx] = schedule[idx - 16]
                    .wrapping_add(s0)
                    .wrapping_add(schedule[idx - 7])
                    .wrapping_add(s1);
            }

            let mut v = state;
            for idx in 0..64 {
                let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
                let choose = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
                let t1 = v[7]
                    .wrapping_add(s1)
                    .wrapping_add(choose)
                    .wrapping_add(K[idx])
                    .wrapping_add(schedule[idx]);
                let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
                let majority = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
                let t2 = s0.wrapping_add(majority);
                v[7] = v[6];
                v[6] = v[5];
                v[5] = v[4];
                v[4] = v[3].wrapping_add(t1);
                v[3] = v[2];
                v[2] = v[1];
                v[1] = v[0];
                v[0] = t1.wrapping_add(t2);
            }
            for idx in 0..8 {
                state[idx] = state[idx].wrapping_add(v[idx]);
            }
        }

        let mut out = [0u8; 32];
        for (idx, word) in state.iter().enumerate() {
            out[idx * 4..idx * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, PairingStore) {
        let dir = TempDir::new().expect("temp dir");
        let store = PairingStore::open(dir.path()).expect("open store");
        (dir, store)
    }

    /// Rewrite a store file the way an external writer (or the passage of time) would.
    fn mutate(path: PathBuf, op: impl FnOnce(&mut Map<String, Value>)) {
        let mut data = load_json(&path);
        op(&mut data);
        save_json(&path, &data).expect("rewrite store");
    }

    /// Approve a code end to end and return the issued code.
    fn issue(store: &PairingStore, platform: &str, user_id: &str, user_name: &str) -> String {
        store
            .generate_code(platform, user_id, user_name)
            .expect("generate")
            .expect("code issued")
    }

    #[test]
    fn sha256_matches_fips_vectors() {
        assert_eq!(
            sha256::hex_digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256::hex_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256::hex_digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(sha256::hex_digest(&vec![b'a'; 1024 * 1024]).len(), 64);
    }

    #[test]
    fn issue_and_verify_happy_path() {
        let (_dir, store) = store();
        let code = issue(&store, "telegram", "42", "Ada");
        assert_eq!(code.len(), CODE_LENGTH);
        assert!(code.chars().all(|c| ALPHABET.contains(c)));

        // Nothing is approved until the operator says so.
        assert!(!store.is_approved("telegram", "42"));
        let pending = store.list_pending(Some("telegram")).expect("list pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].user_id, "42");
        assert_eq!(pending[0].user_name, "Ada");
        assert_eq!(pending[0].request_id.len(), REQUEST_ID_LENGTH);
        assert!(PairingStore::looks_like_request_id(&pending[0].request_id));
        assert!(!PairingStore::looks_like_request_id(&code));

        // Lowercase and spaced input still matches: chat UIs add visual spacing.
        let spaced = format!(
            " {} ",
            code.to_lowercase()
                .chars()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let approval = store
            .approve_code("telegram", &spaced)
            .expect("approve")
            .expect("valid code");
        assert_eq!(
            approval,
            Approval {
                user_id: "42".to_string(),
                user_name: "Ada".to_string()
            }
        );
        assert!(store.is_approved("telegram", "42"));
        assert!(store.list_pending(None).expect("pending").is_empty());
        assert_eq!(store.list_approved(None).expect("approved").len(), 1);
        // The code is single use.
        assert!(
            store
                .approve_code("telegram", &code)
                .expect("again")
                .is_none()
        );
    }

    #[test]
    fn approve_request_admits_by_id_and_ignores_lockout() {
        let (_dir, store) = store();
        issue(&store, "slack", "u1", "Grace");
        let request_id = store.list_pending(None).expect("pending")[0]
            .request_id
            .clone();
        let approval = store
            .approve_request("slack", &request_id.to_uppercase())
            .expect("approve request")
            .expect("row exists");
        assert_eq!(approval.user_name, "Grace");
        assert!(store.is_approved("slack", "u1"));
        // A stale id is "that row expired", not an attack.
        assert!(
            store
                .approve_request("slack", &request_id)
                .expect("stale id")
                .is_none()
        );
        assert!(!store.is_locked_out("slack"));
        assert!(
            store
                .approve_request("slack", "  ")
                .expect("empty id")
                .is_none()
        );
    }

    #[test]
    fn wrong_code_locks_out_after_max_failures() {
        let (_dir, store) = store();
        let code = issue(&store, "discord", "7", "Linus");

        for attempt in 0..MAX_FAILED_ATTEMPTS {
            assert!(!store.is_locked_out("discord"), "attempt {attempt}");
            assert!(
                store
                    .approve_code("discord", "ZZZZZZZZ")
                    .expect("wrong code")
                    .is_none()
            );
        }
        assert!(store.is_locked_out("discord"));
        // The valid code is now refused: the lockout is checked before the lookup.
        assert!(
            store
                .approve_code("discord", &code)
                .expect("locked out")
                .is_none()
        );
        assert!(!store.is_approved("discord", "7"));
        // Lockout also blocks new codes.
        assert!(
            store
                .generate_code("discord", "8", "")
                .expect("locked out")
                .is_none()
        );
        // Another platform is untouched.
        assert!(!store.is_locked_out("telegram"));
    }

    #[test]
    fn failed_attempts_reset_after_a_success() {
        let (_dir, store) = store();
        let code = issue(&store, "matrix", "u1", "");
        for _ in 0..(MAX_FAILED_ATTEMPTS - 1) {
            store.approve_code("matrix", "AAAAAAAA").expect("wrong");
        }
        store
            .approve_code("matrix", &code)
            .expect("approve")
            .expect("valid code");
        let limits = load_json(&store.rate_limit_path());
        assert_eq!(
            number_at(&limits, "_failures:matrix").unwrap_or(-1.0),
            0.0,
            "a success clears the consecutive-failure streak"
        );
    }

    #[test]
    fn code_expires_after_ttl() {
        let (_dir, store) = store();
        let code = issue(&store, "signal", "u1", "");
        let path = store.pending_path("signal");
        // Age the request past the TTL instead of sleeping.
        let created = now_secs() - (CODE_TTL_SECONDS + 5.0);
        mutate(path.clone(), |pending| {
            for (_, entry) in pending.iter_mut() {
                entry["created_at"] = Value::from(created);
            }
        });

        assert!(
            store
                .approve_code("signal", &code)
                .expect("expired")
                .is_none()
        );
        assert!(store.list_pending(None).expect("pending").is_empty());
        assert!(
            load_json(&path).is_empty(),
            "expired entry pruned from the file"
        );
        // Expiry alone does not spend the failure budget: the entry was gone before the lookup,
        // so the miss still counted as one failed attempt but never lockout material.
        assert!(!store.is_locked_out("signal"));
    }

    #[test]
    fn legacy_entries_without_salt_are_skipped() {
        let (_dir, store) = store();
        let path = store.pending_path("telegram");
        let mut legacy = Map::new();
        legacy.insert("hash".to_string(), Value::String("deadbeef".to_string()));
        legacy.insert("user_id".to_string(), Value::String("u1".to_string()));
        legacy.insert("created_at".to_string(), Value::from(now_secs()));
        let mut pending = Map::new();
        pending.insert("legacy".to_string(), Value::Object(legacy));
        save_json(&path, &pending).expect("write legacy");

        assert!(
            store
                .approve_code("telegram", "ABCDEFGH")
                .expect("skips legacy")
                .is_none()
        );
        // Listing still reports the row, with no usable request id.
        let listed = store.list_pending(None).expect("pending");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].request_id, "");
        // A malformed timestamp makes it expired.
        mutate(path, |pending| {
            for (_, entry) in pending.iter_mut() {
                entry["created_at"] = Value::String("yesterday".to_string());
            }
        });
        assert!(store.list_pending(None).expect("pending").is_empty());
    }

    #[test]
    fn max_pending_per_platform_is_enforced() {
        let (_dir, store) = store();
        for user in ["a", "b", "c"] {
            assert!(
                store
                    .generate_code("telegram", user, "")
                    .expect("generate")
                    .is_some()
            );
        }
        assert!(
            store
                .generate_code("telegram", "d", "")
                .expect("fourth")
                .is_none()
        );
        assert_eq!(store.list_pending(None).expect("pending").len(), 3);
        // A different platform has its own budget.
        assert!(
            store
                .generate_code("slack", "d", "")
                .expect("other platform")
                .is_some()
        );
    }

    #[test]
    fn rate_limit_is_per_sender_and_alias_aware() {
        let (_dir, store) = store();
        assert!(
            store
                .generate_code("telegram", "u1", "")
                .expect("first")
                .is_some()
        );
        // Same sender again, even with the queue cleared, is rate limited.
        store.clear_pending(Some("telegram")).expect("clear");
        assert!(
            store
                .generate_code("telegram", "u1", "")
                .expect("second")
                .is_none()
        );
        // A different sender is unaffected.
        assert!(
            store
                .generate_code("telegram", "u2", "")
                .expect("other sender")
                .is_some()
        );
        // WhatsApp aliases share one bucket: the jid and the bare phone are the same sender.
        assert!(
            store
                .generate_code("whatsapp", "+15551234567", "")
                .expect("wa phone")
                .is_some()
        );
        assert!(
            store
                .generate_code("whatsapp", "15551234567@s.whatsapp.net", "")
                .expect("wa jid")
                .is_none()
        );
        // The window is bounded: age the stamp past it.
        let path = store.rate_limit_path();
        let stale = now_secs() - (RATE_LIMIT_SECONDS + 5.0);
        mutate(path, |limits| {
            for (key, value) in limits.iter_mut() {
                if !key.starts_with('_') {
                    *value = Value::from(stale);
                }
            }
        });
        assert!(
            store
                .generate_code("telegram", "u1", "")
                .expect("after window")
                .is_some()
        );
    }

    #[test]
    fn decline_dedupe_and_pruning() {
        let (_dir, store) = store();
        assert!(!store.has_recent_decline("whatsapp", "+15551234567"));

        store
            .record_decline("whatsapp", "+15551234567")
            .expect("decline");
        assert!(store.has_recent_decline("whatsapp", "15551234567"));
        assert!(store.has_recent_decline("whatsapp", "15551234567@s.whatsapp.net"));
        assert!(store.has_recent_decline("whatsapp", "+15551234567"));
        // Another platform and another sender are unaffected.
        assert!(!store.has_recent_decline("telegram", "15551234567"));
        assert!(!store.has_recent_decline("whatsapp", "15559999999"));

        // Expired stamps are pruned on the next write, and stop matching.
        let path = store.decline_path();
        let stale = now_secs() - (DECLINE_DEDUPE_SECONDS + 5.0);
        mutate(path.clone(), |stamps| {
            for (_, value) in stamps.iter_mut() {
                *value = Value::from(stale);
            }
        });
        assert!(!store.has_recent_decline("whatsapp", "+15551234567"));
        store.record_decline("whatsapp", "u2").expect("decline");
        let stamps = load_json(&path);
        assert!(
            !stamps.contains_key("whatsapp:15551234567"),
            "expired stamps pruned on write"
        );
        assert_eq!(
            stamps.len(),
            3,
            "the fresh sender stamped under all three of its alias forms"
        );
    }

    #[test]
    fn approved_identifier_survives_every_whatsapp_alias_form() {
        let (_dir, store) = store();
        let code = issue(&store, "whatsapp", "+15551234567", "Ada");
        store
            .approve_code("whatsapp", &code)
            .expect("approve")
            .expect("valid code");

        for form in [
            "+15551234567",
            "15551234567",
            "15551234567@s.whatsapp.net",
            "15551234567@lid",
            "15551234567:12@s.whatsapp.net",
            "  +15551234567  ",
        ] {
            assert!(store.is_approved("whatsapp", form), "should match {form:?}");
        }
        // The stored key is the normalized bare phone, not the raw form.
        let approved = store.list_approved(Some("whatsapp")).expect("approved");
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].user_id, "15551234567");
        assert_eq!(approved[0].user_name, "Ada");
        assert!(approved[0].approved_at.is_some());

        // Revoking by a different alias form still removes the principal.
        assert!(
            store
                .revoke("whatsapp", "15551234567@s.whatsapp.net")
                .expect("revoke")
        );
        assert!(!store.is_approved("whatsapp", "+15551234567"));
        assert!(
            !store
                .revoke("whatsapp", "+15551234567")
                .expect("second revoke")
        );

        // whatsapp_cloud shares the identity rules.
        let code = issue(&store, "whatsapp_cloud", "15550000000@s.whatsapp.net", "");
        store
            .approve_code("whatsapp_cloud", &code)
            .expect("approve cloud")
            .expect("valid");
        assert!(store.is_approved("whatsapp_cloud", "+15550000000"));
    }

    #[test]
    fn approval_replaces_an_alias_equivalent_existing_entry() {
        let (_dir, store) = store();
        let first = issue(&store, "whatsapp", "15551112222", "Old Name");
        store
            .approve_code("whatsapp", &first)
            .expect("approve")
            .expect("valid");
        // Age the rate limit so the same sender can request again.
        mutate(store.rate_limit_path(), |limits| {
            for (key, value) in limits.iter_mut() {
                if !key.starts_with('_') {
                    *value = Value::from(0.0);
                }
            }
        });
        let second = issue(&store, "whatsapp", "+15551112222", "New Name");
        store
            .approve_code("whatsapp", &second)
            .expect("approve")
            .expect("valid");

        let approved = store.list_approved(Some("whatsapp")).expect("approved");
        assert_eq!(
            approved.len(),
            1,
            "alias-equivalent entry replaced, not duplicated"
        );
        assert_eq!(approved[0].user_name, "New Name");
    }

    #[test]
    fn list_and_clear_across_platforms() {
        let (_dir, store) = store();
        issue(&store, "telegram", "t1", "T");
        issue(&store, "slack", "s1", "S");

        let all = store.list_pending(None).expect("pending");
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|row| row.platform == "telegram"));
        assert!(all.iter().any(|row| row.platform == "slack"));

        assert_eq!(store.clear_pending(Some("telegram")).expect("clear"), 1);
        assert_eq!(store.list_pending(None).expect("pending").len(), 1);
        assert_eq!(store.clear_pending(None).expect("clear all"), 1);
        assert!(store.list_pending(None).expect("pending").is_empty());
        assert_eq!(store.clear_pending(None).expect("nothing left"), 0);
    }

    #[test]
    fn store_files_are_owner_only_on_unix() {
        let (_dir, store) = store();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode_of = |path: &Path| {
                fs::metadata(path)
                    .unwrap_or_else(|_| panic!("stat {}", path.display()))
                    .permissions()
                    .mode()
                    & 0o777
            };

            let code = issue(&store, "telegram", "u1", "");
            store.approve_code("telegram", &code).expect("approve");
            store.record_decline("telegram", "u1").expect("decline");

            for path in [
                store.pending_path("telegram"),
                store.approved_path("telegram"),
                store.rate_limit_path(),
                store.decline_path(),
                store.dir().join(LOCK_FILE),
            ] {
                assert_eq!(mode_of(&path), FILE_MODE, "{} must be 0600", path.display());
            }
        }
    }

    #[test]
    fn code_and_salt_are_hashed_never_stored_plaintext() {
        let (_dir, store) = store();
        let code = issue(&store, "telegram", "u1", "");
        let raw = fs::read_to_string(store.pending_path("telegram")).expect("read store");
        assert!(!raw.contains(&code), "the code leaked into the store");

        let pending = load_json(&store.pending_path("telegram"));
        let (_, entry) = pending.iter().next().expect("one entry");
        let salt = hex_decode(entry["salt"].as_str().expect("salt")).expect("hex salt");
        assert_eq!(salt.len(), 16);
        assert_eq!(
            entry["hash"].as_str().expect("hash"),
            hash_code(&code, &salt),
            "hash is sha256(salt ‖ code)"
        );
        // A different code never matches the stored hash.
        assert_ne!(
            entry["hash"].as_str().expect("hash"),
            hash_code("ZZZZZZZZ", &salt)
        );
    }

    #[test]
    fn corrupt_or_missing_files_degrade_to_empty() {
        let (dir, store) = store();
        fs::write(store.pending_path("telegram"), b"{ not json").expect("write garbage");
        assert!(store.list_pending(None).expect("pending").is_empty());
        assert!(!store.is_approved("telegram", "u1"));
        // A non-object JSON document is not a store either.
        fs::write(store.approved_path("telegram"), b"[1,2,3]").expect("write array");
        assert!(store.list_approved(None).expect("approved").is_empty());
        // A BOM does not break the read.
        fs::write(
            dir.path().join("telegram-approved.json"),
            b"\xef\xbb\xbf{\"u1\": {\"user_name\": \"Ada\"}}",
        )
        .expect("write bom");
        assert!(store.is_approved("telegram", "u1"));
    }

    #[test]
    fn empty_or_traversing_platform_is_rejected() {
        let (_dir, store) = store();
        assert!(matches!(
            store.generate_code("  ", "u1", ""),
            Err(PairingError::InvalidPlatform(_))
        ));
        assert!(matches!(
            store.generate_code("../escape", "u1", ""),
            Err(PairingError::InvalidPlatform(_))
        ));
        assert!(!store.is_approved("", "u1"));
        assert!(!store.has_recent_decline("", "u1"));
    }

    #[test]
    fn concurrent_issue_does_not_corrupt_the_store() {
        use std::sync::Arc;

        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(PairingStore::open(dir.path()).expect("open store"));

        let handle = |users: &'static [&'static str]| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                users
                    .iter()
                    .filter_map(|user| store.generate_code("telegram", user, "").expect("generate"))
                    .count()
            })
        };
        let left = handle(&["a1", "a2", "a3"]);
        let right = handle(&["b1", "b2", "b3"]);
        let issued = left.join().expect("left thread") + right.join().expect("right thread");

        // The pending budget is three, and the file lock makes the check and the write one step:
        // without it both threads would read an empty store and lose one update.
        assert_eq!(issued, MAX_PENDING_PER_PLATFORM);
        assert_eq!(
            store.list_pending(Some("telegram")).expect("pending").len(),
            MAX_PENDING_PER_PLATFORM
        );
        assert_eq!(
            load_json(&store.pending_path("telegram")).len(),
            MAX_PENDING_PER_PLATFORM
        );
        // Every surviving entry still names a distinct user and hashes a real code.
        let users: HashSet<String> = load_json(&store.pending_path("telegram"))
            .values()
            .map(|entry| string_at(entry, "user_id"))
            .collect();
        assert_eq!(users.len(), MAX_PENDING_PER_PLATFORM);
    }

    #[test]
    fn lock_file_is_released_after_each_operation() {
        let (_dir, store) = store();
        // Two sequential locked operations in the same thread must not self-deadlock.
        issue(&store, "telegram", "u1", "");
        store.clear_pending(None).expect("clear");
        store.record_decline("telegram", "u1").expect("decline");
        store.cleanup_expired("telegram").expect("cleanup");
    }
}
