//! JWT verification for webhook platforms that sign their requests with a JWT.
//!
//! Google Chat and the Bot Framework both deliver webhook POSTs with an
//! `Authorization: Bearer <jwt>` header, and that token is the only proof the request came from the
//! platform. This module verifies it: an RS256 or ES256 signature over the compact JWS, checked
//! against the platform's published JWKS, followed by the issuer, audience and expiry rules the
//! platform documents.
//!
//! Port status: complete. [`JwtVerifier::verify`] does the whole check and
//! [`validate_claims`] holds the claim rules on their own, so they can be tested (and reused)
//! without any crypto. hermes-agent delegates the same job to `google.oauth2.id_token` and the
//! Bot Framework SDK; both are unavailable here, so the verification is implemented directly on
//! `ring` (the crate's only crypto dependency).
//!
//! # What is checked
//!
//! 1. **Shape** — exactly three dot-separated base64url parts (padding tolerated, never required),
//!    a JSON object header and a JSON object payload.
//! 2. **Algorithm** — `RS256` or `ES256` only. `none` is refused outright, and so is every
//!    symmetric algorithm (`HS256` and friends), because a platform's JWKS publishes public keys
//!    and a shared secret in a public JWKS would be a forgery kit.
//! 3. **Signature** — the JWK named by the header's `kid` when present (every key of the matching
//!    type when the header carries no `kid`):
//!    - RS256: `n`/`e` from the JWK feeds
//!      [`ring::signature::RsaPublicKeyComponents`] with
//!      `RSA_PKCS1_2048_8192_SHA256`;
//!    - ES256: `x`/`y` become the uncompressed SEC1 point `0x04 || x || y` for
//!      [`ring::signature::ECDSA_P256_SHA256_FIXED`] (a JWS ES256 signature is already `r || s`,
//!      not DER).
//! 4. **Claims** — see [`validate_claims`]: `exp` must be present and later than `now` minus a
//!    60-second clock-skew allowance, `iss` must equal the expected issuer, and `aud` (a string or
//!    an array of strings) must contain the expected audience.
//!
//! # Known gaps
//!
//! - **Key rotation inside the cache window.** A JWKS is cached per URL for an hour. An unknown
//!   `kid` is reported as `no key`, it does **not** force a
//!   refetch, so a key the platform rotated in is only picked up once the entry expires. The
//!   platforms keep overlap keys published for far longer than an hour, and the caller may build a
//!   fresh [`JwtVerifier`] to drop the cache if it ever needs to.
//! - **No certificate-chain validation.** Only the JWK's `n`/`e` or `x`/`y` are used; an `x5c`
//!   chain or a `jku` header is ignored, and the JWKS URL is the caller's constant, never a value
//!   from the token. Transport security (TLS to the JWKS host) is
//!   [`crate::transport::ReqwestTransport`]'s job.
//! - **No `nbf`/`iat`/`jti` handling.** Neither platform uses them, and the webhook endpoint is
//!   not replay-protected here — the host's dedup layer owns that.
//! - **`typ` is not required.** Google and the Bot Framework both send `JWT`, but neither
//!   documents it as required, so a missing `typ` is accepted.

use crate::transport::{HttpRequest, SharedTransport};
use ring::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The signature algorithm a RS256 JWS names in its header.
const ALG_RS256: &str = "RS256";

/// The signature algorithm an ES256 JWS names in its header.
const ALG_ES256: &str = "ES256";

/// How long a fetched JWKS stays usable. One hour matches Google's own cache headers for its
/// OAuth2 certificates and is far shorter than the lifetime of a published signing key.
const JWKS_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// How far a token's `exp` may trail the local clock before the token is called expired. Both
/// platforms issue short-lived tokens, and a webhook host's clock is not guaranteed to be exact.
const EXPIRY_LEEWAY_SECS: u64 = 60;

/// A P-256 coordinate is 32 bytes, so an uncompressed point is `1 + 32 + 32`.
const P256_COORDINATE_LEN: usize = 32;

/// A decoded set of verified claims.
#[derive(Clone, Debug, PartialEq)]
pub struct Claims {
    /// The `iss` claim; already known to equal the issuer the caller expected.
    pub issuer: String,
    /// The `sub` claim when the token carries one.
    pub subject: Option<String>,
    /// The `aud` claim, normalized from a string or an array of strings.
    pub audience: Vec<String>,
    /// The `exp` claim in seconds since the Unix epoch.
    pub expires_at: u64,
    /// The decoded payload, for platform-specific claims the caller wants to read (Google's
    /// `email`, the Bot Framework's `serviceurl`, …).
    pub raw: Value,
}

impl Claims {
    /// Build [`Claims`] from a decoded payload, rejecting a payload that is missing `iss` or `exp`.
    fn from_payload(payload: &Value) -> Result<Self, String> {
        if !payload.is_object() {
            return Err("malformed claims: payload is not a JSON object".to_string());
        }
        let issuer = payload
            .get("iss")
            .and_then(Value::as_str)
            .ok_or_else(|| "malformed claims: no string iss claim".to_string())?
            .to_string();
        let subject = payload
            .get("sub")
            .and_then(Value::as_str)
            .map(str::to_string);
        let audience = parse_audience(payload.get("aud"));
        let expires_at = payload
            .get("exp")
            .and_then(numeric_date)
            .ok_or_else(|| "malformed claims: no numeric exp claim".to_string())?;
        Ok(Self {
            issuer,
            subject,
            audience,
            expires_at,
            raw: payload.clone(),
        })
    }
}

/// One usable signing key from a JWKS. Components are already base64url-decoded to big-endian
/// bytes, which is exactly the shape `ring` wants.
#[derive(Clone, Debug, PartialEq)]
enum Jwk {
    /// An RSA key: modulus `n` and public exponent `e`.
    Rsa {
        kid: Option<String>,
        n: Vec<u8>,
        e: Vec<u8>,
    },
    /// A NIST P-256 key: affine coordinates `x` and `y`.
    Ec {
        kid: Option<String>,
        x: Vec<u8>,
        y: Vec<u8>,
    },
}

impl Jwk {
    /// The JWK `kid`, when the platform published one.
    fn kid(&self) -> Option<&str> {
        match self {
            Jwk::Rsa { kid, .. } | Jwk::Ec { kid, .. } => kid.as_deref(),
        }
    }

    /// Whether this key's type can check a signature made with `alg`.
    fn matches_alg(&self, alg: &str) -> bool {
        matches!(
            (self, alg),
            (Jwk::Rsa { .. }, ALG_RS256) | (Jwk::Ec { .. }, ALG_ES256)
        )
    }

    /// Verify `signature` over `message`. Any key problem (wrong length, out-of-range component,
    /// unusable point) is reported as `false` — the caller turns it into "bad signature", which is
    /// the only thing a verifier is entitled to say about a key it did not choose.
    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        match self {
            Jwk::Rsa { n, e, .. } => {
                let components = RsaPublicKeyComponents {
                    n: n.as_slice(),
                    e: e.as_slice(),
                };
                components
                    .verify(&signature::RSA_PKCS1_2048_8192_SHA256, message, signature)
                    .is_ok()
            }
            Jwk::Ec { x, y, .. } => {
                if x.len() != P256_COORDINATE_LEN
                    || y.len() != P256_COORDINATE_LEN
                    || signature.len() != P256_COORDINATE_LEN * 2
                {
                    return false;
                }
                let mut point = Vec::with_capacity(1 + P256_COORDINATE_LEN * 2);
                point.push(0x04); // uncompressed point, SEC 1 §2.3.3
                point.extend_from_slice(x);
                point.extend_from_slice(y);
                // The JWS ES256 signature is r || s (RFC 7518 §3.4), which is what the FIXED
                // algorithm expects; the ASN.1 variants would want DER here.
                UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point.as_slice())
                    .verify(message, signature)
                    .is_ok()
            }
        }
    }
}

