//! `vikram gateway <action>` — the managed surface over the env-driven gateway.
//!
//! `vikram gateway` with no action still just runs every platform the environment configures.
//! The actions here manage `gateway.json` (per-platform on/off toggles) and scaffold credential
//! placeholders into `$VIKRAM_HOME/.env`. A platform with no toggle entry is "auto": it runs
//! exactly when its variables are set, which is the pre-existing behavior.

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;
use serde::{Deserialize, Serialize};
use vikram_gateway::platforms::dingtalk_stream::OPEN_CONNECTION_URL;
use vikram_gateway::platforms::discord::DISCORD_API_BASE;
use vikram_gateway::platforms::feishu::FEISHU_API_BASE;
use vikram_gateway::platforms::line::LINE_API_BASE;
use vikram_gateway::platforms::matrix::MATRIX_API_PREFIX;
use vikram_gateway::platforms::mattermost::MATTERMOST_API_PREFIX;
use vikram_gateway::platforms::qqbot::QQ_TOKEN_URL;
use vikram_gateway::platforms::slack::SLACK_API_BASE;
use vikram_gateway::platforms::sms::TWILIO_API_BASE;
use vikram_gateway::platforms::teams::{BOT_FRAMEWORK_SCOPE, TEAMS_LOGIN_BASE};
use vikram_gateway::platforms::telegram::TELEGRAM_API_BASE;
use vikram_gateway::platforms::wecom::WECOM_WS_URL;
use vikram_gateway::platforms::whatsapp_cloud::{GRAPH_API_BASE, GRAPH_API_VERSION};
use vikram_gateway::platforms::yuanbao::YUANBAO_WS_URL;
use vikram_gateway::transport::{HttpRequest, HttpResponse, ReqwestTransport, SharedTransport};
use vikram_runtime::config::{ensure_home, parse_dotenv, vikram_home};

#[derive(Debug, Subcommand)]
pub enum GatewayAction {
    /// List every supported platform, its toggle state, and the variables it still needs.
    #[command(alias = "status")]
    List,
    /// Switch a platform on. It still needs its credential variables to do anything.
    Enable { platform: String },
    /// Switch a platform off even when its credential variables are set.
    Disable { platform: String },
    /// Append a platform's variables to $VIKRAM_HOME/.env as commented placeholders.
    /// On a terminal, prompts for each value instead; --yes keeps the placeholder-only path.
    Add {
        platform: String,
        /// Write commented placeholders without prompting.
        #[arg(long)]
        yes: bool,
    },
    /// Verify configured platforms' credentials with the cheapest live call each API offers.
    Check {
        /// Check only this platform; omit to check every configured one.
        platform: Option<String>,
    },
}

/// One row of the platform registry: the canonical name `gateway.json` and the CLI share (it
/// matches `GatewayPlatform::name`), the variables that must all be set for the platform to count
/// as configured, the optional ones `add` also scaffolds, and whether the platform only exists
/// behind the webhook ingress listener.
pub struct PlatformSpec {
    pub name: &'static str,
    pub required: &'static [&'static str],
    pub optional: &'static [&'static str],
    pub webhook: bool,
}

const fn spec(
    name: &'static str,
    required: &'static [&'static str],
    optional: &'static [&'static str],
    webhook: bool,
) -> PlatformSpec {
    PlatformSpec {
        name,
        required,
        optional,
        webhook,
    }
}

/// Every platform `gateway::run` can build, in the order it tries them. The gating literals in
/// `run` must match these names.
pub const PLATFORM_SPECS: &[PlatformSpec] = &[
    spec(
        "telegram",
        &["TELEGRAM_BOT_TOKEN"],
        &["TELEGRAM_API_BASE"],
        false,
    ),
    spec(
        "discord",
        &["DISCORD_BOT_TOKEN"],
        &["DISCORD_API_BASE", "DISCORD_GATEWAY_URL"],
        false,
    ),
    spec("signal", &["SIGNAL_HTTP_URL", "SIGNAL_ACCOUNT"], &[], false),
    spec(
        "ntfy",
        &["NTFY_TOPIC"],
        &["NTFY_SERVER_URL", "NTFY_TOKEN"],
        false,
    ),
    spec(
        "weixin",
        &["WEIXIN_ACCOUNT_ID", "WEIXIN_TOKEN"],
        &["WEIXIN_BASE_URL"],
        false,
    ),
    spec("simplex", &["SIMPLEX_WS_URL"], &[], false),
    spec(
        "irc",
        &["IRC_SERVER", "IRC_NICKNAME"],
        &["IRC_PORT", "IRC_CHANNEL"],
        false,
    ),
    spec(
        "matrix",
        &["MATRIX_HOMESERVER", "MATRIX_ACCESS_TOKEN"],
        &[],
        false,
    ),
    spec(
        "mattermost",
        &[
            "MATTERMOST_URL",
            "MATTERMOST_TOKEN",
            "MATTERMOST_CHANNEL_ID",
        ],
        &[],
        false,
    ),
    spec("whatsapp", &["WHATSAPP_BRIDGE_URL"], &[], false),
    spec(
        "wecom",
        &["WECOM_BOT_ID", "WECOM_SECRET"],
        &["WECOM_WEBSOCKET_URL", "WECOM_DEVICE_ID"],
        false,
    ),
    spec(
        "email",
        &["EMAIL_FROM", "EMAIL_SMTP_HOST", "EMAIL_IMAP_HOST"],
        &[
            "EMAIL_TLS",
            "EMAIL_SMTP_PORT",
            "EMAIL_IMAP_PORT",
            "EMAIL_USERNAME",
            "EMAIL_PASSWORD",
            "EMAIL_MAILBOX",
        ],
        false,
    ),
    spec(
        "yuanbao",
        &["YUANBAO_APP_ID", "YUANBAO_APP_SECRET"],
        &["YUANBAO_BOT_ID", "YUANBAO_WS_URL", "YUANBAO_API_DOMAIN"],
        false,
    ),
    spec("buzz", &["BUZZ_RELAY_URL", "BUZZ_PRIVATE_KEY"], &[], false),
    spec(
        "dingtalk",
        &["DINGTALK_CLIENT_ID", "DINGTALK_CLIENT_SECRET"],
        &[],
        false,
    ),
    spec(
        "photon",
        &["PHOTON_SIDECAR_URL", "PHOTON_SIDECAR_TOKEN"],
        &[],
        false,
    ),
    spec(
        "bluebubbles",
        &["BLUEBUBBLES_SERVER_URL", "BLUEBUBBLES_PASSWORD"],
        &[],
        true,
    ),
    spec(
        "whatsapp_cloud",
        &[
            "WHATSAPP_CLOUD_ACCESS_TOKEN",
            "WHATSAPP_CLOUD_PHONE_NUMBER_ID",
            "WHATSAPP_CLOUD_APP_SECRET",
            "WHATSAPP_CLOUD_VERIFY_TOKEN",
        ],
        &[],
        true,
    ),
    spec(
        "sms",
        &[
            "TWILIO_ACCOUNT_SID",
            "TWILIO_AUTH_TOKEN",
            "TWILIO_FROM_NUMBER",
            "TWILIO_WEBHOOK_URL",
        ],
        &[],
        true,
    ),
    spec(
        "feishu",
        &["FEISHU_APP_ID", "FEISHU_APP_SECRET"],
        &["FEISHU_VERIFICATION_TOKEN", "FEISHU_API_BASE"],
        true,
    ),
    spec(
        "line",
        &["LINE_CHANNEL_ACCESS_TOKEN", "LINE_CHANNEL_SECRET"],
        &[],
        true,
    ),
    spec(
        "slack",
        &["SLACK_BOT_TOKEN", "SLACK_SIGNING_SECRET"],
        &["SLACK_API_BASE"],
        true,
    ),
    spec("qqbot", &["QQ_BOT_APP_ID", "QQ_BOT_SECRET"], &[], true),
    spec(
        "raft",
        &["RAFT_BRIDGE_TOKEN"],
        &[
            "RAFT_HOST",
            "RAFT_PORT",
            "RAFT_PATH",
            "RAFT_RUNTIME_SESSION",
        ],
        true,
    ),
    spec(
        "google_chat",
        &["GOOGLE_CHAT_WEBHOOK_URL", "GOOGLE_CHAT_PROJECT_NUMBER"],
        &[],
        true,
    ),
    spec(
        "teams",
        &["TEAMS_TENANT_ID", "TEAMS_CLIENT_ID", "TEAMS_CLIENT_SECRET"],
        &[],
        true,
    ),
    // A2A needs no credential of its own: the route exists whenever the webhook listener does,
    // and refuses POSTs until A2A_TOKEN is set.
    spec("a2a", &[], &["A2A_TOKEN"], true),
];

