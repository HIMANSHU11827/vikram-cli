//! Secret redaction for persisted transcripts. Tool arguments and results are journaled verbatim,
//! so a command like `curl -H "Authorization: Bearer …"` would otherwise sit in plaintext in the
//! session store. This is a pattern scan, not a parser: it errs toward redacting.

/// Names whose `name=value` / `name: value` / `name value` forms carry a credential.
const SENSITIVE_NAMES: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "access_token",
    "refresh_token",
    "auth_token",
    "session_token",
    "private_key",
    "access_key",
    "secret_key",
    "client_secret",
    "authorization",
    "auth",
    "credential",
    "bearer",
];

/// Token shapes that are secrets wherever they appear.
const TOKEN_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "xoxs-",
    "AKIA",
    "ASIA",
    "AIza",
    "ya29.",
    "dop_v1_",
    "npm_",
    "pypi-",
    "glpat-",
];

const REDACTED: &str = "[redacted]";

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

fn name_is_sensitive(name: &str) -> bool {
    let trimmed = name.to_lowercase();
    let trimmed = trimmed.trim_end_matches([':', '=']);
    SENSITIVE_NAMES.contains(&trimmed)
        || trimmed.ends_with("_key")
        || trimmed.ends_with("_token")
        || trimmed.ends_with("_secret")
        || trimmed.ends_with("_password")
}

/// Redact secrets in free text: `name=value`, `name: value`, `Bearer x`, and known token shapes.
pub fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let gap = rest.len() - rest.trim_start().len();
        if gap > 0 {
            out.push_str(&rest[..gap]);
            rest = rest.trim_start();
            if rest.is_empty() {
                break;
            }
        }
        // Try to match a sensitive `name<sep>` at the current position's word start.
        let word_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..word_len];
        if let Some((_name, value_start)) = split_named_secret(word) {
            out.push_str(&redact_named(word, value_start));
            rest = &rest[word_len..];
            continue;
        }
        if let Some(_prefix_len) = token_prefix_hit(word) {
            out.push_str(REDACTED);
            rest = &rest[word_len..];
            continue;
        }
        // `Bearer <token>` — the value is the NEXT word.
        if word.eq_ignore_ascii_case("bearer") {
            out.push_str(word);
            rest = &rest[word_len..];
            let gap = rest.len() - rest.trim_start().len();
            out.push_str(&rest[..gap]);
            rest = rest.trim_start();
            let vlen = rest.find(char::is_whitespace).unwrap_or(rest.len());
            out.push_str(&redact_value_word(&rest[..vlen]));
            rest = &rest[vlen..];
            continue;
        }
        // `name:` / `name=` with the value in the NEXT word (`Authorization: Bearer x`,
        // `token: abc123`). The name stays for context; the value becomes the marker.
        if names_only(word) {
            out.push_str(word);
            rest = &rest[word_len..];
            let gap = rest.len() - rest.trim_start().len();
            out.push_str(&rest[..gap]);
            rest = rest.trim_start();
            // `Bearer` after the name is a scheme, not the secret: keep it and redact its value.
            let vlen = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let next = &rest[..vlen];
            if next.eq_ignore_ascii_case("bearer") {
                out.push_str(next);
                rest = &rest[vlen..];
                let gap = rest.len() - rest.trim_start().len();
                out.push_str(&rest[..gap]);
                rest = rest.trim_start();
                let vlen = rest.find(char::is_whitespace).unwrap_or(rest.len());
                out.push_str(&redact_value_word(&rest[..vlen]));
                rest = &rest[vlen..];
            } else {
                out.push_str(&redact_value_word(next));
                rest = &rest[vlen..];
            }
            continue;
        }
        out.push_str(word);
        rest = &rest[word_len..];
    }
    out
}