/// One cached JWKS: the usable keys and when they were fetched.
struct CachedJwks {
    keys: Vec<Jwk>,
    fetched_at: Instant,
}

/// Verifies platform webhook JWTs against a published JWKS.
///
/// The transport is the same one the adapters use, so verification is testable with a mock and
/// needs no network in tests. The JWKS cache is a plain [`Mutex`] keyed by JWKS URL; it is never
/// held across an `await`.
pub struct JwtVerifier {
    transport: SharedTransport,
    cache: Mutex<HashMap<String, CachedJwks>>,
}

impl JwtVerifier {
    /// Build a verifier over `transport`. Nothing is fetched until the first
    /// [`verify`](JwtVerifier::verify).
    pub fn new(transport: SharedTransport) -> Self {
        Self {
            transport,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Verify `token` and return its claims.
    ///
    /// `jwks_url` is where the platform publishes its signing keys (always a constant, never a
    /// value taken from the token), `issuer` is the exact `iss` the platform uses and `audience` is
    /// the expected `aud` entry — the Google Cloud project number for Google Chat, the bot's
    /// Microsoft app id for the Bot Framework.
    ///
    /// # Errors
    ///
    /// Every failure names its reason: `malformed token` (bad shape or undecodable part),
    /// `unsupported alg`, `jwks fetch failed`, `no usable keys`, `no key` (no JWKS key for the
    /// header's `kid`, or none of the right type), `bad signature`, and — via
    /// [`validate_claims`] — `expired`, `wrong issuer` or `wrong audience`.
    pub async fn verify(
        &self,
        token: &str,
        jwks_url: &str,
        issuer: &str,
        audience: &str,
    ) -> Result<Claims, String> {
        let token = token.trim();
        let parts: Vec<&str> = token.split('.').collect();
        let [header_b64, payload_b64, signature_b64] = parts.as_slice() else {
            return Err(format!(
                "malformed token: expected 3 dot-separated parts, found {}",
                parts.len()
            ));
        };

        let header = decode_json_part(header_b64, "header")?;

        // The algorithm is settled before anything else is decoded: a token that asks for `none` (or
        // any algorithm this verifier does not implement) must be refused no matter how its payload
        // or signature are shaped.
        let alg = header
            .get("alg")
            .and_then(Value::as_str)
            .ok_or_else(|| "malformed token: header has no string alg".to_string())?;
        if alg.eq_ignore_ascii_case("none") {
            return Err("unsupported alg: none (an unsigned token is never accepted)".to_string());
        }
        if alg != ALG_RS256 && alg != ALG_ES256 {
            return Err(format!(
                "unsupported alg: {alg} (only {ALG_RS256} and {ALG_ES256} are accepted)"
            ));
        }

        let payload = decode_json_part(payload_b64, "payload")?;
        let signature = base64url_decode(signature_b64)
            .map_err(|err| format!("malformed token: signature is not valid base64url: {err}"))?;

        let keys = self.signing_keys(jwks_url).await?;
        let kid = header.get("kid").and_then(Value::as_str);
        let signing_input = format!("{header_b64}.{payload_b64}");
        verify_signature(&keys, kid, alg, signing_input.as_bytes(), &signature)?;

        let claims = Claims::from_payload(&payload)?;
        validate_claims(&claims, issuer, audience, now_unix())?;
        Ok(claims)
    }

    /// The cached keys for `jwks_url` while they are younger than [`JWKS_CACHE_TTL`], otherwise a
    /// fresh fetch.
    async fn signing_keys(&self, jwks_url: &str) -> Result<Vec<Jwk>, String> {
        if let Some(keys) = self.cached_keys(jwks_url) {
            return Ok(keys);
        }
        let keys = self.fetch_keys(jwks_url).await?;
        self.store_keys(jwks_url, &keys);
        Ok(keys)
    }

    /// The cached key set, if the entry exists and has not expired.
    fn cached_keys(&self, jwks_url: &str) -> Option<Vec<Jwk>> {
        let cache = lock_cache(&self.cache);
        let entry = cache.get(jwks_url)?;
        if entry.fetched_at.elapsed() < JWKS_CACHE_TTL {
            Some(entry.keys.clone())
        } else {
            None
        }
    }

    /// Replace the cache entry for `jwks_url`.
    fn store_keys(&self, jwks_url: &str, keys: &[Jwk]) {
        let mut cache = lock_cache(&self.cache);
        cache.insert(
            jwks_url.to_string(),
            CachedJwks {
                keys: keys.to_vec(),
                fetched_at: Instant::now(),
            },
        );
    }

    /// `GET jwks_url` and parse the key set out of it.
    async fn fetch_keys(&self, jwks_url: &str) -> Result<Vec<Jwk>, String> {
        let response = self
            .transport
            .execute(HttpRequest::get(jwks_url))
            .await
            .map_err(|err| format!("jwks fetch failed: {err}"))?;
        if !response.ok() {
            return Err(format!(
                "jwks fetch failed: HTTP {} from {jwks_url}",
                response.status
            ));
        }
        let body = response
            .json()
            .map_err(|err| format!("jwks fetch failed: response is not JSON: {err}"))?;
        let keys = body
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("jwks fetch failed: {jwks_url} published no keys array"))?;
        let mut usable = Vec::with_capacity(keys.len());
        for (index, key) in keys.iter().enumerate() {
            if let Some(jwk) = parse_jwk(key, index)? {
                usable.push(jwk);
            }
        }
        if usable.is_empty() {
            return Err(format!(
                "no usable keys: {jwks_url} published no RS256/ES256 key"
            ));
        }
        Ok(usable)
    }
}

/// The claim rules, without any crypto or I/O.
///
/// `now` is passed in (seconds since the Unix epoch) so the boundary is testable:
///
/// - **expiry** — `exp` must be present (enforced when [`Claims`] is built) and `now` must not be
///   more than the 60-second clock-skew allowance past it;
/// - **issuer** — `iss` must equal `issuer` exactly;
/// - **audience** — `audience` must be one of the token's `aud` entries (a token carrying `aud` as
///   a bare string is normalized to a one-element list).
///
/// An empty expected `issuer` or `audience` is refused rather than treated as a wildcard: a
/// misconfigured caller must fail closed.
///
/// # Errors
///
/// `expired`, `wrong issuer` or `wrong audience`, each naming the expected value.
pub fn validate_claims(
    claims: &Claims,
    issuer: &str,
    audience: &str,
    now: u64,
) -> Result<(), String> {
    if issuer.trim().is_empty() {
        return Err("wrong issuer: no expected issuer is configured".to_string());
    }
    if audience.trim().is_empty() {
        return Err("wrong audience: no expected audience is configured".to_string());
    }
    if now > claims.expires_at.saturating_add(EXPIRY_LEEWAY_SECS) {
        return Err(format!(
            "expired: token expired at {} (now {now})",
            claims.expires_at
        ));
    }
    if claims.issuer != issuer {
        return Err(format!(
            "wrong issuer: expected {issuer}, token says {}",
            claims.issuer
        ));
    }
    if !claims.audience.iter().any(|entry| entry == audience) {
        return Err(format!(
            "wrong audience: expected {audience}, token carries [{}]",
            claims.audience.join(", ")
        ));
    }
    Ok(())
}

/// Check `signature` over `message` against the keys a JWKS published.
///
/// The header's `kid` narrows the candidates when present; a token with no `kid` is tried against
/// every key of the right type. A `kid` that no published key carries is `no key`, and so is a key
/// set with nothing usable for `alg` — reporting those as "bad signature" would hide a config
/// error behind what looks like an attack.
fn verify_signature(
    keys: &[Jwk],
    kid: Option<&str>,
    alg: &str,
    message: &[u8],
    signature: &[u8],
) -> Result<(), String> {
    let candidates: Vec<&Jwk> = match kid {
        Some(kid) => keys.iter().filter(|key| key.kid() == Some(kid)).collect(),
        None => keys.iter().collect(),
    };
    if candidates.is_empty() {
        return Err(format!(
            "no key: the JWKS publishes no key with kid {}",
            kid.unwrap_or_default()
        ));
    }
    let mut tried = 0usize;
    for key in candidates {
        if !key.matches_alg(alg) {
            continue;
        }
        tried += 1;
        if key.verify(message, signature) {
            return Ok(());
        }
    }
    if tried == 0 {
        return Err(match kid {
            Some(kid) => format!("no key: the JWKS key {kid} cannot verify {alg}"),
            None => format!("no key: the JWKS publishes no {alg} key"),
        });
    }
    Err(format!(
        "bad signature: none of the {tried} candidate key(s) verified this token"
    ))
}

/// A non-panicking cache guard: a poisoned mutex still yields the map, matching the rest of the
/// crate's "no panic on shared state" rule.
fn lock_cache(
    cache: &Mutex<HashMap<String, CachedJwks>>,
) -> MutexGuard<'_, HashMap<String, CachedJwks>> {
    match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The token from an `Authorization: Bearer <token>` header, or `None` when the header carries no
/// HTTP bearer credential.
///
/// The scheme is matched case-insensitively (RFC 7235 §2.1 says it is case-insensitive, and
/// proxies do rewrite it); the token itself is returned untouched apart from surrounding
/// whitespace.
pub(crate) fn bearer_token(authorization: &str) -> Option<&str> {
    let (scheme, token) = authorization.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() { None } else { Some(token) }
}

/// Decode one JWS part as a base64url-encoded JSON object.
fn decode_json_part(part: &str, name: &str) -> Result<Value, String> {
    let bytes = base64url_decode(part)
        .map_err(|err| format!("malformed token: {name} is not valid base64url: {err}"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|err| format!("malformed token: {name} is not valid JSON: {err}"))?;
    if !value.is_object() {
        return Err(format!("malformed token: {name} is not a JSON object"));
    }
    Ok(value)
}

/// Normalize an `aud` claim to a list: a bare string is one entry, an array keeps its strings and
/// drops anything else, and a missing claim is empty (so the audience rule rejects it).
fn parse_audience(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(audience)) => vec![audience.clone()],
        Some(Value::Array(entries)) => entries
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// A NumericDate claim as seconds since the epoch. JSON numbers only — a stringified `exp` is not
/// something either platform sends, and accepting it would be inventing a leniency nobody asked
/// for. A fractional value is truncated.
fn numeric_date(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            let float = number.as_f64()?;
            (float.is_finite() && float >= 0.0).then_some(float as u64)
        }),
        _ => None,
    }
}

