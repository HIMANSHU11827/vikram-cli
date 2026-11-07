//! Routing module: byte-compatible port of hermes `build_session_key`
//! (gateway/session.py) and WhatsApp identity canonicalisation
//! (gateway/whatsapp_identity.py).
//!
//! The session key is a stable, deterministic `<ns>:<platform>:<slot>[:<scope>][:<chat_id>][:<thread_id>][:<user>]`
//! string. Hermes uses it as the routing index into the session store, so keys
//! produced here must byte-match hermes for every source they share.
//!
//! WhatsApp lid-mapping files (`lid-mapping-*.json` in the bridge session dir)
//! are deliberately NOT consulted: they only exist on hermes hosts that ran the
//! Baileys bridge and pair WhatsApp. Vikram will grow its own mapping store under
//! `$VIKRAM_HOME` when the WhatsApp adapter lands; until then `expand_aliases`
//! degrades to the identifier's own forms, which is the same result hermes
//! produces when no mapping files exist.

use crate::event::{SessionSource, platform_names};

/// Options mirroring `build_session_key(group_sessions_per_user, thread_sessions_per_user)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionKeyOptions {
    /// Groups/channels get a per-participant key unless in a shared thread.
    pub group_sessions_per_user: bool,
    /// Threads isolate per participant instead of sharing one key.
    pub thread_sessions_per_user: bool,
}

impl Default for SessionKeyOptions {
    fn default() -> Self {
        SessionKeyOptions {
            group_sessions_per_user: true,
            thread_sessions_per_user: false,
        }
    }
}

/// `<ns>` prefix for the key. `default`/`None` keep the historical `agent:main`
/// namespace; a profile literally named `main` is marked `main~` because `~` is
/// outside the profile-id alphabet, so it can never be another profile's id.
fn session_key_namespace(profile: Option<&str>) -> String {
    match profile {
        None | Some("") | Some("default") => "agent:main".to_string(),
        Some("main") => "agent:main~".to_string(),
        Some(other) => format!("agent:{other}"),
    }
}

/// Strip whitespace, one leading `+`, then drop the device suffix (`:`)
/// and the domain (`@`). Port of hermes
/// `normalize_whatsapp_identifier` (whatsapp_identity.py).
fn normalize_whatsapp_identifier(value: &str) -> String {
    value
        .trim()
        .replacen('+', "", 1)
        .split(':')
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("")
        .to_string()
}

/// True for a >=7-digit local part (jid or bare phone).
#[cfg(test)]
fn is_plausible_whatsapp_identifier(identifier: &str) -> bool {
    let normalized = normalize_whatsapp_identifier(identifier);
    if normalized.is_empty() {
        return false;
    }
    let local = normalized.split('@').next().unwrap_or("");
    local.chars().filter(|c| c.is_ascii_digit()).count() >= 7
}

/// Stable sender identity across phone-JID/LID variants. Empty input -> "".
///
/// Hermes additionally expands the bridge's `lid-mapping-*.json` files and takes
/// the shortest transitive alias. Those files do not exist in VIKRAM yet
/// (the mapping store lands with the WhatsApp adapter), so this degrades to
/// normalize() — the same result hermes produces when no mapping files exist.
pub fn canonical_whatsapp_identifier(identifier: &str) -> String {
    normalize_whatsapp_identifier(identifier)
}

/// Wire-invisible: `detached_route` is the key of a conversation this chat has
/// moved past; a detached source routes straight to it.
fn detached_route_of(source: &SessionSource) -> Option<&str> {
    source.detached_route.as_deref().filter(|s| !s.is_empty())
}

/// Sender id used for key isolation; WhatsApp aliases are canonicalised so
/// alias flips cannot split one member into two sessions.
fn canonical_participant(source: &SessionSource) -> Option<String> {
    let participant = source
        .user_id_alt
        .as_deref()
        .or(source.user_id.as_deref())?;
    if participant.is_empty() {
        return None;
    }
    if source.platform.as_str() == platform_names::WHATSAPP {
        let canonical = canonical_whatsapp_identifier(participant);
        if !canonical.is_empty() {
            return Some(canonical);
        }
    }
    Some(participant.to_string())
}

/// True when a non-DM session is shared across participants (mirrors the
/// isolation rules in `build_session_key`).
pub fn shared_session_participant(source: &SessionSource, opts: &SessionKeyOptions) -> bool {
    if source.chat_type == "dm" {
        return false;
    }
    let in_thread = source.thread_id.is_some();
    !(if in_thread {
        opts.thread_sessions_per_user
    } else {
        opts.group_sessions_per_user
    })
}

