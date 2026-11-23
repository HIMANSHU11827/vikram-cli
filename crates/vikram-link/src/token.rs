//! The token store: who may reach this runtime, and how long for.
//!
//! A token is a name, a tier and the SHA-256 of a secret that is shown once and never stored. The
//! file is `$VIKRAM_HOME/link/tokens.json`, mode 0600, written whole and atomically: a half-written
//! token file would lock every linked device out at once.

use crate::tier::Tier;
use crate::time::{humanize_ms, now_ms, parse_duration};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A temporary token may not outlive this unless the operator raises it.
pub const DEFAULT_MAX_TEMP_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

/// One stored token. `salt` and `hash` are private on purpose: a summary is the only shape the
/// server and the CLI can see, so no route can serialize a hash by accident.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenRecord {
    pub name: String,
    pub tier: Tier,
    salt: String,
    hash: String,
    pub created_ms: u64,
    /// `None` is a permanent token: it has no expiry field in the file at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<u64>,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct State {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(default)]
    tokens: Vec<TokenRecord>,
}

fn state_version() -> u32 {
    1
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            owner: None,
            tokens: Vec::new(),
        }
    }
}

/// How long a token lives. The two are exclusive by construction: there is no value that means
/// "both", and the CLI refuses to guess when neither is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lifetime {
    /// A human duration: `90s`, `30m`, `2h`, `7d`.
    For(String),
    Permanent,
}

/// A mint request, already parsed out of the flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddRequest {
    pub name: String,
    pub tier: String,
    pub lifetime: Lifetime,
    /// Make this token the owner. Only meaningful for `admin`.
    pub owner: bool,
}

/// What `add` returns. The secret is here and nowhere else, ever.
#[derive(Clone, Debug)]
pub struct Minted {
    pub name: String,
    pub tier: Tier,
    pub secret: String,
    pub expires_ms: Option<u64>,
}

/// One row of `vikram link token list` and of `GET /link/tokens`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenSummary {
    pub name: String,
    pub tier: Tier,
    pub created_ms: u64,
    pub expires_ms: Option<u64>,
    pub last_seen_ms: Option<u64>,
    pub revoked: bool,
    pub owner: bool,
}

impl TokenSummary {
    pub fn state(&self, now: u64) -> &'static str {
        if self.revoked {
            "revoked"
        } else if self.expires_ms.is_some_and(|expires| expires <= now) {
            "expired"
        } else {
            "active"
        }
    }

    pub fn permanent(&self) -> bool {
        self.expires_ms.is_none()
    }
}

/// A verified token, as the request boundary needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub name: String,
    pub tier: Tier,
    pub owner: bool,
}

/// The answer to "is this secret good?".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Valid(Verified),
    Unknown,
    Revoked { name: String },
    Expired { name: String, expires_ms: u64 },
}

pub struct TokenStore {
    path: PathBuf,
    state: State,
}