/// Parse one JWKS entry into a usable [`Jwk`].
///
/// `Ok(None)` means "not a key this verifier can use" — an unsupported `kty`, or an EC key on a
/// curve ES256 cannot use. That is a skip, not an error: platforms publish keys for algorithms we
/// do not implement, and one of them must not break verification of the others. A key that claims
/// a type we *do* implement but is missing its components is `Err`, because that JWKS is damaged.
fn parse_jwk(key: &Value, index: usize) -> Result<Option<Jwk>, String> {
    let kid = key.get("kid").and_then(Value::as_str).map(str::to_string);
    let label = match kid.as_deref() {
        Some(kid) => format!("key {kid}"),
        None => format!("key at index {index}"),
    };
    match key.get("kty").and_then(Value::as_str) {
        Some("RSA") => {
            let n = jwk_component(key, "n")
                .ok_or_else(|| format!("malformed jwks: RSA {label} has no base64url n"))?;
            let e = jwk_component(key, "e")
                .ok_or_else(|| format!("malformed jwks: RSA {label} has no base64url e"))?;
            Ok(Some(Jwk::Rsa { kid, n, e }))
        }
        Some("EC") => {
            if key.get("crv").and_then(Value::as_str) != Some("P-256") {
                return Ok(None);
            }
            let x = jwk_component(key, "x")
                .ok_or_else(|| format!("malformed jwks: EC {label} has no base64url x"))?;
            let y = jwk_component(key, "y")
                .ok_or_else(|| format!("malformed jwks: EC {label} has no base64url y"))?;
            Ok(Some(Jwk::Ec { kid, x, y }))
        }
        _ => Ok(None),
    }
}

/// One base64url JWK component, decoded.
fn jwk_component(key: &Value, name: &str) -> Option<Vec<u8>> {
    base64url_decode(key.get(name)?.as_str()?).ok()
}

/// Seconds since the Unix epoch. A clock before 1970 reads as zero, which can only make a token
/// look *newer* than it is — the expiry check still has to pass.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Decode base64url as JWS uses it: the `-`/`_` alphabet, and padding accepted but never required.
fn base64url_decode(input: &str) -> Result<Vec<u8>, String> {
    if input.is_empty() {
        return Err("empty input".to_string());
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            other => {
                return Err(format!(
                    "character {:?} is not in the base64url alphabet",
                    char::from(other)
                ));
            }
        };
        // Only the low six bits can still be pending, so masking to twelve bits is enough to keep
        // the accumulator from growing without bound.
        accumulator = ((accumulator << 6) | u32::from(value)) & 0x0FFF;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    if bits >= 6 {
        return Err("trailing symbol carries no data (length ≡ 1 mod 4)".to_string());
    }
    if accumulator & ((1u32 << bits) - 1) != 0 {
        return Err("non-zero trailing bits".to_string());
    }
    Ok(out)
}

/// Encode bytes as unpadded base64url. Test-only: production code never has to *produce* a JWS.
#[cfg(test)]
pub(crate) fn base64url_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let packed =
            (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);
        // One output character per six bits actually present; the spare low bits are dropped.
        for symbol in 0..chunk.len() + 1 {
            let shift = 18 - 6 * symbol;
            let index = usize::from(((packed >> shift) & 0x3F) as u8);
            match ALPHABET.get(index) {
                Some(character) => out.push(char::from(*character)),
                None => return out,
            }
        }
    }
    out
}

