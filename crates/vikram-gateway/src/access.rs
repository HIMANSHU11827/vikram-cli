//! Access-policy / authorization (module 4 of the foundation).
//!
//! Port of `gateway/platforms/access_policy_mixin.py` and the decision core of
//! `gateway/authz_mixin.py`. Two layers:
//!
//! 1. [`AccessPolicy`] — an adapter's own intake gate (WeCom / Weixin / QQBot / WhatsApp / ...):
//!    DM pairing vs allowlist vs open vs disabled. Adapters with `enforces_own_access_policy`
//!    decide at the edge; the central layer delegates to [`OwnPolicySnapshot`] only when no env
//!    allowlist is configured.
//! 2. [`authorize_source`] — the central, default-deny union run by the gateway runner. The
//!    decision order and the deny-by-default stance mirror hermes exactly (see docs/
//!    gateway-port-plan.md for the full verbatim decision list):
//!
//!    0. local owner principals (LOCAL / Desktop / TUI) are always authorized
//!    1. webhook / `trusted_inbound` → allowed (HMAC-verified upstream)
//!    2. chat-scoped grant: upstream relay delegation, `*_GROUP_ALLOWED_CHATS` env, adapter
//!       `group_allowed_chats` extra, or an admitted bot via `allow_bots` mode `mentions|all`
//!    3. missing user_id → denied
//!    4. `allow_all` env truthy → allowed
//!    5. adapter-verified `role_authorized` → allowed
//!    6. operator-approved pairing code → allowed
//!    7. assembled allowlists (platform ∪ group-user ∪ global; forum/group only lists) →
//!       wildcard / principal-match verdict
//!    8. otherwise (no lists configured): own-policy snapshot → adapter extra allowlist →
//!       `GATEWAY_ALLOW_ALL_USERS`, else deny
//!
//! Wire notes kept faithful to hermes: truthy envs are `{true, 1, yes}`; `*` wildcard is the
//! only wildcard marker; `_GROUP_CHAT_TYPES = {group, forum, channel}` while only
//! `{group, forum}` (never `channel`) consult the group-user / group-chat lists — the channel
//! distinction is deliberate and must not blur; Telegram's legacy `-<chat-id>` values in
//! `TELEGRAM_GROUP_ALLOWED_USERS` are honored as chat IDs (deprecated path).
//!
//! Aliases: WhatsApp principals expand to their lid/phone/jid alias set before matching;
//! Buzz/Nostr entries are normalized npub→hex; SimpleX matches the numeric contact id only.
//!
//! All env reads flow through the [`EnvLookup`] trait so every decision is testable without
//! touching process env. Pairing approval is injected through [`PairingVerdict`].

use std::collections::HashSet;

use crate::event::SessionSource;

pub const OPTIN_TRUTHY: [&str; 3] = ["true", "1", "yes"];
pub const GROUP_CHAT_TYPES: [&str; 3] = ["group", "forum", "channel"];
pub const GROUP_FORUM_TYPES: [&str; 2] = ["group", "forum"];
pub const GLOBAL_ALLOWED_ENV: &str = "GATEWAY_ALLOWED_USERS";
pub const GLOBAL_ALLOW_ALL_ENV: &str = "GATEWAY_ALLOW_ALL_USERS";
/// Only Telegram is hard-wired to a group-user sender list.
pub const GROUP_USER_ENV: [(&str, &str); 1] = [("telegram", "TELEGRAM_GROUP_ALLOWED_USERS")];

/// Read-only view over the process environment (or a test map).
pub trait EnvLookup {
    fn get(&self, key: &str) -> Option<String>;
}

impl EnvLookup for std::collections::HashMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        self.get(key).cloned()
    }
}