pub fn platform_specs() -> &'static [PlatformSpec] {
    PLATFORM_SPECS
}

pub fn find_spec(name: &str) -> Option<&'static PlatformSpec> {
    PLATFORM_SPECS.iter().find(|spec| spec.name == name)
}

fn unknown_platform(name: &str) -> String {
    format!(
        "unknown gateway platform '{name}'\nvalid platforms: {}",
        PLATFORM_SPECS
            .iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A gateway credential, resolved the way `Config::api_key` resolves keys: the process
/// environment first, then `$VIKRAM_HOME/.env`. Empty means "not configured".
pub(crate) fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let raw = std::fs::read_to_string(vikram_home().join(".env")).ok()?;
            parse_dotenv(&raw)
                .get(name)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

impl PlatformSpec {
    /// The variables the platform still needs: every required one that is unset, plus
    /// `GATEWAY_WEBHOOK_BIND` for webhook platforms (without it the routes are never built).
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing: Vec<&'static str> = self
            .required
            .iter()
            .copied()
            .filter(|var| env(var).is_none())
            .collect();
        if self.webhook && env("GATEWAY_WEBHOOK_BIND").is_none() {
            missing.push("GATEWAY_WEBHOOK_BIND");
        }
        missing
    }
}

/// The persisted toggles: `{ "platforms": { "<name>": { "enabled": bool } } }`. Only platforms the
/// operator touched appear; absence means "auto".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub platforms: BTreeMap<String, PlatformToggle>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformToggle {
    pub enabled: bool,
}

pub fn config_path() -> PathBuf {
    vikram_home().join("gateway.json")
}

pub fn load() -> Result<GatewayConfig, String> {
    let path = config_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GatewayConfig::default());
        }
        Err(err) => return Err(format!("could not read {}: {err}", path.display())),
    };
    serde_json::from_str(&raw).map_err(|err| format!("could not parse {}: {err}", path.display()))
}

/// What `run` gates on. A corrupt file must not stop the gateway, so it warns and treats every
/// platform as auto.
pub fn load_for_run() -> GatewayConfig {
    match load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("[gateway] {err}; platform toggles ignored");
            GatewayConfig::default()
        }
    }
}

impl GatewayConfig {
    /// `Some(true)`/`Some(false)` only when the operator set the platform explicitly; `None` is
    /// "auto" and follows the environment.
    pub fn toggle(&self, name: &str) -> Option<bool> {
        self.platforms.get(name).map(|toggle| toggle.enabled)
    }

    /// The gating helper `run` applies before building a platform: only an explicit
    /// `enabled: false` keeps it out; absence is auto and behaves exactly as before.
    pub fn is_disabled(&self, name: &str) -> bool {
        self.toggle(name) == Some(false)
    }

    fn set(&mut self, name: &str, enabled: bool) {
        self.platforms
            .insert(name.to_string(), PlatformToggle { enabled });
    }

    pub(crate) fn save(&self) -> Result<(), String> {
        ensure_home().map_err(|err| err.to_string())?;
        let path = config_path();
        let body = serde_json::to_string_pretty(self).expect("gateway config is serializable");
        std::fs::write(&path, format!("{body}\n"))
            .map_err(|err| format!("could not write {}: {err}", path.display()))
    }
}

pub fn run(action: GatewayAction) -> Result<(), String> {
    match action {
        GatewayAction::List => list(),
        GatewayAction::Enable { platform } => set_flag(&platform, true),
        GatewayAction::Disable { platform } => set_flag(&platform, false),
        GatewayAction::Add { platform, yes } => add(&platform, yes),
        GatewayAction::Check { platform } => check(platform.as_deref()),
    }
}

fn list() -> Result<(), String> {
    let config = load()?;
    println!(
        "{:<16} {:<8} {:<11} MISSING",
        "PLATFORM", "STATE", "CONFIGURED"
    );
    for spec in platform_specs() {
        let state = match config.toggle(spec.name) {
            Some(true) => "enabled",
            Some(false) => "disabled",
            None => "auto",
        };
        let missing = spec.missing();
        let configured = if missing.is_empty() { "yes" } else { "no" };
        let detail = if missing.is_empty() {
            "-".to_string()
        } else {
            missing.join(", ")
        };
        println!(
            "{:<16} {:<8} {:<11} {}",
            spec.name, state, configured, detail
        );
    }
    println!("toggles: {}", config_path().display());
    Ok(())
}