/// Build a deterministic session key from a message source.
///
/// Layout: `<ns>:<platform>:<slot>[:<scope_id>][:<chat_id>][:<thread_id>][:<user>]`.
/// Slack `scope_id` precedes chat ids (Discord guild scope is deliberately NOT
/// added, for key compatibility). DMs are isolated per chat_id, falling back to
/// the sender id, then to one session per platform. Groups add the participant
/// id only when `group_sessions_per_user` and not in a thread (threads are
/// shared unless `thread_sessions_per_user`).
pub fn build_session_key(
    source: &SessionSource,
    opts: &SessionKeyOptions,
    profile: Option<&str>,
) -> String {
    if let Some(detached) = detached_route_of(source) {
        return detached.to_string();
    }
    let is_dm = source.chat_type == "dm";
    let mut chat_id = source.chat_id.clone();
    if is_dm && source.platform.as_str() == platform_names::WHATSAPP {
        chat_id = canonical_whatsapp_identifier(&chat_id);
    }
    let thread_id = source.thread_id.clone().or_else(|| {
        if is_dm {
            None
        } else {
            source.prospective_thread_id.clone()
        }
    });
    let chat_type_slot = if thread_id.is_some() && source.thread_id.is_none() {
        "thread"
    } else {
        source.chat_type.as_str()
    };
    let isolate_user = if is_dm {
        chat_id.is_empty()
    } else {
        opts.group_sessions_per_user && !(thread_id.is_some() && !opts.thread_sessions_per_user)
    };
    let participant_id = if isolate_user || !is_dm {
        canonical_participant(source)
    } else {
        None
    };

    let mut parts: Vec<String> = vec![
        session_key_namespace(profile),
        source.platform.as_str().to_string(),
        chat_type_slot.to_string(),
    ];
    if source.platform.as_str() == platform_names::SLACK
        && let Some(scope) = source.scope_id.as_deref()
        && !scope.is_empty()
    {
        parts.push(scope.to_string());
    }
    if !chat_id.is_empty() {
        parts.push(chat_id);
    }
    let user_part: Vec<String> = match (isolate_user, participant_id) {
        (true, Some(p)) => vec![p],
        _ => Vec::new(),
    };
    let thread_part: Vec<String> = thread_id.map(|t| vec![t]).unwrap_or_default();
    if is_dm {
        parts.extend(user_part);
        parts.extend(thread_part);
    } else {
        parts.extend(thread_part);
        parts.extend(user_part);
    }
    parts.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Platform;

    fn src(platform: &str, chat_type: &str, chat_id: &str) -> SessionSource {
        let mut s = SessionSource::new(Platform::new(platform).unwrap(), chat_id);
        s.chat_type = chat_type.to_string();
        s
    }

    #[test]
    fn dm_is_per_chat_id() {
        let s = src("telegram", "dm", "42");
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:telegram:dm:42"
        );
    }

    #[test]
    fn dm_falls_back_to_participant_then_sink() {
        let mut s = src("telegram", "dm", "");
        s.user_id = Some("7".to_string());
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:telegram:dm:7"
        );
        s.user_id = None;
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:telegram:dm"
        );
    }

    #[test]
    fn dm_participant_precedes_thread() {
        let s = src("telegram", "dm", "");
        let mut s = s;
        s.thread_id = Some("99".to_string());
        s.user_id = Some("7".to_string());
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:telegram:dm:7:99"
        );
    }

    #[test]
    fn group_adds_participant_when_isolating() {
        let mut s = src("telegram", "group", "42");
        s.user_id = Some("7".to_string());
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:telegram:group:42:7"
        );
        let opts = SessionKeyOptions {
            group_sessions_per_user: false,
            ..SessionKeyOptions::default()
        };
        assert_eq!(
            build_session_key(&s, &opts, None),
            "agent:main:telegram:group:42"
        );
    }

    #[test]
    fn group_threads_are_shared_unless_opted_in() {
        let mut s = src("telegram", "group", "42");
        s.thread_id = Some("99".to_string());
        s.user_id = Some("7".to_string());
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:telegram:group:42:99"
        );
        let opts = SessionKeyOptions {
            thread_sessions_per_user: true,
            ..SessionKeyOptions::default()
        };
        assert_eq!(
            build_session_key(&s, &opts, None),
            "agent:main:telegram:group:42:99:7"
        );
    }

    #[test]
    fn prospective_thread_normalizes_slot() {
        let mut s = src("discord", "channel", "42");
        s.prospective_thread_id = Some("99".to_string());
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:discord:thread:42:99"
        );
        s.thread_id = Some("100".to_string());
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:discord:channel:42:100"
        );
    }

    #[test]
    fn slack_scope_precedes_chat_id_and_discord_scope_is_dropped() {
        let mut s = src("slack", "channel", "42");
        s.scope_id = Some("w1".to_string());
        s.user_id = Some("7".to_string());
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:slack:channel:w1:42:7"
        );
        let mut d = src("discord", "channel", "42");
        d.scope_id = Some("guild9".to_string());
        assert_eq!(
            build_session_key(&d, &SessionKeyOptions::default(), None),
            "agent:main:discord:channel:42"
        );
    }

    #[test]
    fn profile_namespaces_distinguish_default_main_and_named() {
        let s = src("telegram", "dm", "42");
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:telegram:dm:42"
        );
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), Some("default")),
            "agent:main:telegram:dm:42"
        );
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), Some("main")),
            "agent:main~:telegram:dm:42"
        );
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), Some("rob")),
            "agent:rob:telegram:dm:42"
        );
    }

    #[test]
    fn whatsapp_jid_canonicalizes_to_digits() {
        let mut s = src("whatsapp", "dm", "6012:47@s.whatsapp.net");
        s.user_id = Some("6012:9@lid".to_string());
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:whatsapp:dm:6012"
        );
    }

    #[test]
    fn whatsapp_bare_phone_normalizes_to_digits() {
        let s = src("whatsapp", "dm", "+601234567");
        assert_eq!(canonical_whatsapp_identifier(&s.chat_id), "601234567");
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:main:whatsapp:dm:601234567"
        );
    }

    #[test]
    fn detached_route_wins_verbatim() {
        let mut s = src("telegram", "dm", "42");
        s.detached_route = Some("agent:rob:detached:sess_9".to_string());
        assert_eq!(
            build_session_key(&s, &SessionKeyOptions::default(), None),
            "agent:rob:detached:sess_9"
        );
    }

    #[test]
    fn is_plausible_requires_seven_digits() {
        assert!(is_plausible_whatsapp_identifier("60123456789"));
        assert!(is_plausible_whatsapp_identifier("6012345@s.whatsapp.net"));
        assert!(!is_plausible_whatsapp_identifier("12@s.whatsapp.net"));
        assert!(!is_plausible_whatsapp_identifier(""));
    }
}