/// Pairing-store projection needed by the authorizer (implemented by `pairing.rs`).
pub trait PairingVerdict {
    fn is_approved(&self, platform: &str, user_id: &str) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChatKind {
    #[default]
    Dm,
    Group,
    Forum,
    Channel,
    Unknown,
}

impl ChatKind {
    pub fn from_chat_type(ct: &str) -> Self {
        match ct.trim().to_ascii_lowercase().as_str() {
            "dm" => ChatKind::Dm,
            "group" => ChatKind::Group,
            "forum" => ChatKind::Forum,
            "channel" => ChatKind::Channel,
            _ => ChatKind::Unknown,
        }
    }
    pub fn is_group_family(&self) -> bool {
        matches!(self, ChatKind::Group | ChatKind::Forum | ChatKind::Channel)
    }
    pub fn is_forum_family(&self) -> bool {
        matches!(self, ChatKind::Group | ChatKind::Forum)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OwnPolicy {
    #[default]
    Pairing,
    Allowlist,
    Open,
    Disabled,
}

impl OwnPolicy {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "allowlist" => OwnPolicy::Allowlist,
            "open" => OwnPolicy::Open,
            "disabled" => OwnPolicy::Disabled,
            _ => OwnPolicy::Pairing,
        }
    }
    pub fn is_open_opt_in(raw: &str) -> bool {
        is_truthy(raw)
    }
}

pub fn is_truthy(raw: &str) -> bool {
    OPTIN_TRUTHY.contains(&raw.trim().to_ascii_lowercase().as_str())
}

/// Decode an allowlist value: JSON/YAML-ish list literal when it starts with `[`, else
/// comma-separated scalar. Strips surrounding quotes. Mirrors hermes `_coerce_allow_set`.
pub fn coerce_allow_set(raw: Option<&str>) -> HashSet<String> {
    let Some(raw) = raw else {
        return HashSet::new();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return HashSet::new();
    }
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        return inner
            .split(',')
            .map(|s| trim_quotes(s.trim()))
            .filter(|s| !s.is_empty())
            .collect();
    }
    trimmed
        .split(',')
        .map(|s| trim_quotes(s.trim()))
        .filter(|s| !s.is_empty())
        .collect()
}

fn trim_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'').to_string()
}

pub fn allows(allowed: &HashSet<String>, candidate: &str) -> bool {
    allowed.contains("*") || allowed.contains(candidate)
}

/// Expand WhatsApp principals to the full lid/phone/jid alias set (authorization-time).
pub(crate) fn whatsapp_auth_aliases(id: &str) -> HashSet<String> {
    let normalized = crate::routing::canonical_whatsapp_identifier(id);
    if normalized.is_empty() {
        return HashSet::new();
    }
    let mut out = HashSet::new();
    out.insert(normalized.clone());
    if !normalized.contains('@') {
        out.insert(format!("{normalized}@s.whatsapp.net"));
        out.insert(format!("{normalized}@lid"));
    } else {
        let local = normalized.split('@').next().unwrap_or_default().to_string();
        out.insert(local);
    }
    out
}

/// Buzz/Nostr: npub bech32 → hex (decode-only, no wasm). Entries and principals are normalized
/// the same way. Non-nostr platforms return the candidate unchanged.
fn normalize_nostr(id: &str) -> String {
    let lower = id.trim();
    const ALPHABET: &[u8] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
    const GEN: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
    fn polymod(values: &[u8]) -> u32 {
        let mut chk = 1u32;
        for &v in values {
            let top = chk >> 25;
            chk = ((chk & 0x01ffffff) << 5) ^ u32::from(v);
            for (i, g) in GEN.iter().enumerate() {
                if (top >> i) & 1 == 1 {
                    chk ^= g;
                }
            }
        }
        chk
    }
    if lower.starts_with("npub1") {
        let hrp = b"npub";
        let data = &lower.as_bytes()[5..];
        let values: Option<Vec<u8>> = data
            .iter()
            .map(|b| ALPHABET.iter().position(|a| a == b).map(|p| p as u8))
            .collect();
        if let Some(values) = values
            && values.len() >= 6
        {
            let hrp_expand: Vec<u8> = hrp
                .iter()
                .map(|c| c >> 5)
                .chain(std::iter::once(0))
                .chain(hrp.iter().map(|c| c & 31))
                .collect();
            let mut full = hrp_expand.clone();
            full.extend_from_slice(&values);
            if polymod(&full) == 1 {
                // Drop the 6-char checksum, convert 5-bit groups to bytes.
                let data = &values[..values.len() - 6];
                let mut acc = 0u32;
                let mut bits = 0u32;
                let mut out = Vec::with_capacity(32);
                for &v in data {
                    acc = (acc << 5) | u32::from(v);
                    bits += 5;
                    if bits >= 8 {
                        bits -= 8;
                        out.push(((acc >> bits) & 0xff) as u8);
                    }
                }
                if out.len() == 32 {
                    return out.iter().map(|b| format!("{b:02x}")).collect();
                }
            }
        }
    }
    lower.to_string()
}