fn set_flag(name: &str, enabled: bool) -> Result<(), String> {
    let spec = find_spec(name).ok_or_else(|| unknown_platform(name))?;
    let mut config = load()?;
    config.set(spec.name, enabled);
    config.save()?;
    println!(
        "{} {}",
        if enabled { "enabled" } else { "disabled" },
        spec.name
    );
    if enabled {
        let missing = spec.missing();
        if !missing.is_empty() {
            println!(
                "note: {} is not configured yet; missing {}",
                spec.name,
                missing.join(", ")
            );
        }
    }
    Ok(())
}

/// Keys already present in a dotenv file, commented placeholders included, so `add` is idempotent.
fn dotenv_keys(raw: &str) -> BTreeSet<String> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches('#').trim();
            let line = line.strip_prefix("export ").unwrap_or(line).trim();
            line.split_once('=')
                .map(|(key, _)| key.trim().to_string())
                .filter(|key| !key.is_empty())
        })
        .collect()
}

fn add(name: &str, yes: bool) -> Result<(), String> {
    let spec = find_spec(name).ok_or_else(|| unknown_platform(name))?;
    ensure_home().map_err(|err| err.to_string())?;
    let path = vikram_home().join(".env");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let present = dotenv_keys(&existing);
    let mut added = Vec::new();
    let mut kept = Vec::new();
    for var in spec.required.iter().chain(spec.optional.iter()) {
        if present.contains(*var) || env(var).is_some() {
            kept.push(*var);
        } else {
            added.push(*var);
        }
    }
    if added.is_empty() {
        println!(
            "{}: every variable is already set; nothing to add",
            spec.name
        );
        if !kept.is_empty() {
            println!("present: {}", kept.join(", "));
        }
        return Ok(());
    }
    let interactive = !yes && std::io::stdout().is_terminal();
    let values = if interactive {
        collect_values(spec, &added, tty_ask)?
    } else {
        BTreeMap::new()
    };
    write_scaffold(&path, &existing, spec, &added, &values)?;
    println!("scaffolded {} into {}", spec.name, path.display());
    if interactive {
        for (var, value) in &values {
            println!("  {var}={}", mask_hint(value));
        }
        for var in added.iter().filter(|var| !values.contains_key(**var)) {
            println!("  # {var}=");
        }
        println!(
            "fill any remaining '# KEY=' lines, then `vikram gateway enable {}` if you want it running",
            spec.name
        );
    } else {
        for var in &added {
            println!("  # {var}=");
        }
        println!("uncomment and fill each line, then `vikram gateway list` shows what is missing");
    }
    if spec.webhook {
        println!("webhook platform: also set GATEWAY_WEBHOOK_BIND=<bind address> for the routes");
    }
    if !kept.is_empty() {
        println!("already present, left untouched: {}", kept.join(", "));
    }
    Ok(())
}

/// Values collected on the terminal, keyed by variable name. Required variables loop until a
/// non-empty answer; optional ones accept an empty answer and are left as placeholders.
fn collect_values(
    spec: &PlatformSpec,
    vars: &[&'static str],
    mut ask: impl FnMut(&str) -> Result<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    for var in vars {
        let required = spec.required.contains(var);
        let prompt = if required {
            format!("{var} (required): ")
        } else {
            format!("{var} (optional, empty to skip): ")
        };
        loop {
            let value = ask(&prompt)?.trim().to_string();
            if value.is_empty() {
                if required {
                    eprintln!("{var} is required; try again");
                    continue;
                }
                break;
            }
            values.insert((*var).to_string(), value);
            break;
        }
    }
    Ok(values)
}

/// One prompted line on stderr (stdout stays script-clean). There is no password-prompt helper
/// in the workspace, so values echo while typing; EOF aborts instead of hanging.
fn tty_ask(prompt: &str) -> Result<String, String> {
    use std::io::Write;
    eprint!("{prompt}");
    std::io::stderr().flush().map_err(|err| err.to_string())?;
    let mut line = String::new();
    let read = std::io::stdin()
        .read_line(&mut line)
        .map_err(|err| format!("cannot read the answer: {err}"))?;
    if read == 0 {
        return Err("aborted: end of input".to_string());
    }
    Ok(line)
}

/// Print a collected value without leaking it back to the terminal scrollback.
fn mask_hint(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 4 {
        "****".to_string()
    } else {
        let head: String = chars[..2].iter().collect();
        let tail: String = chars[chars.len() - 2..].iter().collect();
        format!("{head}…{tail}")
    }
}

/// `KEY=value` when the value needs no quoting, `KEY="value"` when it carries edge whitespace —
/// `parse_dotenv` trims unquoted values and strips one matched quote pair.
fn dotenv_value(value: &str) -> String {
    if !value.is_empty() && value.trim() == value {
        value.to_string()
    } else {
        format!("\"{value}\"")
    }
}

/// Append the `# platform` block to the .env file: real `KEY=value` lines for collected values,
/// `# KEY=` placeholders for everything else.
fn write_scaffold(
    path: &std::path::Path,
    existing: &str,
    spec: &PlatformSpec,
    added: &[&'static str],
    values: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut block = format!("# {}\n", spec.name);
    for var in added {
        match values.get(*var) {
            Some(value) => block.push_str(&format!("{var}={}\n", dotenv_value(value))),
            None => block.push_str(&format!("# {var}=\n")),
        }
    }
    let mut body = existing.to_string();
    if !body.is_empty() {
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push('\n');
    }
    body.push_str(&block);
    std::fs::write(path, &body)
        .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `vikram gateway check` — live credential verification.
//
// Every probe reuses the endpoint and auth scheme its adapter in
// `vikram-gateway/src/platforms/` already speaks. A probe either authenticates
// (2xx plus the response shape the API documents) or only proves reachability;
// nothing reports success it did not earn.
// ---------------------------------------------------------------------------

/// How long any single check call may take before it counts as unreachable.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyStatus {
    /// The credentialed endpoint accepted the configuration.
    Ok,
    /// The server answered, but the probe carries no credential — reachability only.
    Reachable,
    /// The endpoint answered and refused the credentials or the request.
    Failed,
    /// Nothing answered: DNS, TCP, TLS or the timeout fired.
    Unreachable,
    /// No cheap read-only verification exists for this platform.
    Unverified,
}

impl VerifyStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Reachable => "reachable",
            Self::Failed => "failed",
            Self::Unreachable => "unreachable",
            Self::Unverified => "unverified",
        }
    }
}