/// Test fixtures shared with the platform adapters' tests: a P-256 key, an RSA-2048 key, the JWKS
/// bodies that publish them, and JWS signers for both algorithms.
///
/// The keys are throwaway values generated with `openssl` for these tests only. They live here
/// (rather than in each adapter's test module) so the Google Chat and Teams tests sign with the
/// same key set the verifier tests use.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{base64url_decode, base64url_encode};
    use ring::rand::SystemRandom;
    use ring::signature::{self, EcdsaKeyPair, RsaKeyPair};
    use serde_json::{Value, json};

    /// `kid` the RSA fixture publishes, and the one the signer stamps into the header.
    pub(crate) const RSA_KID: &str = "test-rsa-1";

    /// `kid` the EC fixture publishes.
    pub(crate) const EC_KID: &str = "test-ec-1";

    /// The RSA-2048 fixture's modulus (base64url, big-endian, no leading zero).
    const RSA_N: &str = "vUAG1AefBUziNJWvI3eTcVtHrpEDbBLKcWLOEa5BgDfYshV66JTGJwrcnRKzxiz-6nbiNW07xmvsDcmmkZsiUIkorCFGWUnL9gyfG-nuwWqVVwfSxQ-mtS3F7fdjCwwr4-TYP_YKINFtTdgUVQ9uFch22BR8xW9FMXHnwNOvCD3yFP4l975J9VylULRQJzlOn-hKbSqqrQl5iJpN_3PuIHRA8J3XQ6Gijt4Cl8CawHZ009ylpCgXafJ-NRr5-MBkBD2_haITOmBAkNQUmXtNA7Gqx9aF0yxI4zh6LMWwcWonLN78UEerF22lzKpxMrYf3b6alrHIYjRpFxQ0My9Qyw";

    /// The RSA fixture's public exponent, 65537.
    const RSA_E: &str = "AQAB";

    /// The RSA fixture's PKCS#8 private key (2048-bit, `openssl genpkey`).
    const RSA_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC9QAbUB58FTOI0
la8jd5NxW0eukQNsEspxYs4RrkGAN9iyFXrolMYnCtydErPGLP7qduI1bTvGa+wN
yaaRmyJQiSisIUZZScv2DJ8b6e7BapVXB9LFD6a1LcXt92MLDCvj5Ng/9gog0W1N
2BRVD24VyHbYFHzFb0UxcefA068IPfIU/iX3vkn1XKVQtFAnOU6f6EptKqqtCXmI
mk3/c+4gdEDwnddDoaKO3gKXwJrAdnTT3KWkKBdp8n41Gvn4wGQEPb+FohM6YECQ
1BSZe00DsarH1oXTLEjjOHosxbBxaics3vxQR6sXbaXMqnEyth/dvpqWschiNGkX
FDQzL1DLAgMBAAECggEAMrn7OpESZErP5TVr/O87w6oqZBW1m53Lyqmnv6B8KSCP
Amj0iWv7UxK7wJR3QHrmFzWne5ecapqZ6rYu4h95iqnBD6ZZo2w+8eQ/L628E/b8
MZhEvBJuZmjvAVKKpzbresnn1vkdjN9l03REOuqIYOlbRZ70NL2XB+JWst5qr6Po
+4CRZbZtqiYceOtgXE2ShGZIHFpUmQ2ReSZCH9zfm8PkxlB08lfOskzbEUsiBTv/
Ua6NJEi8VkX1/7O8FDBAnxvlZKc/CiRlT+Bsi4ocHxQkotdUNGOYy/RGOOoKRBbj
zmV9Wa862dTzkivp3Jm5T1bv62fWJOrbdU8vNbFYgQKBgQDhl+tNByYxBI1NZCGm
KRfnFFT+XzStSgF2H8xRQZ77ZzaBvIpzyw834yhW9kIFCOiFM+Ag1pLDtaLXHHND
5r4cy6ZzexpA2uIAEVAI86TMjEm4GQuMMLJQmO2PMCtgGVw93B8n7J6jwVYEq4LO
eKpLnIbA0OlWgelxHXS8kzIbSwKBgQDWwhWRGOCBkVn718syWBs6Yh/29a7M6wiA
SowcyUWVs20svWNmHJUNyXPpK9eX4+33HshIoftq7xK9drqdp3HjyGu+uWKOKFPJ
TMwy0jJJ6o1iJWW2bzuScdff4vFfGPFfDiy3igQHy2FXj+7k+e0ylC4EbF9qQ+aN
ytkiFQSwgQKBgDuLL64rHud9PhaZQVBsNETn1u81DeBXUj9tcS5ZVa8htP9VVm3t
wNyNdo5NfWt6yHSwFo5UVHSXsp14guv/MVrEXfDJI0rO+yvtZeSW+n6K4MJYyfV8
vPQNCVbr7eb1iqVBPm/5Qn4Sf7PH2UTKEJjYfbDS82PD8mvKzI8hgu41AoGAM4al
fgal9vBU0qPX0OBqx/s7CBmyMuBBTeaU72MrGtvN2eomqx04dfJbqRonw/2f3qa4
DxUuKV3q9SJDbAf8v1Ya48u+5bSvRaDzbU4ZWrq36VBnsvnFJUHDNahRmSt23gOc
nvyyMtyNImEaRD3itzvuPolx6yEXc/WzWwYH9gECgYBVDLdisfzHnRvtqrmkhbRz
uAFgCIvMKL2Bkc8y+XMhC62fIKBJsy2Qs6JhAoPLIxhxaEe4lc9Eo9WgNgwPVIhj
QxZlHcL84b9S3qBE8o3sVZzcXqfO9vuGsnQ713rl8ImQwK5d2hHz19Glg8twkzoo
0HtnrWaOqIFuw2UG6rEqXA==
-----END PRIVATE KEY-----";

    /// The P-256 fixture's `x` coordinate.
    const EC_X: &str = "6fBmOjutLqDpfeTtHQqhBAZ1e3j5mw9PoE7FXjF-Z74";

    /// The P-256 fixture's `y` coordinate.
    const EC_Y: &str = "FkOdyf4JvADCIOFAK6fcr_X9hLNXyG6qUa-yUT7YDrQ";

    /// The P-256 fixture's PKCS#8 private key (`openssl genpkey -algorithm EC`, prime256v1).
    const EC_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg3zfHAnPRQFo3vTIz