impl TokenStore {
    /// Open the store, creating nothing. A missing file is an empty store; an unreadable one is an
    /// error, because silently starting empty would mint over tokens that still exist on disk.
    pub fn open(path: impl Into<PathBuf>) -> Result<TokenStore, String> {
        let path = path.into();
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                format!(
                    "cannot read the token store at {}: {e} — fix or remove it before serving",
                    path.display()
                )
            })?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(err) => {
                return Err(format!(
                    "cannot read the token store at {}: {err}",
                    path.display()
                ));
            }
        };
        Ok(TokenStore { path, state })
    }

    /// `$VIKRAM_HOME/link/tokens.json`.
    pub fn at_home() -> Result<TokenStore, String> {
        TokenStore::open(link_dir().join("tokens.json"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn owner(&self) -> Option<&str> {
        self.state.owner.as_deref()
    }

    pub fn tokens(&self) -> &[TokenRecord] {
        &self.state.tokens
    }

    pub fn summaries(&self, now: u64) -> Vec<TokenSummary> {
        self.state
            .tokens
            .iter()
            .map(|record| TokenSummary {
                name: record.name.clone(),
                tier: record.tier.clone(),
                created_ms: record.created_ms,
                expires_ms: record.expires_ms,
                last_seen_ms: record.last_seen_ms,
                revoked: record.revoked,
                owner: self.state.owner.as_deref() == Some(record.name.as_str()),
            })
            .collect::<Vec<_>>()
            .tap_sort(now)
    }

    /// Mint a token and write the store. The secret exists in the returned value and nowhere else.
    pub fn add(
        &mut self,
        request: AddRequest,
        now: u64,
        max_temp_ms: u64,
    ) -> Result<Minted, String> {
        let tier = Tier::parse(&request.tier).map_err(|e| e.to_string())?;
        let name = request.name.trim().to_owned();
        if name.is_empty() {
            return Err("a token needs a name".into());
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!(
                "`{name}` is not a token name; use letters, digits, `-`, `_` or `.`"
            ));
        }
        if self
            .state
            .tokens
            .iter()
            .any(|record| record.name == name && !record.revoked)
        {
            return Err(format!(
                "a token named `{name}` already exists; revoke it first with `vikram link token rm {name}`"
            ));
        }
        let expires_ms = match &request.lifetime {
            Lifetime::Permanent => None,
            Lifetime::For(raw) => {
                let span = parse_duration(raw)?;
                if span > max_temp_ms {
                    return Err(format!(
                        "`--for {raw}` is longer than the `link.max_temp` ceiling of {} — raise `link.max_temp` in $VIKRAM_HOME/config.json to allow it, or use --permanent",
                        humanize_ms(max_temp_ms)
                    ));
                }
                Some(now + span)
            }
        };
        if request.owner && !matches!(tier, Tier::Admin) {
            return Err(format!(
                "only an `admin` token can own the runtime; `{}` cannot",
                tier
            ));
        }

        let secret = new_secret()?;
        let salt = new_salt()?;
        let hash = digest(&salt, &secret);
        // A revoked token with the same name is replaced: the operator is re-issuing, not adding.
        self.state.tokens.retain(|record| record.name != name);
        self.state.tokens.push(TokenRecord {
            name: name.clone(),
            tier: tier.clone(),
            salt,
            hash,
            created_ms: now,
            expires_ms,
            last_seen_ms: None,
            revoked: false,
        });
        // The first admin becomes the owner; an explicit `--owner` transfers it.
        if matches!(tier, Tier::Admin) && (request.owner || self.state.owner.is_none()) {
            self.state.owner = Some(name.clone());
        }
        self.save()?;
        Ok(Minted {
            name,
            tier,
            secret,
            expires_ms,
        })
    }

    /// Revoke by name. Returns whether a token was there to revoke.
    pub fn revoke(&mut self, name: &str) -> Result<bool, String> {
        let Some(record) = self
            .state
            .tokens
            .iter_mut()
            .find(|record| record.name == name)
        else {
            return Ok(false);
        };
        record.revoked = true;
        if self.state.owner.as_deref() == Some(name) {
            self.state.owner = None;
        }
        self.save()?;
        Ok(true)
    }

    /// Drop every token whose expiry has passed. Permanent and revoked tokens stay: the first has
    /// no expiry, the second is the record of a decision.
    pub fn remove_expired(&mut self, now: u64) -> Result<Vec<String>, String> {
        let before = self.state.tokens.len();
        let mut removed = Vec::new();
        self.state.tokens.retain(|record| {
            let expired = record.expires_ms.is_some_and(|expires| expires <= now);
            if expired {
                removed.push(record.name.clone());
            }
            !expired
        });
        if self.state.tokens.len() != before {
            self.save()?;
        }
        Ok(removed)
    }

    /// Is this secret good, and for what? Every record is compared, with no early exit, so the
    /// work does not depend on which token matched.
    pub fn verify(&mut self, secret: &str, now: u64) -> Verdict {
        let mut matched: Option<usize> = None;
        for (index, record) in self.state.tokens.iter().enumerate() {
            let candidate = digest(&record.salt, secret);
            if constant_time_eq(candidate.as_bytes(), record.hash.as_bytes()) && matched.is_none() {
                matched = Some(index);
            }
        }
        let Some(index) = matched else {
            return Verdict::Unknown;
        };
        let (name, revoked, expires) = {
            let record = &self.state.tokens[index];
            (record.name.clone(), record.revoked, record.expires_ms)
        };
        if revoked {
            return Verdict::Revoked { name };
        }
        if let Some(expires_ms) = expires
            && expires_ms <= now
        {
            return Verdict::Expired { name, expires_ms };
        }
        self.state.tokens[index].last_seen_ms = Some(now);
        // A failed write must not fail the request: the token is valid, and the next write will
        // carry the timestamp.
        let _ = self.save();
        let record = &self.state.tokens[index];
        Verdict::Valid(Verified {
            name: record.name.clone(),
            tier: record.tier.clone(),
            owner: self.state.owner.as_deref() == Some(record.name.as_str()),
        })
    }

    fn save(&self) -> Result<(), String> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", self.path.display()))?;
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        restrict(dir, 0o700);
        let bytes = serde_json::to_vec_pretty(&self.state)
            .map_err(|e| format!("cannot serialize the token store: {e}"))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        restrict(&tmp, 0o600);
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("cannot replace {}: {e}", self.path.display()))?;
        Ok(())
    }
}

