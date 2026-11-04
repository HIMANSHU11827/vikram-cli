/// Strict semver core check: exactly `MAJOR.MINOR.PATCH`, all numeric.
pub fn is_semver(raw: &str) -> bool {
    let core = numeric_core(raw);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The part of a version string before any pre-release (`-`) or build (`+`) metadata.
fn numeric_core(raw: &str) -> &str {
    let trimmed = raw.trim();
    let cut = trimmed.find(['-', '+']).unwrap_or(trimmed.len());
    &trimmed[..cut]
}

/// Parse up to three numeric components; missing trailing components are zero.
pub(crate) fn numeric_parts(raw: &str) -> Option<(u64, u64, u64)> {
    let core = numeric_core(raw);
    if core.is_empty() {
        return None;
    }
    let mut numbers = [0u64; 3];
    let mut count = 0;
    for part in core.split('.') {
        if count >= 3 {
            return None;
        }
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        numbers[count] = part.parse().ok()?;
        count += 1;
    }
    if count == 0 {
        return None;
    }
    Some((numbers[0], numbers[1], numbers[2]))
}

/// Does `version` satisfy the requirement `req`?
///
/// Supported forms: exact (`1.2.3`, optional `=` prefix), caret (`^1.2.3`), tilde (`~1.2.3`) and
/// `>=1.2.3`. An empty requirement or `*` matches anything. Anything else does not match.
pub fn requirement_satisfied(version: &str, req: &str) -> bool {
    let req = req.trim();
    if req.is_empty() || req == "*" {
        return true;
    }
    let Some(found) = numeric_parts(version) else {
        return false;
    };
    if let Some(rest) = req.strip_prefix(">=") {
        return numeric_parts(rest).map(|min| found >= min).unwrap_or(false);
    }
    if let Some(rest) = req.strip_prefix('^') {
        let Some(min) = numeric_parts(rest) else {
            return false;
        };
        if found < min {
            return false;
        }
        return if min.0 > 0 {
            found.0 == min.0
        } else if min.1 > 0 {
            found.0 == 0 && found.1 == min.1
        } else {
            found == min
        };
    }
    if let Some(rest) = req.strip_prefix('~') {
        let Some(min) = numeric_parts(rest) else {
            return false;
        };
        return found >= min && found.0 == min.0 && found.1 == min.1;
    }
    let exact = req.strip_prefix('=').unwrap_or(req);
    numeric_parts(exact)
        .map(|want| found == want)
        .unwrap_or(false)
}
