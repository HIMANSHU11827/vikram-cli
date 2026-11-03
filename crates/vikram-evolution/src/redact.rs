//! Secret redaction and prompt-injection scanning for learned content.

use crate::error::{EvolutionError, Result};

const SECRET_PATTERNS: &[&str] = &[
    "api_key",
    "apikey",
    "password",
    "passwd",
    "secret",
    "token",
    "private_key",
    "access_key",
    "bearer ",
    "sk-",
    "ghp_",
    "xoxb-",
    "AKIA",
];

pub fn redact_secrets(input: &str) -> String {
    // Line-based redaction
    let mut lines = Vec::new();
    for line in input.lines() {
        let lower = line.to_lowercase();
        if SECRET_PATTERNS.iter().any(|p| lower.contains(p)) {
            // redact after first '=' or ':'
            if let Some(idx) = line.find('=').or_else(|| line.find(':')) {
                let (head, _) = line.split_at(idx + 1);
                lines.push(format!("{head} [REDACTED]"));
            } else {
                lines.push("[REDACTED LINE]".to_string());
            }
        } else {
            lines.push(line.to_string());
        }
    }
    lines.join("\n")
}

pub fn scan_injection(input: &str) -> Result<()> {
    let lower = input.to_lowercase();
    let dangerous = [
        "ignore previous instructions",
        "ignore all previous",
        "system prompt",
        "you are now",
        "disregard the above",
        "override safety",
        "bypass security",
        "disable approval",
    ];
    for d in dangerous {
        if lower.contains(d) {
            return Err(EvolutionError::Security(format!(
                "potential prompt injection detected: {d}"
            )));
        }
    }
    Ok(())
}

pub fn sanitize_for_storage(input: &str) -> Result<String> {
    scan_injection(input)?;
    Ok(redact_secrets(input))
}