/// `$VIKRAM_HOME/link`.
pub fn link_dir() -> PathBuf {
    vikram_runtime::config::vikram_home().join("link")
}

/// The temporary-token ceiling, `link.max_temp`, default 30 days. A string (`30d`) or a number of
/// seconds is accepted; anything else is an error naming the key, never a silent default.
pub fn max_temp_ms() -> Result<u64, String> {
    let path = vikram_runtime::config::config_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(DEFAULT_MAX_TEMP_MS);
    };
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(DEFAULT_MAX_TEMP_MS);
    };
    let Some(value) = root.get("link").and_then(|link| link.get("max_temp")) else {
        return Ok(DEFAULT_MAX_TEMP_MS);
    };
    match value {
        serde_json::Value::String(text) => parse_duration(text).map_err(|e| {
            format!(
                "`link.max_temp` in {} is `{text}`: {e}",
                path.display()
            )
        }),
        serde_json::Value::Number(number) => number
            .as_u64()
            .map(|seconds| seconds * 1_000)
            .filter(|ms| *ms > 0)
            .ok_or_else(|| {
                format!(
                    "`link.max_temp` in {} must be a positive number of seconds or a duration like `30d`",
                    path.display()
                )
            }),
        _ => Err(format!(
            "`link.max_temp` in {} must be a duration string like `30d` or a number of seconds",
            path.display()
        )),
    }
}

fn new_secret() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| format!("the system entropy source is unavailable: {e}"))?;
    Ok(format!("vlink_{}", hex(&bytes)))
}

fn new_salt() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| format!("the system entropy source is unavailable: {e}"))?;
    Ok(hex(&bytes))
}

fn digest(salt: &str, secret: &str) -> String {
    let mut material = Vec::with_capacity(salt.len() + secret.len());
    material.extend_from_slice(salt.as_bytes());
    material.extend_from_slice(secret.as_bytes());
    vikram_runtime::plugins::sha256_hex(&material)
}

/// Compare without an early exit: the loop's work must not say where the first difference was.
pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) {}

/// Sort summaries with the owner first, then the live ones, then by name. `add` order is not an
/// order an operator can scan.
trait TapSort {
    fn tap_sort(self, now: u64) -> Self;
}

impl TapSort for Vec<TokenSummary> {
    fn tap_sort(mut self, now: u64) -> Self {
        self.sort_by(|a, b| {
            let rank = |row: &TokenSummary| match row.state(now) {
                "active" if row.owner => 0,
                "active" => 1,
                "expired" => 2,
                _ => 3,
            };
            rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
        });
        self
    }
}

/// A timestamp for a token file, or "never".
pub fn seen_label(ms: Option<u64>) -> String {
    match ms {
        Some(ms) => crate::time::iso8601(ms),
        None => "never".to_owned(),
    }
}