/// Snapshot of an adapter's live intake policy (the own-policy layer). Re-reads the live
/// allowlist on every check so revocation works without a gateway restart.
#[derive(Clone, Debug, Default)]
pub struct OwnPolicySnapshot {
    pub dm_policy: OwnPolicy,
    pub group_policy: OwnPolicy,
    pub dm_allow_from: HashSet<String>,
    pub group_allow_from: HashSet<String>,
    pub open_dm_opt_in: bool,
}

/// Config-driven intake gate used by adapters with `enforces_own_access_policy`.
pub struct AccessPolicy {
    pub snapshot: OwnPolicySnapshot,
}

impl AccessPolicy {
    pub fn new(snapshot: OwnPolicySnapshot) -> Self {
        Self { snapshot }
    }
    pub fn is_dm_allowed(&self, sender_id: &str) -> bool {
        match self.snapshot.dm_policy {
            OwnPolicy::Allowlist => allows(&self.snapshot.dm_allow_from, sender_id),
            OwnPolicy::Open => self.snapshot.open_dm_opt_in,
            _ => false,
        }
    }
    pub fn is_dm_intake_allowed(&self, sender_id: &str) -> bool {
        if sender_id.is_empty() {
            return false;
        }
        match self.snapshot.dm_policy {
            OwnPolicy::Pairing => true,
            _ => self.is_dm_allowed(sender_id),
        }
    }
    pub fn is_group_allowed(&self, chat_id: &str, _sender_id: &str) -> bool {
        match self.snapshot.group_policy {
            OwnPolicy::Allowlist => allows(&self.snapshot.group_allow_from, chat_id),
            OwnPolicy::Open => true,
            _ => false,
        }
    }
}

/// What the runner knows about the platform at decision time.
#[derive(Clone, Debug, Default)]
pub struct PlatformAuthSpec {
    pub allowed_users_env: Option<String>,
    pub allow_all_env: Option<String>,
    pub group_allowed_chats_env: Option<String>,
    pub allow_bots_env: Option<String>,
    pub allowlist_suppresses_pairing: bool,
    pub enforces_own_access_policy: bool,
    pub authorization_is_upstream: bool,
    pub trusted_inbound: bool,
    pub allow_all_env_extra: Option<String>,
    /// config.extra allow_from / group_allow_from (Telegram-style adapters under multiplex).
    pub adapter_extra_allow_from: Option<Vec<String>>,
    pub adapter_extra_group_allow_from: Option<Vec<String>>,
    /// What the platform's group-user env list is called, if not Telegram.
    pub group_user_allowed_env: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AuthzInput<'a> {
    pub source: &'a SessionSource,
    /// True when the adapter verified a platform role (e.g. DISCORD_ALLOWED_ROLES).
    pub role_authorized: bool,
    pub is_bot: bool,
    pub spec: &'a PlatformAuthSpec,
    pub allow_adapter_delegation: bool,
}

/// Resolution of the unauthorized-DM behavior ("pair" | "ignore" | "decline").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnauthorizedDmBehavior {
    Pair,
    Ignore,
    Decline,
}

pub struct UnauthorizedDmContext {
    pub per_platform_override: Option<UnauthorizedDmBehavior>,
    pub explicit_global_non_default: Option<UnauthorizedDmBehavior>,
    pub dm_policy: Option<OwnPolicy>,
    pub allowlist_configured: bool,
    pub is_email: bool,
}