#[derive(Debug)]
pub struct VerifyResult {
    pub status: VerifyStatus,
    pub detail: String,
}

/// What a 2xx body must additionally prove for an authenticated probe.
#[derive(Clone, Copy)]
enum Expect {
    /// Any 2xx is proof (REST identity endpoints).
    Status,
    /// JSON `ok: true` (Telegram Bot API, Slack Web API).
    OkFlag,
    /// JSON `code: 0` (Feishu-style envelopes).
    CodeZero,
    /// A non-empty `access_token` or `tenant_access_token` string.
    AccessToken,
    /// Every listed top-level JSON field present and non-null.
    Fields(&'static [&'static str]),
}

/// One unauthenticated reachability target.
#[derive(Debug)]
enum ProbeTarget {
    /// `GET` the URL; any 2xx counts as reachable.
    Http(String),
    /// A bare TCP connect to `host:port`.
    Tcp { host: String, port: u16 },
}

enum VerifyPlan {
    /// Carries the reason `check` prints verbatim.
    Unverified(&'static str),
    /// One authenticated request built from the environment at check time.
    Api {
        request: fn() -> Option<HttpRequest>,
        expect: Expect,
    },
    /// Unauthenticated probes; every target must answer to count as reachable.
    Probe {
        targets: fn() -> Option<Vec<ProbeTarget>>,
    },
}

fn api_base(var: &str, default: &str) -> String {
    env(var)
        .unwrap_or_else(|| default.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn trim_slash(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

/// Percent-encode one URI component (unreserved kept, rest %XX) — the same shape the
/// gateway's own `encode_component` helpers use for query values.
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Standard padded base64 (RFC 4648), the same helper shape `chat.rs` and the gateway
/// adapters keep locally — needed for Twilio's HTTP Basic credentials.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b1 = u32::from(chunk[0]);
        let b2 = chunk.get(1).copied().map_or(0, u32::from);
        let b3 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b1 << 16) | (b2 << 8) | b3;
        let digit = |shift: u32| ALPHABET[(triple >> shift) as usize & 0x3f] as char;
        out.push(digit(18));
        out.push(digit(12));
        out.push(if chunk.len() > 1 { digit(6) } else { '=' });
        out.push(if chunk.len() > 2 { digit(0) } else { '=' });
    }
    out
}

fn telegram_request() -> Option<HttpRequest> {
    let token = env("TELEGRAM_BOT_TOKEN")?;
    let base = api_base("TELEGRAM_API_BASE", TELEGRAM_API_BASE);
    Some(HttpRequest::get(format!("{base}/bot{token}/getMe")))
}

fn discord_request() -> Option<HttpRequest> {
    let token = env("DISCORD_BOT_TOKEN")?;
    let base = api_base("DISCORD_API_BASE", DISCORD_API_BASE);
    Some(
        HttpRequest::get(format!("{base}/users/@me"))
            .header("Authorization", format!("Bot {token}")),
    )
}

fn matrix_request() -> Option<HttpRequest> {
    let homeserver = env("MATRIX_HOMESERVER")?;
    let token = env("MATRIX_ACCESS_TOKEN")?;
    Some(
        HttpRequest::get(format!(
            "{}{MATRIX_API_PREFIX}/account/whoami",
            trim_slash(&homeserver)
        ))
        .bearer(&token),
    )
}

fn mattermost_request() -> Option<HttpRequest> {
    let base = env("MATTERMOST_URL")?;
    let token = env("MATTERMOST_TOKEN")?;
    Some(
        HttpRequest::get(format!(
            "{}{MATTERMOST_API_PREFIX}/users/me",
            trim_slash(&base)
        ))
        .bearer(&token),
    )
}

fn slack_request() -> Option<HttpRequest> {
    let token = env("SLACK_BOT_TOKEN")?;
    let base = api_base("SLACK_API_BASE", SLACK_API_BASE);
    Some(HttpRequest::post(format!("{base}/auth.test")).bearer(&token))
}

fn feishu_request() -> Option<HttpRequest> {
    let app_id = env("FEISHU_APP_ID")?;
    let app_secret = env("FEISHU_APP_SECRET")?;
    let base = api_base("FEISHU_API_BASE", FEISHU_API_BASE);
    Some(HttpRequest::json(
        format!("{base}/auth/v3/tenant_access_token/internal"),
        &serde_json::json!({ "app_id": app_id, "app_secret": app_secret }),
    ))
}

fn line_request() -> Option<HttpRequest> {
    let token = env("LINE_CHANNEL_ACCESS_TOKEN")?;
    Some(HttpRequest::get(format!("{LINE_API_BASE}/v2/bot/info")).bearer(&token))
}

fn teams_request() -> Option<HttpRequest> {
    let tenant = env("TEAMS_TENANT_ID")?;
    let client_id = env("TEAMS_CLIENT_ID")?;
    let client_secret = env("TEAMS_CLIENT_SECRET")?;
    let login = api_base("TEAMS_LOGIN_BASE", TEAMS_LOGIN_BASE);
    Some(HttpRequest::form(
        format!("{login}/{}/oauth2/v2.0/token", encode_component(&tenant)),
        &[
            ("grant_type", "client_credentials".to_string()),
            ("scope", BOT_FRAMEWORK_SCOPE.to_string()),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ],
    ))
}

fn qqbot_request() -> Option<HttpRequest> {
    let app_id = env("QQ_BOT_APP_ID")?;
    let secret = env("QQ_BOT_SECRET")?;
    Some(HttpRequest::json(
        QQ_TOKEN_URL,
        &serde_json::json!({ "appId": app_id, "clientSecret": secret }),
    ))
}

fn sms_request() -> Option<HttpRequest> {
    let sid = env("TWILIO_ACCOUNT_SID")?;
    let token = env("TWILIO_AUTH_TOKEN")?;
    let credentials = base64(format!("{sid}:{token}").as_bytes());
    Some(
        HttpRequest::get(format!("{TWILIO_API_BASE}/2010-04-01/Accounts/{sid}.json"))
            .header("Authorization", format!("Basic {credentials}")),
    )
}

fn whatsapp_cloud_request() -> Option<HttpRequest> {
    let token = env("WHATSAPP_CLOUD_ACCESS_TOKEN")?;
    let phone = env("WHATSAPP_CLOUD_PHONE_NUMBER_ID")?;
    Some(
        HttpRequest::get(format!(
            "{}/{GRAPH_API_VERSION}/{phone}",
            trim_slash(GRAPH_API_BASE)
        ))
        .bearer(&token),
    )
}

fn dingtalk_request() -> Option<HttpRequest> {
    let client_id = env("DINGTALK_CLIENT_ID")?;
    let client_secret = env("DINGTALK_CLIENT_SECRET")?;
    Some(HttpRequest::json(
        OPEN_CONNECTION_URL,
        &serde_json::json!({
            "clientId": client_id,
            "clientSecret": client_secret,
            "subscriptions": [],
            "ua": "",
            "localIp": "",
        }),
    ))
}

fn photon_request() -> Option<HttpRequest> {
    let base = env("PHOTON_SIDECAR_URL")?;
    let token = env("PHOTON_SIDECAR_TOKEN")?;
    Some(
        HttpRequest::get(format!("{}/health", trim_slash(&base)))
            .header("X-Hermes-Sidecar-Token", token),
    )
}

fn bluebubbles_request() -> Option<HttpRequest> {
    let server = env("BLUEBUBBLES_SERVER_URL")?;
    let password = env("BLUEBUBBLES_PASSWORD")?;
    let server = server.trim().trim_end_matches('/').to_string();
    let server = if server.starts_with("http://") || server.starts_with("https://") {
        server
    } else {
        format!("http://{server}")
    };
    Some(HttpRequest::get(format!(
        "{server}/api/v1/server/info?password={}",
        encode_component(&password)
    )))
}

fn ntfy_probe() -> Option<Vec<ProbeTarget>> {
    let server = api_base("NTFY_SERVER_URL", "https://ntfy.sh");
    Some(vec![ProbeTarget::Http(format!("{server}/v1/health"))])
}

fn signal_probe() -> Option<Vec<ProbeTarget>> {
    let base = env("SIGNAL_HTTP_URL")?;
    Some(vec![ProbeTarget::Http(format!(
        "{}/v1/about",
        trim_slash(&base)
    ))])
}

fn whatsapp_probe() -> Option<Vec<ProbeTarget>> {
    let base = env("WHATSAPP_BRIDGE_URL")?;
    Some(vec![ProbeTarget::Http(format!(
        "{}/health",
        trim_slash(&base)
    ))])
}

fn email_probe() -> Option<Vec<ProbeTarget>> {
    let smtp_host = env("EMAIL_SMTP_HOST")?;
    let imap_host = env("EMAIL_IMAP_HOST")?;
    let tls = env("EMAIL_TLS")
        .map(|value| value != "false")
        .unwrap_or(true);
    let smtp_port = env("EMAIL_SMTP_PORT")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(if tls { 465 } else { 25 });
    let imap_port = env("EMAIL_IMAP_PORT")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(if tls { 993 } else { 143 });
    Some(vec![
        ProbeTarget::Tcp {
            host: smtp_host,
            port: smtp_port,
        },
        ProbeTarget::Tcp {
            host: imap_host,
            port: imap_port,
        },
    ])
}

fn irc_probe() -> Option<Vec<ProbeTarget>> {
    let server = env("IRC_SERVER")?;
    let port = env("IRC_PORT")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(6667);
    Some(vec![ProbeTarget::Tcp { host: server, port }])
}

/// `scheme://host[:port]` → a TCP target; `wss`/`https` dial 443, `ws`/`http` dial 80.
fn url_tcp_target(url: &str) -> Option<ProbeTarget> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme {
        "wss" | "https" => 443,
        "ws" | "http" => 80,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => (host, port.parse::<u16>().ok()?),
        _ => (authority, default_port),
    };
    if host.is_empty() {
        return None;
    }
    Some(ProbeTarget::Tcp {
        host: host.to_string(),
        port,
    })
}

