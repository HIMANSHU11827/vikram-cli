use std::path::Path;

/// The SHA-256 round constants: the first 32 bits of the fractional parts of the cube roots of
/// the first 64 primes.
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The SHA-256 initial hash values: the first 32 bits of the fractional parts of the square roots
/// of the first 8 primes.
const SHA256_H: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// The SHA-256 digest of `data`, as 32 bytes, big-endian words.
///
/// Self-contained on purpose: verifying a plugin's `plugin.sha256` should not force a hash crate
/// into the dependency graph, and the algorithm is small enough to keep honest here.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state = SHA256_H;
    let bit_length = (data.len() as u64).wrapping_mul(8);

    // Padding: 0x80, then zeroes until the block is 56 bytes long, then the 64-bit bit length.
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for block in message.chunks_exact(64) {
        let mut schedule = [0u32; 64];
        for (slot, word) in schedule.iter_mut().zip(block.chunks_exact(4)) {
            *slot = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = schedule[i - 15].rotate_right(7)
                ^ schedule[i - 15].rotate_right(18)
                ^ (schedule[i - 15] >> 3);
            let s1 = schedule[i - 2].rotate_right(17)
                ^ schedule[i - 2].rotate_right(19)
                ^ (schedule[i - 2] >> 10);
            schedule[i] = schedule[i - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[i - 7])
                .wrapping_add(s1);
        }

        let mut working = state;
        for (constant, word) in SHA256_K.iter().zip(schedule.iter()) {
            let s1 = working[4].rotate_right(6)
                ^ working[4].rotate_right(11)
                ^ working[4].rotate_right(25);
            let choose = (working[4] & working[5]) ^ (!working[4] & working[6]);
            let temp1 = working[7]
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(*constant)
                .wrapping_add(*word);
            let s0 = working[0].rotate_right(2)
                ^ working[0].rotate_right(13)
                ^ working[0].rotate_right(22);
            let majority =
                (working[0] & working[1]) ^ (working[0] & working[2]) ^ (working[1] & working[2]);
            let temp2 = s0.wrapping_add(majority);

            working[7] = working[6];
            working[6] = working[5];
            working[5] = working[4];
            working[4] = working[3].wrapping_add(temp1);
            working[3] = working[2];
            working[2] = working[1];
            working[1] = working[0];
            working[0] = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip(working.iter()) {
            *slot = slot.wrapping_add(*value);
        }
    }

    let mut digest = [0u8; 32];
    for (word, bytes) in state.iter().zip(digest.chunks_exact_mut(4)) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// The lowercase hex SHA-256 of `data` — the form `plugin.sha256` holds.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in sha256(data) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The hex SHA-256 of a directory's `plugin.json`.
fn manifest_digest(dir: &Path) -> Result<String, String> {
    let path = dir.join("plugin.json");
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

/// The hash a directory's `plugin.sha256` asks for, `None` when the file is absent.
fn expected_manifest_digest(dir: &Path) -> Result<Option<String>, String> {
    let path = dir.join(PLUGIN_HASH_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    // Tolerate the `sha256sum` output form (`<hex>  plugin.json`) as well as a bare hex line.
    let field = raw.split_whitespace().next().unwrap_or_default();
    Ok(Some(field.trim().to_ascii_lowercase()))
}

/// Verify a plugin directory against its shipped `plugin.sha256`.
///
/// `Ok(None)` means the plugin ships no hash (nothing to check), `Ok(Some(hash))` means the hash
/// matched, and `Err` means it did not — the message names the plugin and the file, and says
/// exactly which comparison failed.
pub(crate) fn verify_integrity(dir: &Path, id: &str) -> Result<Option<String>, String> {
    let Some(expected) = expected_manifest_digest(dir)? else {
        return Ok(None);
    };
    let actual = manifest_digest(dir)?;
    if actual == expected {
        return Ok(Some(actual));
    }
    Err(format!(
        "integrity check failed for `{id}`: plugin.json hash does not match {PLUGIN_HASH_FILE}"
    ))
}

/// One line describing a directory's integrity state, for [`PluginRegistry::inspect`].
pub(crate) fn integrity_summary(dir: &Path) -> String {
    match expected_manifest_digest(dir) {
        Err(err) => format!("cannot be checked ({err})"),
        Ok(None) => "not verified (no plugin.sha256)".to_string(),
        Ok(Some(expected)) => match manifest_digest(dir) {
            Err(err) => format!("cannot be checked ({err})"),
            Ok(actual) if actual == expected => format!("plugin.sha256 matches ({actual})"),
            Ok(actual) => format!("plugin.sha256 MISMATCH: plugin.json hashes to {actual}"),
        },
    }
}

/// Is there a non-empty `plugin.sig`? Presence only — nothing here verifies a signature.
pub(crate) fn signature_present(dir: &Path) -> bool {
    let path = dir.join(PLUGIN_SIG_FILE);
    std::fs::read_to_string(&path)
        .map(|raw| !raw.trim().is_empty())
        .unwrap_or(false)
}

/// Render a dependency chain as `` `a` -> `b` -> `a` ``.
pub(crate) fn quote_chain(chain: &[String]) -> String {
    chain
        .iter()
        .map(|link| format!("`{link}`"))
        .collect::<Vec<_>>()
        .join(" -> ")
}

// ---- versions ------------------------------------------------------------

use crate::plugins::*;