impl UnauthorizedDmContext {
    /// hermes `_get_unauthorized_dm_behavior` decision order.
    pub fn resolve(&self) -> UnauthorizedDmBehavior {
        if let Some(o) = self.per_platform_override {
            return o;
        }
        if self.is_email {
            return UnauthorizedDmBehavior::Ignore;
        }
        if let Some(g) = self.explicit_global_non_default {
            return g;
        }
        match self.dm_policy {
            Some(OwnPolicy::Pairing) => return UnauthorizedDmBehavior::Pair,
            Some(OwnPolicy::Allowlist) | Some(OwnPolicy::Disabled) => {
                return UnauthorizedDmBehavior::Ignore;
            }
            _ => {}
        }
        if self.allowlist_configured {
            return UnauthorizedDmBehavior::Ignore;
        }
        UnauthorizedDmBehavior::Pair
    }
}

/// The central authorizer. `env` is the process-environment view; `pairing` consulted only
/// after env allowlists fail (pairing ≠ access).
pub fn authorize_source(
    input: &AuthzInput,
    env: &dyn EnvLookup,
    pairing: Option<&dyn PairingVerdict>,
    own: Option<&OwnPolicySnapshot>,
) -> bool {
    let source = input.source;
    let platform_value = source.platform.as_str();
    // 0. Local principals are the host owner.
    if platform_value == "local" {
        return true;
    }
    // 1. Webhook / trusted upstream.
    if platform_value == "webhook" || input.spec.trusted_inbound {
        return true;
    }
    let is_group = ChatKind::from_chat_type(&source.chat_type).is_group_family();
    let is_forum = ChatKind::from_chat_type(&source.chat_type).is_forum_family();

    // 2. Chat-scoped grant.
    if input.allow_adapter_delegation
        && (source.delivered_via_upstream_relay || input.spec.authorization_is_upstream)
    {
        return true;
    }
    if is_group
        && let Some(env_name) = &input.spec.group_allowed_chats_env
        && allows(
            &coerce_allow_set(env.get(env_name).as_deref()),
            &source.chat_id,
        )
    {
        return true;
    }
    if input.is_bot
        && let Some(env_name) = &input.spec.allow_bots_env
    {
        let mode = env
            .get(env_name)
            .unwrap_or_else(|| "none".to_string())
            .trim()
            .to_ascii_lowercase();
        if mode == "mentions" || mode == "all" {
            return true;
        }
    }

    // 3. Missing user id → deny.
    let user_id = source.user_id.as_deref().unwrap_or("");
    if user_id.is_empty() {
        return false;
    }

    // 4. allow-all env.
    if let Some(env_name) = input
        .spec
        .allow_all_env
        .as_deref()
        .or(input.spec.allow_all_env_extra.as_deref())
        && let Some(raw) = env.get(env_name)
        && is_truthy(&raw)
    {
        return true;
    }
    // 5. Role authorization.
    if input.allow_adapter_delegation && input.role_authorized {
        return true;
    }
    // 6. Operator-approved pairing code.
    if let Some(store) = pairing
        && store.is_approved(platform_value, user_id)
    {
        return true;
    }

    // 7. Assembled allowlists.
    let platform_allowlist = input
        .spec
        .allowed_users_env
        .as_ref()
        .and_then(|k| env.get(k));
    let group_user_allowlist = input
        .spec
        .group_user_allowed_env
        .as_deref()
        .or_else(|| group_user_env_for(platform_value))
        .filter(|_| is_forum)
        .and_then(|k| env.get(k));
    let group_chat_allowlist = input
        .spec
        .group_allowed_chats_env
        .as_ref()
        .filter(|_| is_forum)
        .and_then(|k| env.get(k));
    let global_allowlist = env.get(GLOBAL_ALLOWED_ENV);

    if platform_allowlist.is_none()
        && group_user_allowlist.is_none()
        && group_chat_allowlist.is_none()
        && global_allowlist.is_none()
    {
        // No env allowlists: delegate to own-policy snapshot, then adapter extras, then global.
        if input.allow_adapter_delegation
            && input.spec.enforces_own_access_policy
            && let Some(snap) = own
        {
            if is_group {
                // hermes rubber-stamps groups when policy == allowlist; open/pairing/disabled → fall through.
                if snap.group_policy == OwnPolicy::Allowlist {
                    return true;
                }
            } else {
                return snap.is_dm_allowed_value(user_id);
            }
        }
        if let Some(extra) = &input.spec.adapter_extra_allow_from {
            let set: HashSet<String> = extra.iter().map(|s| s.trim().to_string()).collect();
            if allows(&set, user_id) {
                return true;
            }
        }
        if let Some(extra) = &input.spec.adapter_extra_group_allow_from
            && is_group
        {
            let set: HashSet<String> = extra.iter().map(|s| s.trim().to_string()).collect();
            if allows(&set, source.chat_id.as_str()) {
                return true;
            }
        }
        return env.get(GLOBAL_ALLOW_ALL_ENV).is_some_and(|v| is_truthy(&v));
    }

    // Group / forum chat-ID grant (forum family only — channel lists never apply).
    if is_forum {
        if let Some(raw) = group_chat_allowlist.as_deref()
            && allows(&coerce_allow_set(Some(raw)), &source.chat_id)
        {
            return true;
        }
        if platform_value == "telegram"
            && let Some(raw) = group_user_allowlist.as_deref()
            && legacy_telegram_chat_grant(raw, &source.chat_id)
        {
            return true;
        }
    }

    let mut allowed_ids = coerce_allow_set(platform_allowlist.as_deref());
    allowed_ids.extend(coerce_allow_set(group_user_allowlist.as_deref()));
    allowed_ids.extend(coerce_allow_set(global_allowlist.as_deref()));
    if allowed_ids.contains("*") {
        return true;
    }
    principal_matches(
        platform_value,
        user_id,
        source.user_id_alt.as_deref(),
        &allowed_ids,
    )
}