/// Build a websocket-platform reachability probe: TCP-connect the URL's host on its scheme port.
fn ws_probe(env_var: &str, default: Option<&str>) -> impl Fn() -> Option<Vec<ProbeTarget>> {
    let default = default.map(str::to_string);
    move || {
        let url = env(env_var).or_else(|| default.clone())?;
        url_tcp_target(&url).map(|target| vec![target])
    }
}

fn wecom_probe() -> Option<Vec<ProbeTarget>> {
    ws_probe("WECOM_WEBSOCKET_URL", Some(WECOM_WS_URL))()
}

fn simplex_probe() -> Option<Vec<ProbeTarget>> {
    ws_probe("SIMPLEX_WS_URL", None)()
}

fn buzz_probe() -> Option<Vec<ProbeTarget>> {
    ws_probe("BUZZ_RELAY_URL", None)()
}

fn yuanbao_probe() -> Option<Vec<ProbeTarget>> {
    ws_probe("YUANBAO_WS_URL", Some(YUANBAO_WS_URL))()
}

/// The verify plan for every platform in [`PLATFORM_SPECS`], in the same order. A platform
/// absent here is a bug the tests catch; `Unverified` is a deliberate, explained answer.
const VERIFY_PLANS: &[(&str, VerifyPlan)] = &[
    (
        "telegram",
        VerifyPlan::Api {
            request: telegram_request,
            expect: Expect::OkFlag,
        },
    ),
    (
        "discord",
        VerifyPlan::Api {
            request: discord_request,
            expect: Expect::Status,
        },
    ),
    (
        "signal",
        VerifyPlan::Probe {
            targets: signal_probe,
        },
    ),
    (
        "ntfy",
        VerifyPlan::Probe {
            targets: ntfy_probe,
        },
    ),
    (
        "weixin",
        VerifyPlan::Unverified(
            "the iLink API only answers authenticated POSTs; verify by starting `vikram gateway`",
        ),
    ),
    (
        "simplex",
        VerifyPlan::Probe {
            targets: simplex_probe,
        },
    ),
    ("irc", VerifyPlan::Probe { targets: irc_probe }),
    (
        "matrix",
        VerifyPlan::Api {
            request: matrix_request,
            expect: Expect::Status,
        },
    ),
    (
        "mattermost",
        VerifyPlan::Api {
            request: mattermost_request,
            expect: Expect::Status,
        },
    ),
    (
        "whatsapp",
        VerifyPlan::Probe {
            targets: whatsapp_probe,
        },
    ),
    (
        "wecom",
        VerifyPlan::Probe {
            targets: wecom_probe,
        },
    ),
    (
        "email",
        VerifyPlan::Probe {
            targets: email_probe,
        },
    ),
    (
        "yuanbao",
        VerifyPlan::Probe {
            targets: yuanbao_probe,
        },
    ),
    (
        "buzz",
        VerifyPlan::Probe {
            targets: buzz_probe,
        },
    ),
    (
        "dingtalk",
        VerifyPlan::Api {
            request: dingtalk_request,
            expect: Expect::Fields(&["endpoint", "ticket"]),
        },
    ),
    (
        "photon",
        VerifyPlan::Api {
            request: photon_request,
            expect: Expect::Status,
        },
    ),
    (
        "bluebubbles",
        VerifyPlan::Api {
            request: bluebubbles_request,
            expect: Expect::Status,
        },
    ),
    (
        "whatsapp_cloud",
        VerifyPlan::Api {
            request: whatsapp_cloud_request,
            expect: Expect::Status,
        },
    ),
    (
        "sms",
        VerifyPlan::Api {
            request: sms_request,
            expect: Expect::Status,
        },
    ),
    (
        "feishu",
        VerifyPlan::Api {
            request: feishu_request,
            expect: Expect::CodeZero,
        },
    ),
    (
        "line",
        VerifyPlan::Api {
            request: line_request,
            expect: Expect::Status,
        },
    ),
    (
        "slack",
        VerifyPlan::Api {
            request: slack_request,
            expect: Expect::OkFlag,
        },
    ),
    (
        "qqbot",
        VerifyPlan::Api {
            request: qqbot_request,
            expect: Expect::AccessToken,
        },
    ),
    (
        "raft",
        VerifyPlan::Unverified(
            "wake-only inbound bridge; verify by triggering a wake from the raft CLI",
        ),
    ),
    (
        "google_chat",
        VerifyPlan::Unverified(
            "inbound webhook only; verify by sending a test message to the space",
        ),
    ),
    (
        "teams",
        VerifyPlan::Api {
            request: teams_request,
            expect: Expect::AccessToken,
        },
    ),
    (
        "a2a",
        VerifyPlan::Unverified("no credential of its own; each POST is checked against A2A_TOKEN"),
    ),
];

