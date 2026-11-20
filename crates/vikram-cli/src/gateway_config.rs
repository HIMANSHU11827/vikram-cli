//! `vikram gateway <action>` — the managed surface over the env-driven gateway.
//!
//! `vikram gateway` with no action still just runs every platform the environment configures.
//! The actions here manage `gateway.json` (per-platform on/off toggles) and scaffold credential
//! placeholders into `$VIKRAM_HOME/.env`. A platform with no toggle entry is "auto": it runs
//! exactly when its variables are set, which is the pre-existing behavior.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use clap::Subcommand;
use serde::{Deserialize, Serialize};
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
    Add { platform: String },
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

    fn save(&self) -> Result<(), String> {
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
        GatewayAction::Add { platform } => add(&platform),
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

fn add(name: &str) -> Result<(), String> {
    let spec = find_spec(name).ok_or_else(|| unknown_platform(name))?;
    ensure_home().map_err(|err| err.to_string())?;
    let path = vikram_home().join(".env");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let present = dotenv_keys(&existing);
    let mut added = Vec::new();
    let mut kept = Vec::new();
    let mut block = format!("# {}\n", spec.name);
    for var in spec.required.iter().chain(spec.optional.iter()) {
        if present.contains(*var) || env(var).is_some() {
            kept.push(*var);
        } else {
            added.push(*var);
            block.push_str(&format!("# {var}=\n"));
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
    let mut body = existing;
    if !body.is_empty() {
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push('\n');
    }
    body.push_str(&block);
    std::fs::write(&path, &body)
        .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    println!("scaffolded {} into {}", spec.name, path.display());
    for var in &added {
        println!("  # {var}=");
    }
    println!("uncomment and fill each line, then `vikram gateway list` shows what is missing");
    if spec.webhook {
        println!("webhook platform: also set GATEWAY_WEBHOOK_BIND=<bind address> for the routes");
    }
    if !kept.is_empty() {
        println!("already present, left untouched: {}", kept.join(", "));
    }
    Ok(())
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
        let err = add("not-a-platform").unwrap_err();
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

        add("signal").unwrap();
        let body = std::fs::read_to_string(&env_path).unwrap();
        assert!(body.starts_with("KEEP_ME=1\nSIGNAL_HTTP_URL=https://signal.example\n"));
        assert!(body.contains("# signal\n"));
        assert!(body.contains("# SIGNAL_ACCOUNT=\n"));
        // SIGNAL_HTTP_URL is already set, so only the missing variable was scaffolded.
        assert!(!body.contains("# SIGNAL_HTTP_URL="));

        // Running add again must not duplicate the commented placeholders.
        add("signal").unwrap();
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
}