fn group_user_env_for(platform: &str) -> Option<&'static str> {
    GROUP_USER_ENV
        .iter()
        .find(|(p, _)| *p == platform)
        .map(|(_, e)| *e)
}

/// Telegram legacy: "-"-prefixed values in TELEGRAM_GROUP_ALLOWED_USERS mean chat IDs.
fn legacy_telegram_chat_grant(raw: &str, chat_id: &str) -> bool {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| s.starts_with('-'))
        .any(|s| s == chat_id)
}

/// Principal-vs-allowlist match with platform-specific alias semantics.
fn principal_matches(
    platform: &str,
    user_id: &str,
    user_id_alt: Option<&str>,
    allowed_ids: &HashSet<String>,
) -> bool {
    let mut check_ids: HashSet<String> = HashSet::new();
    check_ids.insert(user_id.to_string());
    if let Some(alt) = user_id_alt {
        let a = alt.trim();
        if !a.is_empty() {
            check_ids.insert(a.to_string());
        }
    }
    let mut allowed_expanded = allowed_ids.clone();
    match platform {
        "whatsapp" | "whatsapp_cloud" => {
            check_ids.extend(whatsapp_auth_aliases(user_id));
            if let Some(alt) = user_id_alt {
                check_ids.extend(whatsapp_auth_aliases(alt));
            }
            let mut expanded = HashSet::new();
            for entry in allowed_ids {
                expanded.extend(whatsapp_auth_aliases(entry));
            }
            allowed_expanded = expanded;
            allowed_expanded.extend(allowed_ids.iter().cloned());
        }
        "buzz" => {
            let expanded: HashSet<String> =
                allowed_ids.iter().map(|e| normalize_nostr(e)).collect();
            allowed_expanded = expanded;
            check_ids.insert(normalize_nostr(user_id));
        }
        "simplex" => {
            // Only the numeric contactId is trusted; display names are attacker-controlled.
            let keep = |id: &str| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit());
            check_ids.retain(|i| keep(i));
            allowed_expanded.retain(|i| keep(i));
        }
        _ => {}
    }
    !check_ids
        .intersection(&allowed_expanded)
        .collect::<Vec<_>>()
        .is_empty()
}