/// The `now` a caller should pass in. Kept here so a test can inject a clock through the same
/// functions the server uses.
pub fn clock() -> u64 {
    now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, TokenStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::open(dir.path().join("link/tokens.json")).unwrap();
        (dir, store)
    }

    fn add(store: &mut TokenStore, name: &str, tier: &str, lifetime: Lifetime) -> Minted {
        store
            .add(
                AddRequest {
                    name: name.into(),
                    tier: tier.into(),
                    lifetime,
                    owner: false,
                },
                1_000,
                DEFAULT_MAX_TEMP_MS,
            )
            .unwrap()
    }

    #[test]
    fn a_secret_verifies_and_is_never_written_to_the_file() {
        let (dir, mut store) = store();
        let minted = add(&mut store, "laptop", "admin", Lifetime::Permanent);
        assert!(minted.secret.starts_with("vlink_"));
        let file = std::fs::read_to_string(dir.path().join("link/tokens.json")).unwrap();
        assert!(
            !file.contains(&minted.secret),
            "the secret must not be on disk"
        );
        assert!(file.contains("hash"));
        assert!(matches!(
            store.verify(&minted.secret, 2_000),
            Verdict::Valid(_)
        ));
        assert_eq!(store.verify("vlink_wrong", 2_000), Verdict::Unknown);
    }

    #[test]
    fn a_temporary_token_expires_and_a_permanent_one_has_no_expiry_field() {
        let (dir, mut store) = store();
        let temp = store
            .add(
                AddRequest {
                    name: "temp".into(),
                    tier: "read".into(),
                    lifetime: Lifetime::For("30m".into()),
                    owner: false,
                },
                1_000,
                DEFAULT_MAX_TEMP_MS,
            )
            .unwrap();
        assert_eq!(temp.expires_ms, Some(1_000 + 30 * 60 * 1_000));
        let perm = add(&mut store, "perm", "read", Lifetime::Permanent);
        assert_eq!(perm.expires_ms, None);
        let file = std::fs::read_to_string(dir.path().join("link/tokens.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&file).unwrap();
        let rows = json["tokens"].as_array().unwrap();
        assert!(
            rows[1].get("expires_ms").is_none(),
            "permanent carries no expiry"
        );
        assert!(rows[0].get("expires_ms").is_some());

        assert!(matches!(
            store.verify(&temp.secret, 1_500),
            Verdict::Valid(_)
        ));
        assert_eq!(
            store.verify(&temp.secret, temp.expires_ms.unwrap()),
            Verdict::Expired {
                name: "temp".into(),
                expires_ms: temp.expires_ms.unwrap()
            }
        );
    }

    #[test]
    fn the_first_admin_owns_and_a_revoked_token_cannot_be_used() {
        let (_dir, mut store) = store();
        let first = add(&mut store, "laptop", "admin", Lifetime::Permanent);
        assert_eq!(store.owner(), Some("laptop"));
        let second = add(&mut store, "desktop", "admin", Lifetime::Permanent);
        assert_eq!(store.owner(), Some("laptop"), "one owner at a time");
        let Verdict::Valid(verified) = store.verify(&first.secret, 2_000) else {
            panic!("the owner token must verify");
        };
        assert!(verified.owner);
        let Verdict::Valid(other) = store.verify(&second.secret, 2_000) else {
            panic!("the second admin must verify");
        };
        assert!(!other.owner);

        assert!(store.revoke("laptop").unwrap());
        assert_eq!(
            store.verify(&first.secret, 2_000),
            Verdict::Revoked {
                name: "laptop".into()
            }
        );
        assert_eq!(store.owner(), None);
    }

    #[test]
    fn only_an_admin_may_own() {
        let (_dir, mut store) = store();
        let err = store
            .add(
                AddRequest {
                    name: "bot".into(),
                    tier: "bots".into(),
                    lifetime: Lifetime::Permanent,
                    owner: true,
                },
                1_000,
                DEFAULT_MAX_TEMP_MS,
            )
            .unwrap_err();
        assert!(err.contains("admin"), "{err}");
    }

    #[test]
    fn a_duplicate_live_name_is_refused_but_a_revoked_one_is_reissued() {
        let (_dir, mut store) = store();
        add(&mut store, "laptop", "read", Lifetime::Permanent);
        let err = store
            .add(
                AddRequest {
                    name: "laptop".into(),
                    tier: "read".into(),
                    lifetime: Lifetime::Permanent,
                    owner: false,
                },
                2_000,
                DEFAULT_MAX_TEMP_MS,
            )
            .unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        store.revoke("laptop").unwrap();
        add(&mut store, "laptop", "read", Lifetime::Permanent);
    }

    #[test]
    fn the_ceiling_refuses_a_year_and_names_it() {
        let (_dir, mut store) = store();
        let err = store
            .add(
                AddRequest {
                    name: "forever".into(),
                    tier: "read".into(),
                    lifetime: Lifetime::For("365d".into()),
                    owner: false,
                },
                1_000,
                DEFAULT_MAX_TEMP_MS,
            )
            .unwrap_err();
        assert!(err.contains("link.max_temp"), "{err}");
        assert!(err.contains("30d"), "the ceiling is named: {err}");
    }

    #[test]
    fn pruning_removes_only_expired_tokens() {
        let (_dir, mut store) = store();
        store
            .add(
                AddRequest {
                    name: "old".into(),
                    tier: "read".into(),
                    lifetime: Lifetime::For("1s".into()),
                    owner: false,
                },
                1_000,
                DEFAULT_MAX_TEMP_MS,
            )
            .unwrap();
        add(&mut store, "keep", "read", Lifetime::Permanent);
        let removed = store.remove_expired(1_000 + 2_000).unwrap();
        assert_eq!(removed, vec!["old".to_owned()]);
        assert_eq!(store.tokens().len(), 1);
        assert_eq!(store.tokens()[0].name, "keep");
    }

    #[test]
    fn an_old_token_file_with_an_alias_tier_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link/tokens.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let salt = "00";
        let hash = digest(salt, "vlink_abc");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"tokens":[{{"name":"old","tier":"vikram","salt":"{salt}","hash":"{hash}","created_ms":1}}]}}"#
            ),
        )
        .unwrap();
        let store = TokenStore::open(&path).unwrap();
        assert_eq!(store.tokens()[0].tier, Tier::Main);
    }
}