pUUqpqti2ixnbrW5gw+lJTI4qoehRANCAATp8GY6O60uoOl95O0dCqEEBnV7ePmb
D0+gTsVeMX5nvhZDncn+CbwAwiDhQCun3K/1/YSzV8huqlGvslE+2A60
-----END PRIVATE KEY-----";

    /// The JWKS body the RSA fixture "publishes".
    pub(crate) fn rsa_jwks() -> Value {
        json!({
            "keys": [{
                "kty": "RSA",
                "kid": RSA_KID,
                "use": "sig",
                "alg": "RS256",
                "n": RSA_N,
                "e": RSA_E,
            }]
        })
    }

    /// The JWKS body the EC fixture "publishes".
    pub(crate) fn es256_jwks() -> Value {
        json!({
            "keys": [{
                "kty": "EC",
                "kid": EC_KID,
                "use": "sig",
                "alg": "ES256",
                "crv": "P-256",
                "x": EC_X,
                "y": EC_Y,
            }]
        })
    }

    /// The DER body of a PEM block: the armor lines and every whitespace character removed, then
    /// base64-decoded. `ring` parses DER, and the fixture PEMs are what `openssl` writes.
    fn pem_to_der(pem: &str) -> Vec<u8> {
        let body: String = pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .flat_map(str::chars)
            .filter(|character| !character.is_whitespace())
            .collect();
        base64url_decode(&body).expect("fixture pem body is base64")
    }

    /// Sign `claims` as an RS256 compact JWS stamped with `kid`.
    pub(crate) fn sign_rs256(kid: &str, claims: &Value) -> String {
        sign_rs256_with_header(&json!({"alg": "RS256", "kid": kid, "typ": "JWT"}), claims)
    }

    /// Sign `claims` as an RS256 compact JWS with a caller-chosen header, so a test can leave the
    /// `kid` out or name a key the JWKS does not publish.
    pub(crate) fn sign_rs256_with_header(header: &Value, claims: &Value) -> String {
        let signing_input = signing_input(header, claims);
        let key =
            RsaKeyPair::from_pkcs8(&pem_to_der(RSA_PRIVATE_KEY_PEM)).expect("fixture rsa key");
        let mut signature = vec![0u8; key.public().modulus_len()];
        key.sign(
            &signature::RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .expect("rs256 signature");
        format!("{signing_input}.{}", base64url_encode(&signature))
    }

    /// Sign `claims` as an ES256 compact JWS stamped with the EC fixture's `kid`.
    pub(crate) fn sign_es256(claims: &Value) -> String {
        let header = json!({"alg": "ES256", "kid": EC_KID, "typ": "JWT"});
        let signing_input = signing_input(&header, claims);
        let rng = SystemRandom::new();
        let key = EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &pem_to_der(EC_PRIVATE_KEY_PEM),
            &rng,
        )
        .expect("fixture ec key");
        let signature = key
            .sign(&rng, signing_input.as_bytes())
            .expect("es256 signature");
        format!("{signing_input}.{}", base64url_encode(signature.as_ref()))
    }

    /// `base64url(header) || "." || base64url(claims)`.
    fn signing_input(header: &Value, claims: &Value) -> String {
        format!(
            "{}.{}",
            base64url_encode(header.to_string().as_bytes()),
            base64url_encode(claims.to_string().as_bytes())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{
        RSA_KID, es256_jwks, rsa_jwks, sign_es256, sign_rs256, sign_rs256_with_header,
    };
    use super::*;
    use crate::transport::{HttpMethod, HttpResponse, Transport, TransportError};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Arc;

    /// Serves one scripted JWKS body and records every request it is asked to perform.
    struct MockTransport {
        body: String,
        status: u16,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl MockTransport {
        fn new(body: Value) -> Arc<Self> {
            Self::with_body(body.to_string())
        }

        fn with_body(body: String) -> Arc<Self> {
            Arc::new(Self {
                body,
                status: 200,
                requests: Mutex::new(Vec::new()),
            })
        }

        fn with_status(status: u16, body: &str) -> Arc<Self> {
            Arc::new(Self {
                status,
                body: body.to_string(),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            match self.requests.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            match self.requests.lock() {
                Ok(mut guard) => guard.push(request),
                Err(poisoned) => poisoned.into_inner().push(request),
            }
            Ok(HttpResponse {
                status: self.status,
                body: self.body.clone(),
                retry_after: None,
            })
        }
    }

    const JWKS_URL: &str = "https://keys.example.test/jwks.json";
    const ISSUER: &str = "https://issuer.example.test";
    const AUDIENCE: &str = "aud-1";

    /// A verifier over a transport serving `jwks`, returned with that transport.
    fn verifier_with(jwks: Value) -> (JwtVerifier, Arc<MockTransport>) {
        let transport = MockTransport::new(jwks);
        (
            JwtVerifier::new(Arc::clone(&transport) as SharedTransport),
            transport,
        )
    }

    /// Claims that pass every rule, good for an hour.
    fn good_claims() -> Value {
        json!({
            "iss": ISSUER,
            "sub": "subject-1",
            "aud": AUDIENCE,
            "exp": now_unix() + 3600,
            "iat": now_unix(),
        })
    }

    async fn verify_with(verifier: &JwtVerifier, token: &str) -> Result<Claims, String> {
        verifier.verify(token, JWKS_URL, ISSUER, AUDIENCE).await
    }

    // -- the RFC 7515 vectors ----------------------------------------------------------------

    /// RFC 7515 Appendix A.3's compact JWS and the public key it was signed with.
    const RFC_HEADER: &str = "eyJhbGciOiJFUzI1NiJ9";
    const RFC_PAYLOAD: &str = "eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ";
    const RFC_SIGNATURE: &str =
        "DtEhU3ljbEg8L38VWAfUAqOyKAM6-Xx-F4GawxaepmXFCgfTjDxw5djxLa8ISlSApmWQxfKTUJqPP3-Kg6NU1Q";
    const RFC_JWK_X: &str = "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU";
    const RFC_JWK_Y: &str = "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0";

    fn rfc7515_jwks() -> Value {
        json!({
            "keys": [{
                "kty": "EC",
                "kid": "rfc-7515-a3",
                "crv": "P-256",
                "x": RFC_JWK_X,
                "y": RFC_JWK_Y,
            }]
        })
    }

    #[test]
    fn rfc7515_a3_es256_signature_verifies_against_its_published_jwk() {
        let keys = vec![Jwk::Ec {
            kid: Some("rfc-7515-a3".to_string()),
            x: base64url_decode(RFC_JWK_X).expect("x"),
            y: base64url_decode(RFC_JWK_Y).expect("y"),
        }];
        let signature = base64url_decode(RFC_SIGNATURE).expect("signature");
        assert_eq!(signature.len(), 64, "an ES256 signature is r || s");

        // The vector's header really is `{"alg":"ES256"}` and the payload decodes as the RFC shows.
        assert_eq!(
            decode_json_part(RFC_HEADER, "header").expect("header")["alg"],
            json!("ES256")
        );
        let payload = decode_json_part(RFC_PAYLOAD, "payload").expect("payload");
        assert_eq!(payload["iss"], json!("joe"));
        assert_eq!(payload["exp"], json!(1_300_819_380u64));

        let signing_input = format!("{RFC_HEADER}.{RFC_PAYLOAD}");
        assert_eq!(
            verify_signature(
                &keys,
                Some("rfc-7515-a3"),
                ALG_ES256,
                signing_input.as_bytes(),
                &signature
            ),
            Ok(())
        );
        // The same signature over a different message must not verify.
        assert!(
            verify_signature(
                &keys,
                Some("rfc-7515-a3"),
                ALG_ES256,
                b"eyJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJqb2UifQ",
                &signature
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn rfc7515_a3_token_is_refused_by_the_claim_rules_rather_than_the_signature() {
        let (verifier, _) = verifier_with(rfc7515_jwks());
        let token = format!("{RFC_HEADER}.{RFC_PAYLOAD}.{RFC_SIGNATURE}");

        // The RFC's token carries `iss: "joe"`, `exp: 1300819380` (2011) and **no `aud`**, so it can
        // never pass a real platform's claim rules. Verification must get that far — i.e. reject the
        // claims, not the signature — which is exactly what proves the signature check succeeded.
        let error = verifier
            .verify(&token, JWKS_URL, "joe", "anything")
            .await
            .expect_err("an expired token must be rejected");
        assert!(error.starts_with("expired:"), "{error}");

        // With the expiry ignored by moving "now" back, the missing audience is what bites.
        let claims =
            Claims::from_payload(&decode_json_part(RFC_PAYLOAD, "payload").expect("payload"))
                .expect("claims");
        let error = validate_claims(&claims, "joe", "anything", 1_300_819_380)
            .expect_err("the RFC token carries no aud");
        assert!(error.starts_with("wrong audience:"), "{error}");
        assert!(claims.audience.is_empty());
    }

    // -- happy paths -------------------------------------------------------------------------

    #[tokio::test]
    async fn a_token_signed_with_a_local_rs256_key_verifies_end_to_end() {
        let (verifier, transport) = verifier_with(rsa_jwks());
        let token = sign_rs256(RSA_KID, &good_claims());

        let claims = verify_with(&verifier, &token).await.expect("valid token");
        assert_eq!(claims.issuer, ISSUER);
        assert_eq!(claims.subject.as_deref(), Some("subject-1"));
        assert_eq!(claims.audience, vec![AUDIENCE.to_string()]);
        assert!(claims.expires_at > now_unix());
        assert_eq!(claims.raw["sub"], json!("subject-1"));

        // The JWKS was fetched with a plain GET to the caller's URL.
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[0].url, JWKS_URL);
        assert!(requests[0].body.is_none());
    }

    #[tokio::test]
    async fn a_token_signed_with_a_local_es256_key_verifies_end_to_end() {
        let (verifier, _) = verifier_with(es256_jwks());
        let token = sign_es256(&good_claims());
        let claims = verify_with(&verifier, &token).await.expect("valid token");
        assert_eq!(claims.issuer, ISSUER);
        assert_eq!(claims.audience, vec![AUDIENCE.to_string()]);

        // The signature is checked with the derived P-256 point, so a corrupted r||s must fail.
        let (header, payload, _) = split_for_test(&token);
        let zero_signature = base64url_encode(&[0u8; 64]);
        let forged = format!("{header}.{payload}.{zero_signature}");
        let error = verify_with(&verifier, &forged).await.expect_err("forged");
        assert!(error.starts_with("bad signature:"), "{error}");
    }

    #[tokio::test]
    async fn a_jwks_is_fetched_once_and_then_served_from_the_cache() {
        let (verifier, transport) = verifier_with(rsa_jwks());
        let token = sign_rs256(RSA_KID, &good_claims());
        for _ in 0..3 {
            verify_with(&verifier, &token).await.expect("valid token");
        }
        assert_eq!(
            transport.requests().len(),
            1,
            "the JWKS is cached for an hour"
        );
    }

    // -- malformed shapes --------------------------------------------------------------------

    #[tokio::test]
    async fn malformed_tokens_are_rejected_by_shape() {
        let (verifier, transport) = verifier_with(rsa_jwks());
        let valid = sign_rs256(RSA_KID, &good_claims());
        let (header, payload, signature) = split_for_test(&valid);

        let cases: Vec<(String, &str)> = vec![
            (
                String::new(),
                "malformed token: expected 3 dot-separated parts",
            ),
            (
                "onlyonesegment".to_string(),
                "expected 3 dot-separated parts",
            ),
            (
                format!("{header}.{payload}"),
                "expected 3 dot-separated parts",
            ),
            (
                format!("{header}.{payload}.{signature}.extra"),
                "expected 3 dot-separated parts",
            ),
            (
                format!("not base64!.{payload}.{signature}"),
                "header is not valid base64url",
            ),
            (
                format!("{header}.bm90IGpzb24.{signature}"),
                "payload is not valid JSON",
            ),
            (
                // `[1,2]` — valid base64url and valid JSON, but not an object.
                format!("{header}.WzEsMl0.{signature}"),
                "payload is not a JSON object",
            ),
            (
                // `{"noalg":true}`
                format!(
                    "{}.{payload}.{signature}",
                    base64url_encode(br#"{"noalg":true}"#)
                ),
                "header has no string alg",
            ),
            (
                format!("{header}.{payload}."),
                "signature is not valid base64url",
            ),
        ];
        for (token, expected) in cases {
            let error = verify_with(&verifier, &token)
                .await
                .expect_err("a malformed token must be rejected");
            assert!(
                error.contains(expected),
                "token {token:?} gave {error:?}, wanted {expected:?}"
            );
        }
        // None of those reached the JWKS.
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn alg_none_is_never_accepted() {
        let (verifier, transport) = verifier_with(rsa_jwks());
        let claims = good_claims();
        let unsigned = format!(
            "{}.{}.",
            base64url_encode(br#"{"alg":"none"}"#),
            base64url_encode(claims.to_string().as_bytes())
        );
        let error = verify_with(&verifier, &unsigned)
            .await
            .expect_err("an unsigned token must be rejected");
        assert!(error.contains("unsupported alg: none"), "{error}");

        // The same for a symmetric algorithm and an unknown one; no JWKS is fetched for either.
        let hs256 = format!(
            "{}.{}.{}",
            base64url_encode(br#"{"alg":"HS256"}"#),
            base64url_encode(claims.to_string().as_bytes()),
            base64url_encode(b"whatever"),
        );
        let error = verify_with(&verifier, &hs256)
            .await
            .expect_err("HS256 must be rejected");
        assert!(error.starts_with("unsupported alg: HS256"), "{error}");

        let ps256 = format!(
            "{}.{}.{}",
            base64url_encode(br#"{"alg":"PS256"}"#),
            base64url_encode(claims.to_string().as_bytes()),
            base64url_encode(b"whatever"),
        );
        let error = verify_with(&verifier, &ps256)
            .await
            .expect_err("PS256 must be rejected");
        assert!(error.starts_with("unsupported alg: PS256"), "{error}");
        assert!(transport.requests().is_empty());
    }

    // -- key selection and signatures --------------------------------------------------------

    #[tokio::test]
    async fn an_unknown_kid_is_reported_as_no_key() {
        let (verifier, _) = verifier_with(rsa_jwks());
        let token = sign_rs256("some-other-key", &good_claims());
        let error = verify_with(&verifier, &token)
            .await
            .expect_err("a kid the JWKS does not publish must fail");
        assert!(error.starts_with("no key:"), "{error}");
        assert!(error.contains("some-other-key"), "{error}");
    }

    #[tokio::test]
    async fn a_key_of_the_wrong_type_is_reported_as_no_key() {
        // A kid-less RS256 token against a JWKS that only publishes an EC key: every key is tried,
        // none is usable for the algorithm, and that is "no key", not "bad signature".
        let (verifier, _) = verifier_with(es256_jwks());
        let token = sign_rs256_with_header(&json!({"alg": "RS256"}), &good_claims());
        let error = verify_with(&verifier, &token)
            .await
            .expect_err("an RSA token needs an RSA key");
        assert!(error.starts_with("no key:"), "{error}");
        assert!(error.contains("no RS256 key"), "{error}");
    }

    #[tokio::test]
    async fn a_token_without_a_kid_is_tried_against_every_key_of_the_right_type() {
        let mut jwks = rsa_jwks();
        // A decoy first key: right type, wrong material. The second key is the real one.
        jwks["keys"].as_array_mut().expect("keys").insert(
            0,
            json!({"kty": "RSA", "kid": "decoy", "n": RSA_DECOY_N, "e": "AQAB"}),
        );
        let (verifier, _) = verifier_with(jwks);
        let token = sign_rs256_with_header(&json!({"alg": "RS256"}), &good_claims());
        let claims = verify_with(&verifier, &token)
            .await
            .expect("a kid-less token must find the right key");
        assert_eq!(claims.issuer, ISSUER);
    }

    #[tokio::test]
    async fn a_tampered_token_is_reported_as_bad_signature() {
        let (verifier, _) = verifier_with(rsa_jwks());
        let token = sign_rs256(RSA_KID, &good_claims());
        let (header, payload, signature) = split_for_test(&token);

        // A different payload under the same signature.
        let mut tampered = good_claims();
        tampered["sub"] = json!("attacker");
        let reencoded = format!(
            "{header}.{}.{signature}",
            base64url_encode(tampered.to_string().as_bytes())
        );
        let error = verify_with(&verifier, &reencoded)
            .await
            .expect_err("a tampered payload must fail");
        assert!(error.starts_with("bad signature:"), "{error}");

        // A flipped signature byte.
        let mut bytes = base64url_decode(signature).expect("signature");
        if let Some(last) = bytes.last_mut() {
            *last ^= 0x01;
        }
        let flipped = format!("{header}.{payload}.{}", base64url_encode(&bytes));
        let error = verify_with(&verifier, &flipped)
            .await
            .expect_err("a tampered signature must fail");
        assert!(error.starts_with("bad signature:"), "{error}");
    }

    // -- the claim rules ---------------------------------------------------------------------

    fn claims_for(audience: Value, expires_at: u64) -> Claims {
        Claims {
            issuer: ISSUER.to_string(),
            subject: None,
            audience: parse_audience(Some(&audience)),
            expires_at,
            raw: json!({}),
        }
    }

    #[test]
    fn expiry_is_enforced_by_the_pure_claim_rules() {
        let now = 1_700_000_000u64;
        let claims = claims_for(json!(AUDIENCE), now);
        assert_eq!(validate_claims(&claims, ISSUER, AUDIENCE, now), Ok(()));
        // The 60-second allowance covers a host clock that runs fast …
        assert_eq!(
            validate_claims(&claims, ISSUER, AUDIENCE, now + EXPIRY_LEEWAY_SECS),
            Ok(())
        );
        // … but one second past it the token is expired.
        let error = validate_claims(&claims, ISSUER, AUDIENCE, now + EXPIRY_LEEWAY_SECS + 1)
            .expect_err("expired");
        assert!(error.starts_with("expired:"), "{error}");

        // An `exp` far in the future cannot overflow the allowance.
        let forever = claims_for(json!(AUDIENCE), u64::MAX);
        assert_eq!(validate_claims(&forever, ISSUER, AUDIENCE, now), Ok(()));
    }

    #[tokio::test]
    async fn a_token_without_exp_or_iss_is_rejected_as_malformed_claims() {
        let (verifier, _) = verifier_with(rsa_jwks());

        let mut no_exp = good_claims();
        no_exp.as_object_mut().expect("object").remove("exp");
        let error = verify_with(&verifier, &sign_rs256(RSA_KID, &no_exp))
            .await
            .expect_err("a token without exp must fail");
        assert!(error.contains("no numeric exp claim"), "{error}");

        let mut stringified_exp = good_claims();
        stringified_exp["exp"] = json!("1900000000");
        let error = verify_with(&verifier, &sign_rs256(RSA_KID, &stringified_exp))
            .await
            .expect_err("a string exp is not a NumericDate");
        assert!(error.contains("no numeric exp claim"), "{error}");

        let mut no_iss = good_claims();
        no_iss.as_object_mut().expect("object").remove("iss");
        let error = verify_with(&verifier, &sign_rs256(RSA_KID, &no_iss))
            .await
            .expect_err("a token without iss must fail");
        assert!(error.contains("no string iss claim"), "{error}");
    }

    #[tokio::test]
    async fn an_expired_token_is_rejected_end_to_end() {
        let (verifier, _) = verifier_with(rsa_jwks());
        let mut claims = good_claims();
        claims["exp"] = json!(now_unix().saturating_sub(EXPIRY_LEEWAY_SECS + 5));
        let error = verify_with(&verifier, &sign_rs256(RSA_KID, &claims))
            .await
            .expect_err("an expired token must fail");
        assert!(error.starts_with("expired:"), "{error}");
    }

    #[tokio::test]
    async fn the_issuer_must_match_exactly() {
        let (verifier, _) = verifier_with(rsa_jwks());
        let token = sign_rs256(RSA_KID, &good_claims());
        let error = verifier
            .verify(&token, JWKS_URL, "https://evil.example.test", AUDIENCE)
            .await
            .expect_err("a wrong issuer must fail");
        assert!(error.starts_with("wrong issuer:"), "{error}");
        assert!(error.contains("https://evil.example.test"), "{error}");

        // A prefix or a differing case is not the same issuer.
        for wrong in [
            "https://issuer.example",
            "https://issuer.example.test/",
            "HTTPS://ISSUER.EXAMPLE.TEST",
        ] {
            let error = verifier
                .verify(&token, JWKS_URL, wrong, AUDIENCE)
                .await
                .expect_err("only the exact issuer is accepted");
            assert!(error.starts_with("wrong issuer:"), "{wrong}: {error}");
        }
    }

    #[tokio::test]
    async fn the_audience_must_contain_the_expected_value() {
        let (verifier, _) = verifier_with(rsa_jwks());

        // An array audience that holds the expected value — any position — is accepted.
        let mut claims = good_claims();
        claims["aud"] = json!(["other", AUDIENCE]);
        let verified = verify_with(&verifier, &sign_rs256(RSA_KID, &claims))
            .await
            .expect("the audience is present");
        assert_eq!(
            verified.audience,
            vec!["other".to_string(), AUDIENCE.to_string()]
        );

        // A different audience is not.
        let error = verifier
            .verify(
                &sign_rs256(RSA_KID, &good_claims()),
                JWKS_URL,
                ISSUER,
                "aud-2",
            )
            .await
            .expect_err("a wrong audience must fail");
        assert!(error.starts_with("wrong audience:"), "{error}");
        assert!(error.contains("aud-1"), "{error}");

        // Nor is a token that carries no `aud` at all.
        let mut no_aud = good_claims();
        no_aud.as_object_mut().expect("object").remove("aud");
        let error = verify_with(&verifier, &sign_rs256(RSA_KID, &no_aud))
            .await
            .expect_err("a token without aud must fail");
        assert!(error.starts_with("wrong audience:"), "{error}");
    }

    #[tokio::test]
    async fn an_audience_given_as_a_string_is_accepted() {
        let (verifier, _) = verifier_with(rsa_jwks());
        let mut claims = good_claims();
        claims["aud"] = json!(AUDIENCE);

        let verified = verify_with(&verifier, &sign_rs256(RSA_KID, &claims))
            .await
            .expect("a string aud is a one-entry audience");
        assert_eq!(verified.audience, vec![AUDIENCE.to_string()]);
        // The raw claim is left exactly as the platform sent it.
        assert_eq!(verified.raw["aud"], json!(AUDIENCE));
    }

    #[test]
    fn an_empty_expected_issuer_or_audience_fails_closed() {
        let claims = claims_for(json!(""), 1_700_000_000);
        let error = validate_claims(&claims, "", "", 1_700_000_000).expect_err("no expectation");
        assert!(error.starts_with("wrong issuer:"), "{error}");
        let error =
            validate_claims(&claims, ISSUER, "   ", 1_700_000_000).expect_err("blank audience");
        assert!(error.starts_with("wrong audience:"), "{error}");
    }

    // -- the JWKS fetch ----------------------------------------------------------------------

    #[tokio::test]
    async fn jwks_transport_failures_are_named() {
        // A non-2xx response.
        let transport = MockTransport::with_status(500, "boom");
        let verifier = JwtVerifier::new(Arc::clone(&transport) as SharedTransport);
        let token = sign_rs256(RSA_KID, &good_claims());
        let error = verify_with(&verifier, &token)
            .await
            .expect_err("a 500 JWKS must fail");
        assert!(error.starts_with("jwks fetch failed: HTTP 500"), "{error}");

        // A body that is not JSON.
        let transport = MockTransport::with_status(200, "<html>not a jwks</html>");
        let verifier = JwtVerifier::new(Arc::clone(&transport) as SharedTransport);
        let error = verify_with(&verifier, &token)
            .await
            .expect_err("a non-JSON JWKS must fail");
        assert!(error.contains("response is not JSON"), "{error}");

        // JSON without a `keys` array.
        let (verifier, _) = verifier_with(json!({"published": []}));
        let error = verify_with(&verifier, &token)
            .await
            .expect_err("a JWKS without keys must fail");
        assert!(error.contains("published no keys array"), "{error}");

        // A `keys` array with nothing usable, plus an unsupported key type that is skipped.
        let (verifier, _) = verifier_with(json!({
            "keys": [
                {"kty": "oct", "kid": "hmac", "k": "c2VjcmV0"},
                {"kty": "EC", "kid": "p-384", "crv": "P-384", "x": "AA", "y": "AA"},
            ]
        }));
        let error = verify_with(&verifier, &token)
            .await
            .expect_err("no usable key must fail");
        assert!(error.starts_with("no usable keys:"), "{error}");

        // A key of a supported type with a missing component is a damaged JWKS, not a skip.
        let (verifier, _) =
            verifier_with(json!({"keys": [{"kty": "RSA", "kid": "broken", "e": "AQAB"}]}));
        let error = verify_with(&verifier, &token)
            .await
            .expect_err("a damaged RSA key must fail");
        assert!(error.contains("malformed jwks"), "{error}");
        assert!(error.contains("broken"), "{error}");
    }

    #[tokio::test]
    async fn a_failed_fetch_is_not_cached() {
        let transport = MockTransport::with_status(503, "unavailable");
        let verifier = JwtVerifier::new(Arc::clone(&transport) as SharedTransport);
        let token = sign_rs256(RSA_KID, &good_claims());
        for _ in 0..2 {
            assert!(verify_with(&verifier, &token).await.is_err());
        }
        assert_eq!(transport.requests().len(), 2, "nothing was cached");
    }

    // -- the small helpers -------------------------------------------------------------------

    #[test]
    fn base64url_decodes_without_padding_and_rejects_junk() {
        // RFC 4648 test vectors, in the url-safe alphabet.
        assert!(base64url_decode("").is_err());
        assert_eq!(base64url_decode("Zg"), Ok(b"f".to_vec()));
        assert_eq!(base64url_decode("Zm8"), Ok(b"fo".to_vec()));
        assert_eq!(base64url_decode("Zm9v"), Ok(b"foo".to_vec()));
        assert_eq!(base64url_decode("Zm9vYg"), Ok(b"foob".to_vec()));
        assert_eq!(base64url_decode("Zm9vYmE"), Ok(b"fooba".to_vec()));
        assert_eq!(base64url_decode("Zm9vYmFy"), Ok(b"foobar".to_vec()));
        // Padding is tolerated but not required.
        assert_eq!(base64url_decode("Zg=="), Ok(b"f".to_vec()));
        // The url-safe symbols are what JWS uses; the standard ones are accepted as well.
        assert_eq!(base64url_decode("-_8"), Ok(vec![0xFB, 0xFF]));
        assert_eq!(base64url_decode("+/8"), Ok(vec![0xFB, 0xFF]));
        // Length ≡ 1 mod 4 carries no data.
        assert!(base64url_decode("Z").is_err());
        assert!(base64url_decode("Zm9vY").is_err());
        // Characters outside the alphabet.
        assert!(base64url_decode("Zm9v!").is_err());
        assert!(base64url_decode("Zm 9v").is_err());
    }

    #[test]
    fn base64url_encoding_round_trips() {
        // Zero bytes encode to the empty string; decoding an empty part is refused instead, because
        // in a JWS an empty part is a truncated token rather than a zero-length value.
        assert_eq!(base64url_encode(&[]), "");
        assert!(base64url_decode("").is_err());
        for bytes in [
            b"f".to_vec(),
            b"fo".to_vec(),
            b"foo".to_vec(),
            b"foob".to_vec(),
            b"fooba".to_vec(),
            b"foobar".to_vec(),
            vec![0xFF, 0x00, 0xAB, 0xCD, 0xEF],
        ] {
            let encoded = base64url_encode(&bytes);
            assert!(!encoded.contains('='), "no padding: {encoded}");
            assert_eq!(base64url_decode(&encoded), Ok(bytes));
        }
        assert_eq!(base64url_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64url_encode(&[0xFB, 0xFF]), "-_8");
    }

    #[test]
    fn the_bearer_scheme_is_parsed_case_insensitively() {
        assert_eq!(bearer_token("Bearer abc.def.ghi"), Some("abc.def.ghi"));
        assert_eq!(bearer_token("  bearer abc  "), Some("abc"));
        assert_eq!(bearer_token("BEARER abc"), Some("abc"));
        assert_eq!(bearer_token("Bearer    "), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("abc"), None);
        assert_eq!(bearer_token(""), None);
        // A token is never silently lifted out of a longer header.
        assert_eq!(bearer_token("BearerX abc"), None);
    }

    #[test]
    fn numeric_dates_accept_numbers_only() {
        assert_eq!(numeric_date(&json!(1_700_000_000)), Some(1_700_000_000));
        assert_eq!(numeric_date(&json!(1_700_000_000.7)), Some(1_700_000_000));
        assert_eq!(numeric_date(&json!(-1)), None);
        assert_eq!(numeric_date(&json!("1700000000")), None);
        assert_eq!(numeric_date(&json!(null)), None);
        assert_eq!(numeric_date(&json!([1])), None);
    }

    #[test]
    fn audiences_normalize_from_strings_and_arrays() {
        assert_eq!(parse_audience(None), Vec::<String>::new());
        assert_eq!(parse_audience(Some(&json!("a"))), vec!["a".to_string()]);
        assert_eq!(
            parse_audience(Some(&json!(["a", 7, "b", null]))),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(parse_audience(Some(&json!(3))), Vec::<String>::new());
    }

    #[test]
    fn jwks_entries_are_parsed_and_unsupported_types_are_skipped() {
        let rsa = parse_jwk(
            &json!({"kty": "RSA", "kid": "k", "n": "AQAB", "e": "AQAB"}),
            0,
        )
        .expect("rsa")
        .expect("usable");
        assert_eq!(rsa.kid(), Some("k"));
        assert!(rsa.matches_alg(ALG_RS256));
        assert!(!rsa.matches_alg(ALG_ES256));

        // An EC key on another curve is skipped, not an error.
        assert_eq!(
            parse_jwk(
                &json!({"kty": "EC", "crv": "P-384", "x": "AA", "y": "AA"}),
                0
            ),
            Ok(None)
        );
        // So is an unknown key type.
        assert_eq!(parse_jwk(&json!({"kty": "OKP"}), 0), Ok(None));
        // A supported type with junk components is an error naming where it came from.
        let error = parse_jwk(
            &json!({"kty": "EC", "crv": "P-256", "x": "!!", "y": "AA"}),
            2,
        )
        .expect_err("junk x");
        assert!(error.contains("index 2"), "{error}");
    }

    // -- fixtures used above -------------------------------------------------------------------

    /// A second, real RSA-2048 modulus used as the decoy key in the kid-less selection test.
    const RSA_DECOY_N: &str = "vkCMPZKfCZg5TDhQU8z3WJLi-u0WqfqT6cjfGw1MQL0XUonKCgLiRM07Dj6knkDKIwtZjKCiUhrEHYAhkd7yayQP2Mbdj_kUZ6P2CLba85gSyoxeduul3OQ5ZCU1hfAkD4mOG9JJIntU1biAEBZOBtWrdr4jeQ3pzsTVyCJxcrOWMMXNGX6dVZ6jUFNrp6e6wNmKnyEqW2iOB45KdZYN5J9ELMeukzf2TgjEzgtFITw_tfrJ9_w-D921s0AIbRb1-G4-T26sKH45tyssWSpCgK99bFgxunrOoVuGi--YANPU3NAibnfy45AiS964qmoyJMCpVBs7K2jGJX9xWr_MHw";

    /// Split a compact JWS into its three text parts.
    fn split_for_test(token: &str) -> (&str, &str, &str) {
        let mut parts = token.split('.');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(header), Some(payload), Some(signature), None) => (header, payload, signature),
            _ => panic!("token is not a compact JWS: {token}"),
        }
    }
}