impl OwnPolicySnapshot {
    fn is_dm_allowed_value(&self, user_id: &str) -> bool {
        match self.dm_policy {
            OwnPolicy::Allowlist => allows(&self.dm_allow_from, user_id),
            OwnPolicy::Open => self.open_dm_opt_in,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Platform;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn spec() -> PlatformAuthSpec {
        PlatformAuthSpec {
            allowed_users_env: Some("TELEGRAM_ALLOWED_USERS".into()),
            allow_all_env: Some("TELEGRAM_ALLOW_ALL_USERS".into()),
            group_allowed_chats_env: Some("TELEGRAM_GROUP_ALLOWED_CHATS".into()),
            allow_bots_env: Some("TELEGRAM_ALLOW_BOTS".into()),
            allowlist_suppresses_pairing: true,
            enforces_own_access_policy: false,
            authorization_is_upstream: false,
            trusted_inbound: false,
            allow_all_env_extra: None,
            adapter_extra_allow_from: None,
            adapter_extra_group_allow_from: None,
            group_user_allowed_env: None,
        }
    }

    fn src(platform: &str, chat_type: &str, chat_id: &str, user_id: Option<&str>) -> SessionSource {
        let mut s = SessionSource::new(Platform::new(platform).expect("valid platform"), chat_id);
        if chat_type != "dm" {
            s.chat_type = chat_type.to_string();
        }
        s.user_id = user_id.map(|u| u.to_string());
        s
    }

    #[test]
    fn local_platform_always_allowed() {
        let s = src("local", "dm", "cli", None);
        let e = env(&[]);
        assert!(authorize_source(
            &AuthzInput {
                source: &s,
                role_authorized: false,
                is_bot: false,
                spec: &spec(),
                allow_adapter_delegation: true
            },
            &e,
            None,
            None,
        ));
    }

    #[test]
    fn user_id_missing_denied() {
        let s = src("telegram", "dm", "123", None);
        let e = env(&[]);
        assert!(!authorize_source(
            &AuthzInput {
                source: &s,
                role_authorized: false,
                is_bot: false,
                spec: &spec(),
                allow_adapter_delegation: true
            },
            &e,
            None,
            None,
        ));
    }

    #[test]
    fn allow_all_env_admits() {
        let s = src("telegram", "dm", "123", Some("u1"));
        let e = env(&[("TELEGRAM_ALLOW_ALL_USERS", "true")]);
        assert!(authorize_source(
            &AuthzInput {
                source: &s,
                role_authorized: false,
                is_bot: false,
                spec: &spec(),
                allow_adapter_delegation: true
            },
            &e,
            None,
            None,
        ));
    }

    #[test]
    fn allowlist_admits_listed_user() {
        let s = src("telegram", "dm", "123", Some("u1"));
        let e = env(&[("TELEGRAM_ALLOWED_USERS", "u1, u2")]);
        assert!(authorize_source(
            &AuthzInput {
                source: &s,
                role_authorized: false,
                is_bot: false,
                spec: &spec(),
                allow_adapter_delegation: true
            },
            &e,
            None,
            None,
        ));
        assert!(!authorize_source(
            &AuthzInput {
                source: &s,
                role_authorized: false,
                is_bot: false,
                spec: &spec(),
                allow_adapter_delegation: true
            },
            &env(&[("TELEGRAM_ALLOWED_USERS", "u9")]),
            None,
            None,
        ));
    }

    #[test]
    fn wildcard_admits_anyone() {
        let s = src("telegram", "dm", "123", Some("u1"));
        let e = env(&[("TELEGRAM_ALLOWED_USERS", "*")]);
        assert!(authorize_source(
            &AuthzInput {
                source: &s,
                role_authorized: false,
                is_bot: false,
                spec: &spec(),
                allow_adapter_delegation: true
            },
            &e,
            None,
            None,
        ));
    }

    #[test]
    fn group_chat_env_admits_chat_even_without_user() {
        let s = src("telegram", "group", "-100123", None);
        let e = env(&[("TELEGRAM_GROUP_ALLOWED_CHATS", "-100999, -100123")]);
        assert!(authorize_source(
            &AuthzInput {
                source: &s,
                role_authorized: false,
                is_bot: false,
                spec: &spec(),
                allow_adapter_delegation: true
            },
            &e,
            None,
            None,
        ));
    }

    #[test]
    fn channel_type_uses_group_chat_list_but_not_group_user_list() {
        // hermes _GROUP_CHAT_TYPES = {group, forum, channel}: chat-scoped grant applies to channels.
        let s = src("telegram", "channel", "-100123", Some("u1"));
        let e = env(&[("TELEGRAM_GROUP_ALLOWED_CHATS", "-100123")]);
        assert!(authorize_source(
            &AuthzInput {
                source: &s,
                role_authorized: false,
                is_bot: false,
                spec: &spec(),
                allow_adapter_delegation: true
            },
            &e,
            None,
            None,
        ));
        // channel is NOT forum-family -> TELEGRAM_GROUP_ALLOWED_USERS (sender list) never applies.
        let s = src("telegram", "channel", "-100999", Some("u1"));
        let e = env(&[("TELEGRAM_GROUP_ALLOWED_USERS", "u1")]);
        assert!(!authorize_source(
            &AuthzInput {
                source: &s,
                role_authorized: false,
                is_bot: false,
                spec: &spec(),
                allow_adapter_delegation: true
            },
            &e,
            None,
            None,
        ));
    }

    #[test]
    fn json_list_literal_decodes() {
        let set = coerce_allow_set(Some("[\"-1001\", 2]"));
        assert!(set.contains("-1001"));
        assert!(set.contains("2"));
        let set = coerce_allow_set(Some("a, \"b c\",'d'"));
        assert_eq!(
            set,
            ["a", "b c", "d"].into_iter().map(String::from).collect()
        );
    }

    #[test]
    fn truthy_detection() {
        assert!(is_truthy("TRUE"));
        assert!(is_truthy(" yes "));
        assert!(is_truthy("1"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy(""));
    }

    #[test]
    fn unauthorized_dm_behavior_order() {
        let ctx = UnauthorizedDmContext {
            per_platform_override: Some(UnauthorizedDmBehavior::Decline),
            explicit_global_non_default: Some(UnauthorizedDmBehavior::Pair),
            dm_policy: Some(OwnPolicy::Disabled),
            allowlist_configured: true,
            is_email: false,
        };
        assert_eq!(ctx.resolve(), UnauthorizedDmBehavior::Decline);
        let ctx = UnauthorizedDmContext {
            per_platform_override: None,
            explicit_global_non_default: None,
            dm_policy: Some(OwnPolicy::Pairing),
            allowlist_configured: false,
            is_email: false,
        };
        assert_eq!(ctx.resolve(), UnauthorizedDmBehavior::Pair);
        let ctx = UnauthorizedDmContext {
            per_platform_override: None,
            explicit_global_non_default: None,
            dm_policy: None,
            allowlist_configured: true,
            is_email: false,
        };
        assert_eq!(ctx.resolve(), UnauthorizedDmBehavior::Ignore);
        let ctx = UnauthorizedDmContext {
            per_platform_override: None,
            explicit_global_non_default: Some(UnauthorizedDmBehavior::Pair),
            dm_policy: None,
            allowlist_configured: true,
            is_email: true,
        };
        assert_eq!(ctx.resolve(), UnauthorizedDmBehavior::Ignore);
    }

    #[test]
    fn own_policy_intake_semantics() {
        let snap = OwnPolicySnapshot {
            dm_policy: OwnPolicy::Pairing,
            group_policy: OwnPolicy::Disabled,
            dm_allow_from: HashSet::new(),
            group_allow_from: HashSet::new(),
            open_dm_opt_in: false,
        };
        let policy = AccessPolicy::new(snap);
        assert!(policy.is_dm_intake_allowed("anyone"));
        assert!(!policy.is_dm_intake_allowed(""));
        assert!(!policy.is_group_allowed("g", "s"));
    }

    #[test]
    fn napub_decodes_to_hex_shape() {
        // Not a valid npub — must pass through unchanged instead of panicking.
        assert_eq!(normalize_nostr("npub1notvalid"), "npub1notvalid");
        assert_eq!(normalize_nostr("deadbeef"), "deadbeef");
    }
}