/// If `word` is `name<sep>value…` where name is sensitive, return (name, byte offset of value).
fn split_named_secret(word: &str) -> Option<(&str, usize)> {
    for sep in ['=', ':'] {
        if let Some(idx) = word.find(sep) {
            let name = &word[..idx];
            // An empty value means the secret is the NEXT word — `names_only` handles that.
            if !name.is_empty()
                && name.chars().all(is_name_char)
                && name_is_sensitive(name)
                && idx + 1 < word.len()
            {
                return Some((name, idx + 1));
            }
        }
    }
    None
}

/// `name:` or `name=` ending a word — the secret is the next word. Quotes and other punctuation
/// around the name are ignored so `"Authorization:` still counts.
fn names_only(word: &str) -> bool {
    if !(word.ends_with(':') || word.ends_with('=')) {
        return false;
    }
    let core = word
        .trim_end_matches([':', '='])
        .trim_matches(|c: char| !is_name_char(c));
    name_is_sensitive(core)
}

/// Redact the value half of a `name<sep>value` word, keeping the name for context.
fn redact_named(word: &str, value_start: usize) -> String {
    let (head, _value) = word.split_at(value_start);
    format!("{head}{REDACTED}")
}

/// `[redacted]` plus any trailing punctuation the value word carried (`token"` keeps its quote).
fn redact_value_word(word: &str) -> String {
    let core_len = word
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | '='))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{REDACTED}{}", &word[core_len..])
}

fn token_prefix_hit(word: &str) -> Option<usize> {
    for prefix in TOKEN_PREFIXES {
        if word.starts_with(prefix) && word.len() > prefix.len() + 4 {
            // Require the tail to look token-ish so `sk-` in prose doesn't nuke a sentence.
            let tail = &word[prefix.len()..];
            if tail
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            {
                return Some(prefix.len());
            }
        }
    }
    None
}

/// Redact every string inside a JSON value (arguments, journaled messages). A sensitive object
/// key redacts its whole value — `{"api_key": "hunter2"}` has no token shape to match.
pub fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            let redacted = redact_secrets(s);
            if redacted != *s {
                *s = redacted;
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json),
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if name_is_sensitive(key) {
                    if !item.is_null() {
                        *item = serde_json::Value::String(REDACTED.to_owned());
                    }
                } else {
                    redact_json(item);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_tokens_are_redacted() {
        assert_eq!(
            redact_secrets("curl -H \"Authorization: Bearer abc123def456\" https://x"),
            "curl -H \"Authorization: Bearer [redacted]\" https://x"
        );
        assert_eq!(redact_secrets("Bearer tok_12345678"), "Bearer [redacted]");
    }

    #[test]
    fn named_secrets_are_redacted() {
        assert_eq!(
            redact_secrets("api_key=sk-live-1234567890 password=hunter2secret"),
            "api_key=[redacted] password=[redacted]"
        );
        assert_eq!(redact_secrets("token: abcdef123456"), "token: [redacted]");
    }

    #[test]
    fn known_token_shapes_are_redacted() {
        assert_eq!(
            redact_secrets("key is ghp_abcdefghijklmnop here"),
            "key is [redacted] here"
        );
        assert_eq!(redact_secrets("AKIAIOSFODNN7EXAMPLE"), "[redacted]");
        assert_eq!(redact_secrets("xoxb-1234567890-abcdef"), "[redacted]");
    }

    #[test]
    fn ordinary_text_survives() {
        let text = "read config.rs and fix the token bucket sizing";
        assert_eq!(redact_secrets(text), text);
        assert_eq!(redact_secrets("sk- is not a token"), "sk- is not a token");
    }

    #[test]
    fn json_values_are_walked() {
        let mut v = serde_json::json!({
            "command": "curl -H 'Authorization: Bearer secrettoken123' x",
            "nested": {"api_key": "abcdef123456"},
            "count": 3
        });
        redact_json(&mut v);
        let s = v.to_string();
        assert!(!s.contains("secrettoken123"));
        assert!(!s.contains("abcdef123456"));
        assert!(s.contains("[redacted]"));
    }
}