fn verify_plan(name: &str) -> Option<&'static VerifyPlan> {
    VERIFY_PLANS
        .iter()
        .find(|(platform, _)| *platform == name)
        .map(|(_, plan)| plan)
}

/// A platform counts as configured for `check` when every required variable resolves;
/// `GATEWAY_WEBHOOK_BIND` gates the listener, not the credentials, so it is not required here.
fn credentials_configured(spec: &PlatformSpec) -> bool {
    spec.required.iter().all(|var| env(var).is_some())
}

fn check(platform: Option<&str>) -> Result<(), String> {
    let specs: Vec<&PlatformSpec> = match platform {
        Some(name) => vec![find_spec(name).ok_or_else(|| unknown_platform(name))?],
        None => platform_specs().iter().collect(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("could not start runtime: {err}"))?;
    runtime.block_on(async move {
        let transport = match ReqwestTransport::new() {
            Ok(transport) => transport.into_shared(),
            Err(err) => {
                eprintln!("vikram: could not build the HTTP transport: {err}");
                return;
            }
        };
        println!("{:<16} {:<11} DETAIL", "PLATFORM", "STATUS");
        for spec in specs {
            if !credentials_configured(spec) {
                let missing: Vec<&str> = spec
                    .required
                    .iter()
                    .copied()
                    .filter(|var| env(var).is_none())
                    .collect();
                println!(
                    "{:<16} {:<11} not configured; missing {}",
                    spec.name,
                    "-",
                    missing.join(", ")
                );
                continue;
            }
            let result = verify(spec, &transport).await;
            println!(
                "{:<16} {:<11} {}",
                spec.name,
                result.status.label(),
                result.detail
            );
        }
    });
    Ok(())
}

async fn verify(spec: &PlatformSpec, transport: &SharedTransport) -> VerifyResult {
    let Some(plan) = verify_plan(spec.name) else {
        return VerifyResult {
            status: VerifyStatus::Unverified,
            detail: "no verifier registered".to_string(),
        };
    };
    match plan {
        VerifyPlan::Unverified(reason) => VerifyResult {
            status: VerifyStatus::Unverified,
            detail: (*reason).to_string(),
        },
        VerifyPlan::Api { request, expect } => {
            let Some(request) = request() else {
                return VerifyResult {
                    status: VerifyStatus::Unverified,
                    detail: "credentials incomplete".to_string(),
                };
            };
            match timed_execute(transport, request).await {
                Err(detail) => VerifyResult {
                    status: VerifyStatus::Unreachable,
                    detail,
                },
                Ok(response) => judge(&response, *expect),
            }
        }
        VerifyPlan::Probe { targets } => {
            let Some(targets) = targets() else {
                return VerifyResult {
                    status: VerifyStatus::Unverified,
                    detail: "probe inputs incomplete".to_string(),
                };
            };
            for target in targets {
                if let Err(result) = probe_target(transport, target).await {
                    return result;
                }
            }
            VerifyResult {
                status: VerifyStatus::Reachable,
                detail: "server reachable; this probe does not authenticate".to_string(),
            }
        }
    }
}

async fn timed_execute(
    transport: &SharedTransport,
    request: HttpRequest,
) -> Result<HttpResponse, String> {
    match tokio::time::timeout(VERIFY_TIMEOUT, transport.execute(request)).await {
        Err(_) => Err(format!("no answer within {}s", VERIFY_TIMEOUT.as_secs())),
        Ok(Err(err)) => Err(err.to_string()),
        Ok(Ok(response)) => Ok(response),
    }
}

/// Turn one HTTP answer into ok/failed. Non-2xx is always a failure — 401/403 names rejected
/// credentials, anything else an unexpected answer; nothing here upgrades a refusal to success.
fn judge(response: &HttpResponse, expect: Expect) -> VerifyResult {
    if !response.ok() {
        let detail = if matches!(response.status, 401 | 403) {
            format!("credentials rejected (HTTP {})", response.status)
        } else {
            format!("unexpected response (HTTP {})", response.status)
        };
        return VerifyResult {
            status: VerifyStatus::Failed,
            detail,
        };
    }
    let body = response.json().unwrap_or(serde_json::Value::Null);
    let pass = match expect {
        Expect::Status => true,
        Expect::OkFlag => body.get("ok").and_then(|v| v.as_bool()) == Some(true),
        Expect::CodeZero => body.get("code").and_then(|v| v.as_i64()) == Some(0),
        Expect::AccessToken => ["access_token", "tenant_access_token"].iter().any(|field| {
            body.get(*field)
                .and_then(|v| v.as_str())
                .is_some_and(|token| !token.trim().is_empty())
        }),
        Expect::Fields(fields) => fields
            .iter()
            .all(|field| body.get(*field).is_some_and(|v| !v.is_null())),
    };
    if pass {
        return VerifyResult {
            status: VerifyStatus::Ok,
            detail: "credentials accepted".to_string(),
        };
    }
    let hint = ["error", "errcode", "code", "description"]
        .iter()
        .find_map(|field| body.get(*field))
        .map(|value| value.to_string());
    VerifyResult {
        status: VerifyStatus::Failed,
        detail: format!(
            "endpoint refused the credentials{}",
            hint.map(|h| format!(" ({h})")).unwrap_or_default()
        ),
    }
}

async fn probe_target(
    transport: &SharedTransport,
    target: ProbeTarget,
) -> Result<(), VerifyResult> {
    match target {
        ProbeTarget::Http(url) => match timed_execute(transport, HttpRequest::get(url)).await {
            Ok(response) if response.ok() => Ok(()),
            Ok(response) => Err(VerifyResult {
                status: VerifyStatus::Failed,
                detail: format!("unexpected response (HTTP {})", response.status),
            }),
            Err(detail) => Err(VerifyResult {
                status: VerifyStatus::Unreachable,
                detail,
            }),
        },
        ProbeTarget::Tcp { host, port } => {
            let addr = format!("{host}:{port}");
            match tokio::time::timeout(
                VERIFY_TIMEOUT,
                tokio::net::TcpStream::connect((host.as_str(), port)),
            )
            .await
            {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(err)) => Err(VerifyResult {
                    status: VerifyStatus::Unreachable,
                    detail: format!("{addr}: {err}"),
                }),
                Err(_) => Err(VerifyResult {
                    status: VerifyStatus::Unreachable,
                    detail: format!("{addr}: no answer within {}s", VERIFY_TIMEOUT.as_secs()),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_runtime::config::lock_security_env;

    fn test_home() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let tmp = tempfile::tempdir().unwrap();
        let guard = lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path()) };
        (tmp, guard)
    }

    #[test]
    fn toggles_round_trip_through_gateway_json() {
        let (_tmp, _guard) = test_home();
        let mut config = GatewayConfig::default();
        assert_eq!(config.toggle("telegram"), None);
        config.set("telegram", false);
        config.set("discord", true);
        config.save().unwrap();

        let raw = std::fs::read_to_string(vikram_home().join("gateway.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json["platforms"]["telegram"]["enabled"], false);
        assert_eq!(json["platforms"]["discord"]["enabled"], true);
        assert!(json["platforms"].get("ntfy").is_none());

        let loaded = load().unwrap();
        assert!(loaded.is_disabled("telegram"));
        assert!(!loaded.is_disabled("discord"));
        assert_eq!(loaded.toggle("discord"), Some(true));
    }

    #[test]
    fn enable_and_disable_persist_and_unknown_names_are_refused() {
        let (_tmp, _guard) = test_home();
        set_flag("telegram", false).unwrap();
        assert!(load().unwrap().is_disabled("telegram"));
        set_flag("telegram", true).unwrap();
        assert_eq!(load().unwrap().toggle("telegram"), Some(true));

        let err = set_flag("not-a-platform", true).unwrap_err();
        assert!(err.contains("unknown gateway platform"), "{err}");
        assert!(err.contains("telegram"), "{err}");
        let err = add("not-a-platform", true).unwrap_err();
        assert!(err.contains("unknown gateway platform"), "{err}");
    }

    #[test]
    fn the_gating_helper_only_blocks_explicit_disables() {
        let mut config = GatewayConfig::default();
        // Absent means auto: the platform follows the environment exactly as before.
        assert!(!config.is_disabled("telegram"));
        config.set("telegram", true);
        assert!(!config.is_disabled("telegram"));
        config.set("telegram", false);
        assert!(config.is_disabled("telegram"));
        // An entry for one platform never gates another.
        assert!(!config.is_disabled("discord"));
    }

    #[test]
    fn add_appends_placeholders_without_clobbering_the_env_file() {
        let (_tmp, _guard) = test_home();
        let env_path = vikram_home().join(".env");
        std::fs::write(
            &env_path,
            "KEEP_ME=1\nSIGNAL_HTTP_URL=https://signal.example\n",
        )
        .unwrap();

        add("signal", true).unwrap();
        let body = std::fs::read_to_string(&env_path).unwrap();
        assert!(body.starts_with("KEEP_ME=1\nSIGNAL_HTTP_URL=https://signal.example\n"));
        assert!(body.contains("# signal\n"));
        assert!(body.contains("# SIGNAL_ACCOUNT=\n"));
        // SIGNAL_HTTP_URL is already set, so only the missing variable was scaffolded.
        assert!(!body.contains("# SIGNAL_HTTP_URL="));

        // Running add again must not duplicate the commented placeholders.
        add("signal", true).unwrap();
        let body = std::fs::read_to_string(&env_path).unwrap();
        assert_eq!(body.matches("SIGNAL_ACCOUNT").count(), 1);
    }

    #[test]
    fn specs_cover_every_platform_once() {
        let mut names: Vec<&str> = platform_specs().iter().map(|spec| spec.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            total,
            "duplicate platform names in the registry"
        );
        assert!(platform_specs().len() >= 27);
        assert!(find_spec("a2a").unwrap().webhook);
        assert!(!find_spec("telegram").unwrap().webhook);
    }

    #[test]
    fn every_platform_has_a_verify_plan() {
        for spec in platform_specs() {
            let plan = verify_plan(spec.name)
                .unwrap_or_else(|| panic!("{} has no verify plan", spec.name));
            match plan {
                VerifyPlan::Unverified(reason) => assert!(!reason.is_empty()),
                VerifyPlan::Api { .. } | VerifyPlan::Probe { .. } => {}
            }
        }
        assert_eq!(VERIFY_PLANS.len(), platform_specs().len());
        // Spot-check the honest degradations and the real verifiers.
        assert!(matches!(
            verify_plan("telegram"),
            Some(VerifyPlan::Api {
                expect: Expect::OkFlag,
                ..
            })
        ));
        assert!(matches!(
            verify_plan("google_chat"),
            Some(VerifyPlan::Unverified(_))
        ));
        assert!(matches!(
            verify_plan("a2a"),
            Some(VerifyPlan::Unverified(_))
        ));
        assert!(matches!(
            verify_plan("email"),
            Some(VerifyPlan::Probe { .. })
        ));
    }

    #[test]
    fn judge_never_turns_a_refusal_into_success() {
        let ok = HttpResponse {
            status: 200,
            body: r#"{"ok":true}"#.to_string(),
            retry_after: None,
        };
        assert_eq!(
            judge(&ok, Expect::OkFlag).status,
            VerifyStatus::Ok,
            "telegram-style ok flag"
        );
        assert_eq!(judge(&ok, Expect::Status).status, VerifyStatus::Ok);

        let refused = HttpResponse {
            status: 401,
            body: "{}".to_string(),
            retry_after: None,
        };
        let result = judge(&refused, Expect::Status);
        assert_eq!(result.status, VerifyStatus::Failed);
        assert!(result.detail.contains("401"), "{}", result.detail);

        // Slack answers 200 with ok:false on a bad token — the body check must catch it.
        let slack_denied = HttpResponse {
            status: 200,
            body: r#"{"ok":false,"error":"invalid_auth"}"#.to_string(),
            retry_after: None,
        };
        let result = judge(&slack_denied, Expect::OkFlag);
        assert_eq!(result.status, VerifyStatus::Failed);
        assert!(result.detail.contains("invalid_auth"), "{}", result.detail);

        let token_body = HttpResponse {
            status: 200,
            body: r#"{"access_token":"tok-123","expires_in":7200}"#.to_string(),
            retry_after: None,
        };
        assert_eq!(
            judge(&token_body, Expect::AccessToken).status,
            VerifyStatus::Ok
        );
        let empty_token = HttpResponse {
            status: 200,
            body: r#"{"access_token":"  "}"#.to_string(),
            retry_after: None,
        };
        assert_eq!(
            judge(&empty_token, Expect::AccessToken).status,
            VerifyStatus::Failed
        );
    }

    #[test]
    fn url_tcp_target_parses_scheme_host_and_port() {
        match url_tcp_target("wss://openws.work.weixin.qq.com") {
            Some(ProbeTarget::Tcp { host, port }) => {
                assert_eq!(host, "openws.work.weixin.qq.com");
                assert_eq!(port, 443);
            }
            other => panic!("unexpected target: {other:?}"),
        }
        match url_tcp_target("ws://127.0.0.1:4000/ws") {
            Some(ProbeTarget::Tcp { host, port }) => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 4000);
            }
            other => panic!("unexpected target: {other:?}"),
        }
        assert!(url_tcp_target("not a url").is_none());
    }

    #[test]
    fn collect_values_real_case_signal() {
        let spec = find_spec("signal").unwrap();
        let vars = spec.required;
        let mut answers = ["https://signal.example\n", "acct-1\n"].into_iter();
        let values = collect_values(spec, vars, |_| Ok(answers.next().unwrap().to_string()))
            .expect("both required values supplied");
        assert_eq!(values.len(), 2);
        assert_eq!(
            values.get("SIGNAL_ACCOUNT").map(String::as_str),
            Some("acct-1")
        );
        assert_eq!(
            values.get("SIGNAL_HTTP_URL").map(String::as_str),
            Some("https://signal.example")
        );
    }

    #[test]
    fn collect_values_optional_empty_is_skipped_and_required_eventually_errors_on_eof() {
        let spec = find_spec("irc").unwrap();
        // IRC_SERVER + IRC_NICKNAME required; IRC_PORT/IRC_CHANNEL optional.
        let vars: Vec<&'static str> = spec
            .required
            .iter()
            .chain(spec.optional.iter())
            .copied()
            .collect();
        let mut answers = [
            "irc.example.com\n",
            "\n", // required nickname: empty -> reprompt
            "nick-1\n",
            "\n", // optional port: skip
            "\n", // optional channel: skip
        ]
        .into_iter();
        let values =
            collect_values(spec, &vars, |_| Ok(answers.next().unwrap().to_string())).unwrap();
        assert_eq!(
            values.get("IRC_SERVER").map(String::as_str),
            Some("irc.example.com")
        );
        assert_eq!(
            values.get("IRC_NICKNAME").map(String::as_str),
            Some("nick-1")
        );
        assert!(!values.contains_key("IRC_PORT"));
        assert!(!values.contains_key("IRC_CHANNEL"));

        // EOF on a required variable aborts instead of looping forever.
        let mut empty = std::iter::empty::<String>();
        let err = collect_values(spec, &["IRC_SERVER"], |_| {
            empty
                .next()
                .map(Ok)
                .unwrap_or_else(|| Err("aborted: end of input".to_string()))
        })
        .unwrap_err();
        assert!(err.contains("aborted"), "{err}");
    }

    #[test]
    fn write_scaffold_writes_real_values_and_keeps_placeholders() {
        let (_tmp, _guard) = test_home();
        let spec = find_spec("signal").unwrap();
        let added: Vec<&'static str> = spec
            .required
            .iter()
            .chain(spec.optional.iter())
            .copied()
            .collect();
        let path = vikram_home().join(".env");
        let mut values = BTreeMap::new();
        values.insert("SIGNAL_ACCOUNT".to_string(), "acct-1".to_string());
        values.insert(
            "SIGNAL_HTTP_URL".to_string(),
            "https://signal.example".to_string(),
        );
        write_scaffold(&path, "KEEP_ME=1\n", spec, &added, &values).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("KEEP_ME=1\n"));
        assert!(body.contains("SIGNAL_ACCOUNT=acct-1\n"), "{body}");
        assert!(
            body.contains("SIGNAL_HTTP_URL=https://signal.example\n"),
            "{body}"
        );
        assert!(!body.contains("# SIGNAL_ACCOUNT="), "{body}");

        // dotenv_value quoting round-trips through parse_dotenv.
        assert_eq!(dotenv_value("plain"), "plain");
        assert_eq!(dotenv_value("  padded  "), "\"  padded  \"");
        let parsed = parse_dotenv("A=\"  padded  \"\nB=plain\n");
        assert_eq!(parsed.get("A").map(String::as_str), Some("  padded  "));
        assert_eq!(parsed.get("B").map(String::as_str), Some("plain"));
    }
}
