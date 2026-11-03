//! Configuration and home-directory discovery.
//!
//! Every value here comes from a file the operator can inspect. Nothing is inferred from the
//! environment except the API key, which is read from the environment or from the operator's own
//! `$VIKRAM_HOME/.env` and is never written to disk by us.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use vikram_contracts::PermissionMode;

use crate::error::RuntimeError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// OpenAI-compatible endpoint. Any provider speaking `/chat/completions` works.
    pub base_url: String,
    /// Model identifier. Empty means unconfigured, and the runtime reports that rather than
    /// inventing a name.
    pub model: String,
    /// Environment variable holding the bearer token.
    pub api_key_env: String,
    /// Which backend answers `web.search`, and where it lives.
    #[serde(default)]
    pub search: SearchConfig,
    /// Context window advertised by the chosen model, if the operator knows it.
    pub context_window: Option<u64>,
    /// Root that writes are confined to. Defaults to the process working directory.
    pub workspace_root: Option<PathBuf>,
    /// Hard cap on tool/step iterations inside one turn.
    pub max_steps: u32,
    /// Cap on generated tokens per completion request.
    pub max_output_tokens: Option<u32>,
    /// Two-frame ASCII fallback for terminals without the Unicode set.
    pub ascii_fallback: bool,
    /// Whether the selected model/provider has thinking/reasoning enabled. Drives Thinking vs Working labels.
    #[serde(default)]
    pub thinking: bool,
    /// The posture toward prompts this process starts in. Changeable live in the terminal
    /// interface; the headless path takes it from here or from `--mode`.
    pub mode: PermissionMode,
    /// Settings from the layer that decides which postures may be turned on at all.
    #[serde(default)]
    pub security: SecurityConfig,
}

/// Restrictions on mode *availability*, as distinct from the mode itself. `mode` says what this
/// process does; these say what it is allowed to say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// `yolo` may not be activated by anything, including this file.
    pub disable_yolo: bool,
    /// No posture that approves without a human: `edit-ok`, `smart`, `yolo`.
    pub disable_auto_approve: bool,
    /// Nothing may be written to the durable allow-list, so every approval is good for one request
    /// only. This is about persistence, not about the run itself: `ask` still works, it just forgets.
    pub disable_always_allow: bool,
    /// When present, only these postures exist. Anything outside the list is refused at activation.
    pub allowed_modes: Option<Vec<PermissionMode>>,
    /// OS-level process isolation for shell/build/test tools. On by default.
    #[serde(default)]
    pub sandbox: SandboxSettings,
}

/// Sandbox configuration. `enabled` defaults to true; the platform decides which mechanism to
/// use (bubblewrap on Linux, Seatbelt on macOS, Job objects on Windows where available).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub allow_network: bool,
    /// Extra read-only mounts granted to the sandbox (absolute paths).
    #[serde(default)]
    pub read_only_paths: Vec<PathBuf>,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_network: false,
            read_only_paths: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

impl SecurityConfig {
    fn forbids(&self, mode: PermissionMode) -> Option<&'static str> {
        if mode.is_bypass() && self.disable_yolo {
            return Some("`security.disable_yolo`");
        }
        if mode.auto_approves() && self.disable_auto_approve {
            return Some("`security.disable_auto_approve`");
        }
        if let Some(allowed) = &self.allowed_modes
            && !allowed.contains(&mode)
        {
            return Some("`security.allowed_modes`");
        }
        None
    }
}

/// Which backend answers `web.search`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchBackend {
    /// The keyed backend when its key is present, the keyless HTML backend otherwise.
    #[default]
    Auto,
    /// Brave's JSON API. Needs a key.
    Brave,
    /// Bing's HTML endpoint. Needs no key.
    Bing,
}

/// Search settings. Defaults are usable without any key: the keyless backend answers, and a key
/// only switches the traffic to the JSON API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// `auto`, `brave`, or `bing`.
    pub backend: SearchBackend,
    /// Environment variable holding the keyed backend's key. Read at call time, never stored here.
    pub api_key_env: String,
    /// Brave's web-search endpoint. Overridable for a mirror or a labelled proxy.
    pub brave_endpoint: String,
    /// Bing's HTML search endpoint.
    pub bing_endpoint: String,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            backend: SearchBackend::Auto,
            api_key_env: "VIKRAM_SEARCH_API_KEY".into(),
            brave_endpoint: "https://api.search.brave.com/res/v1/web/search".into(),
            bing_endpoint: "https://www.bing.com/search".into(),
        }
    }
}

/// Every layer that can forbid a posture, merged. Strictest wins: the booleans are ORed across
/// layers and an allow-list, if any layer sets one, constrains all of them.
///
/// `/etc/vikram/requirements.json` sits above the operator's own config so a plugin, a substituted
/// policy, or a session cannot widen it — the same shape Codex uses with `requirements.toml` and
/// Gemini with `security.disableYoloMode`, which is documented to win "even if enabled by a flag".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModeAvailability {
    layers: Vec<(&'static str, SecurityConfig)>,
}

impl ModeAvailability {
    /// Build from the system requirements file, the environment, and the operator's config, in that
    /// order. A missing or unreadable requirements file means no extra restriction, which is the
    /// normal state on a personal machine.
    pub fn resolve(config: &SecurityConfig) -> Self {
        let mut layers = Vec::new();
        let system = load_requirements(&requirements_path());
        if system != SecurityConfig::default() {
            layers.push(("system requirements", system));
        }
        let from_env = env_requirements();
        if from_env != SecurityConfig::default() {
            layers.push(("environment", from_env));
        }
        if *config != SecurityConfig::default() {
            layers.push(("config", config.clone()));
        }
        Self { layers }
    }

    pub fn empty() -> Self {
        Self { layers: Vec::new() }
    }

    /// Whether a permission may be written down at all. Distinct from mode availability: an
    /// operator can be allowed to run `cargo test` on approval but never to keep that approval.
    pub fn always_allow_refusal(&self) -> Option<(&'static str, &'static str)> {
        self.layers
            .iter()
            .find(|(_, config)| config.disable_always_allow)
            .map(|(layer, _)| ("`security.disable_always_allow`", *layer))
    }

    /// The rule and the layer that refuse this posture, so the interface can name where to look.
    pub fn refusal(&self, mode: PermissionMode) -> Option<(&'static str, &'static str)> {
        self.layers
            .iter()
            .find_map(|(layer, config)| config.forbids(mode).map(|rule| (rule, *layer)))
    }

    pub fn permits(&self, mode: PermissionMode) -> bool {
        self.refusal(mode).is_none()
    }

    /// The posture to fall back on when the requested one is forbidden. `ask` first, because it is
    /// the posture that can always do the work it is asked to do with a human in the loop; `plan`
    /// only when the layers left nothing else, since it refuses every mutation.
    pub fn fallback_mode(&self) -> PermissionMode {
        if self.permits(PermissionMode::Ask) {
            return PermissionMode::Ask;
        }
        // Listed by how little authority the posture grants on its own, with the vocabulary
        // derived from the type so a mode added later cannot be left out of the ordering.
        let order = [
            PermissionMode::Plan,
            PermissionMode::AllowList,
            PermissionMode::Unattended,
            PermissionMode::AcceptEdits,
            PermissionMode::Smart,
            PermissionMode::Yolo,
        ];
        order
            .into_iter()
            .find(|mode| self.permits(*mode))
            .unwrap_or(PermissionMode::Ask)
    }

    /// Everything the operator can still turn on, for `vikram doctor` and the mode picker.
    /// Derived from [`PermissionMode::all`] rather than a copy of it: a mode that is not listed
    /// here cannot be selected from the interface, and the seventh mode was forgotten that way once.
    pub fn available(&self) -> Vec<PermissionMode> {
        PermissionMode::all()
            .into_iter()
            .filter(|mode| self.permits(*mode))
            .collect()
    }
}

/// Where the system-level requirements live. `VIKRAM_REQUIREMENTS` overrides it for tests and for
/// operators who keep the file somewhere else.
pub fn requirements_path() -> PathBuf {
    if let Some(path) = std::env::var_os("VIKRAM_REQUIREMENTS") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    PathBuf::from("/etc/vikram/requirements.json")
}

fn load_requirements(path: &Path) -> SecurityConfig {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<SecurityConfig>(&raw).ok())
        .unwrap_or_default()
}

/// Host-controlled switches, so a machine can forbid a posture without editing anyone's config.
fn env_requirements() -> SecurityConfig {
    let forbidden = |name: &str| {
        std::env::var(name)
            .ok()
            .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
    };
    let allowed = std::env::var("VIKRAM_ALLOWED_MODES").ok().and_then(|raw| {
        let modes: Vec<PermissionMode> = raw
            .split(',')
            .filter_map(|name| PermissionMode::parse(name.trim()))
            .collect();
        (!modes.is_empty()).then_some(modes)
    });
    SecurityConfig {
        disable_yolo: forbidden("VIKRAM_DISABLE_YOLO"),
        disable_auto_approve: forbidden("VIKRAM_DISABLE_AUTO_APPROVE"),
        disable_always_allow: forbidden("VIKRAM_DISABLE_ALWAYS_ALLOW"),
        allowed_modes: allowed,
        sandbox: SandboxSettings::default(),
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            model: String::new(),
            api_key_env: "OPENAI_API_KEY".into(),
            search: SearchConfig::default(),
            context_window: None,
            workspace_root: None,
            max_steps: 24,
            max_output_tokens: None,
            ascii_fallback: false,
            thinking: false,
            mode: PermissionMode::default(),
            security: SecurityConfig::default(),
        }
    }
}

impl Config {
    /// `$VIKRAM_HOME/config.json`, falling back to `~/.vikram/config.json`.
    pub fn load() -> Result<Self, RuntimeError> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| RuntimeError::Config {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        serde_json::from_str(&raw).map_err(|e| RuntimeError::Config {
            path,
            reason: e.to_string(),
        })
    }

    pub fn save(&self) -> Result<(), RuntimeError> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| RuntimeError::Config {
                path: parent.to_path_buf(),
                reason: e.to_string(),
            })?;
        }
        let body = serde_json::to_string_pretty(self).expect("config is serializable");
        std::fs::write(&path, body).map_err(|e| RuntimeError::Config {
            path,
            reason: e.to_string(),
        })
    }

    /// The key plus the place it was found in, so diagnostics can name the source instead of
    /// guessing from the variable name.
    pub fn api_key_with_source(&self) -> Option<(String, &'static str)> {
        key_with_source(&self.api_key_env)
    }

    /// The keyed search backend's key, resolved exactly like the model key: the environment first,
    /// then `$VIKRAM_HOME/.env`. `None` means the keyless backend is the only one available.
    pub fn search_api_key_with_source(&self) -> Option<(String, &'static str)> {
        key_with_source(&self.search.api_key_env)
    }

    /// The key is read at call time and never persisted by us. It comes from the environment
    /// variable named in the config, or, if that is unset, from `KEY=value` lines in
    /// `$VIKRAM_HOME/.env` — a file the operator owns and we only read.
    pub fn api_key(&self) -> Option<String> {
        self.api_key_with_source().map(|(value, _)| value)
    }

    pub fn workspace_root(&self) -> PathBuf {
        self.workspace_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// A request is possible only when both an endpoint and a key exist.
    pub fn is_model_configured(&self) -> bool {
        !self.model.trim().is_empty() && self.api_key().is_some()
    }
}

/// Read a key named by a config field, reporting which of the two allowed places it came from.
fn key_with_source(name: &str) -> Option<(String, &'static str)> {
    let from_env = std::env::var(name).ok();
    if let Some(value) = from_env.filter(|v| !v.trim().is_empty()) {
        return Some((value, "environment"));
    }
    home_dotenv()
        .get(name)
        .cloned()
        .filter(|v| !v.trim().is_empty())
        .map(|value| (value, "$VIKRAM_HOME/.env"))
}

/// Root for runtime state: `$VIKRAM_HOME`, else `~/.vikram`.
pub fn vikram_home() -> PathBuf {
    if let Some(home) = std::env::var_os("VIKRAM_HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".vikram")
}

pub fn config_path() -> PathBuf {
    vikram_home().join("config.json")
}

pub fn database_path() -> PathBuf {
    vikram_home().join("vikram.db")
}

/// Create the runtime home with private permissions on Unix.
pub fn ensure_home() -> Result<PathBuf, RuntimeError> {
    let home = vikram_home();
    std::fs::create_dir_all(&home).map_err(|e| RuntimeError::Config {
        path: home.clone(),
        reason: e.to_string(),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
    }
    Ok(home)
}

pub fn mcp_config_path() -> PathBuf {
    vikram_home().join("mcp.json")
}

/// Read `KEY=value` lines from a dotenv file.
pub fn parse_dotenv(raw: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        map.insert(key.to_owned(), unquote(value.trim()).to_owned());
    }
    map
}

fn unquote(value: &str) -> &str {
    let quote = value.chars().next().unwrap_or(' ');
    let wrapped = (quote == '"' || quote == '\'') && value.len() > 1 && value.ends_with(quote);
    if wrapped {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Settings from `$VIKRAM_HOME/.env`, if the operator keeps one. A missing file is normal.
fn home_dotenv() -> std::collections::BTreeMap<String, String> {
    let path = vikram_home().join(".env");
    std::fs::read_to_string(path)
        .map(|raw| parse_dotenv(&raw))
        .unwrap_or_default()
}

pub fn plugins_dir() -> PathBuf {
    vikram_home().join("plugins")
}

/// A configured MCP server entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    /// `vikram mcp serve` exposes exactly these tools over STDIO. Empty means nothing is exposed.
    #[serde(default)]
    pub approved_tools: Vec<String>,
    #[serde(default)]
    pub auth_required: bool,
    #[serde(default)]
    pub auth_token: Option<String>,
}

impl McpConfig {
    pub fn load() -> Self {
        let path = mcp_config_path();
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }
}

/// Resolve a path against the workspace root and refuse escapes.
pub fn resolve_within(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        use std::path::Component;
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.starts_with(root) {
        Some(normalized)
    } else {
        None
    }
}

/// `VIKRAM_HOME` and the security switches are process-global, and `ModeAvailability` reads them
/// while a runtime is being built, so every test that touches either takes turns on this one lock.
pub static SECURITY_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Clear every environment input, then hold the lock for the calling test. A test that sets one of
/// these without clearing it itself leaks into whichever test runs next in the same process — the
/// list is the backstop, not the test's own hygiene.
pub fn lock_security_env() -> std::sync::MutexGuard<'static, ()> {
    let guard = SECURITY_ENV.lock().unwrap_or_else(|err| err.into_inner());
    for name in [
        "VIKRAM_HOME",
        "VIKRAM_REQUIREMENTS",
        "VIKRAM_DISABLE_YOLO",
        "VIKRAM_DISABLE_AUTO_APPROVE",
        "VIKRAM_DISABLE_ALWAYS_ALLOW",
        "VIKRAM_ALLOWED_MODES",
        "VIKRAM_TEST_KEY",
        "VIKRAM_SEARCH_API_KEY",
    ] {
        unsafe { std::env::remove_var(name) };
    }
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated_home(dir: &Path) -> std::sync::MutexGuard<'static, ()> {
        let guard = lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", dir) };
        guard
    }

    #[test]
    fn a_key_in_the_home_dotenv_is_found_when_the_environment_lacks_it() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        std::fs::write(dir.path().join(".env"), "VIKRAM_TEST_KEY=from-the-file\n").unwrap();

        let cfg = Config {
            api_key_env: "VIKRAM_TEST_KEY".into(),
            ..Config::default()
        };
        assert_eq!(cfg.api_key().as_deref(), Some("from-the-file"));
    }

    #[test]
    fn the_environment_wins_over_the_home_dotenv() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        std::fs::write(dir.path().join(".env"), "VIKRAM_TEST_KEY=from-the-file\n").unwrap();
        unsafe { std::env::set_var("VIKRAM_TEST_KEY", "from-the-environment") };

        let cfg = Config {
            api_key_env: "VIKRAM_TEST_KEY".into(),
            ..Config::default()
        };
        assert_eq!(cfg.api_key().as_deref(), Some("from-the-environment"));
    }

    #[test]
    fn dotenv_comments_quotes_and_export_prefixes_are_handled() {
        let map = parse_dotenv(
            "# comment\n\
             \n\
             PLAIN=value\n\
             export EXPORTED=second\n\
             QUOTED=\"third value\"\n\
             SINGLE='fourth'\n\
             =no-key\n",
        );
        assert_eq!(map.get("PLAIN").map(String::as_str), Some("value"));
        assert_eq!(map.get("EXPORTED").map(String::as_str), Some("second"));
        assert_eq!(map.get("QUOTED").map(String::as_str), Some("third value"));
        assert_eq!(map.get("SINGLE").map(String::as_str), Some("fourth"));
        assert_eq!(map.len(), 4, "a line without a key is not a setting");
    }

    #[test]
    fn an_empty_or_absent_key_counts_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        std::fs::write(dir.path().join(".env"), "VIKRAM_TEST_KEY=   \n").unwrap();

        let cfg = Config {
            api_key_env: "VIKRAM_TEST_KEY".into(),
            model: "some-model".into(),
            ..Config::default()
        };
        assert_eq!(cfg.api_key(), None);
        assert!(!cfg.is_model_configured());
    }

    #[test]
    fn the_key_reports_which_of_the_two_places_it_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        std::fs::write(dir.path().join(".env"), "VIKRAM_TEST_KEY=from-the-file\n").unwrap();
        unsafe { std::env::remove_var("VIKRAM_TEST_KEY") };

        let cfg = Config {
            api_key_env: "VIKRAM_TEST_KEY".into(),
            ..Config::default()
        };
        assert_eq!(
            cfg.api_key_with_source().map(|(_, source)| source),
            Some("$VIKRAM_HOME/.env")
        );

        unsafe { std::env::set_var("VIKRAM_TEST_KEY", "from-the-environment") };
        assert_eq!(
            cfg.api_key_with_source().map(|(_, source)| source),
            Some("environment")
        );
    }

    #[test]
    fn an_unconfigured_model_is_reported_as_unconfigured() {
        let cfg = Config::default();
        assert!(!cfg.is_model_configured());
        assert!(cfg.model.is_empty());
    }

    #[test]
    fn path_escapes_are_refused() {
        let root = Path::new("/work/proj");
        assert!(resolve_within(root, Path::new("src/main.rs")).is_some());
        assert!(resolve_within(root, Path::new("../etc/passwd")).is_none());
        assert!(resolve_within(root, Path::new("/etc/passwd")).is_none());
    }

    /// Neutralise every layer a test is not deliberately setting, then restore on the same lock.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        lock_security_env()
    }

    #[test]
    fn a_config_written_before_modes_exist_still_loads_as_ask() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"base_url":"https://x/v1","model":"m","api_key_env":"K","max_steps":8}"#,
        )
        .unwrap();
        let config = Config::load().unwrap();
        assert_eq!(config.mode, PermissionMode::Ask);
        assert_eq!(config.security, SecurityConfig::default());
        assert_eq!(config.search, SearchConfig::default());
    }

    #[test]
    fn the_search_key_is_read_like_the_model_key_and_never_stored() {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        std::fs::write(
            dir.path().join(".env"),
            "VIKRAM_SEARCH_API_KEY=from-the-file\n",
        )
        .unwrap();
        unsafe { std::env::remove_var("VIKRAM_SEARCH_API_KEY") };

        let cfg = Config::default();
        assert_eq!(
            cfg.search_api_key_with_source().map(|(_, source)| source),
            Some("$VIKRAM_HOME/.env")
        );
        unsafe { std::env::set_var("VIKRAM_SEARCH_API_KEY", "from-the-environment") };
        assert_eq!(
            cfg.search_api_key_with_source().map(|(_, source)| source),
            Some("environment")
        );
    }

    #[test]
    fn a_posture_forbidden_by_the_system_file_cannot_be_turned_on() {
        let _env = lock_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requirements.json");
        std::fs::write(&path, r#"{"disable_yolo":true}"#).unwrap();
        unsafe { std::env::set_var("VIKRAM_REQUIREMENTS", &path) };

        let availability = ModeAvailability::resolve(&SecurityConfig::default());
        assert!(!availability.permits(PermissionMode::Yolo));
        assert_eq!(
            availability.refusal(PermissionMode::Yolo),
            Some(("`security.disable_yolo`", "system requirements"))
        );
        // The operator's own config cannot widen what the system layer refused.
        let widened = ModeAvailability::resolve(&SecurityConfig {
            allowed_modes: Some(vec![PermissionMode::Yolo]),
            ..Default::default()
        });
        assert!(!widened.permits(PermissionMode::Yolo));
    }

    #[test]
    fn a_machine_switch_forbids_keeping_an_approval_without_forbidding_asking() {
        let _guard = lock_env();
        let open = ModeAvailability::resolve(&SecurityConfig::default());
        assert_eq!(open.always_allow_refusal(), None);
        unsafe { std::env::set_var("VIKRAM_DISABLE_ALWAYS_ALLOW", "1") };
        let closed = ModeAvailability::resolve(&SecurityConfig::default());
        assert_eq!(
            closed.always_allow_refusal(),
            Some(("`security.disable_always_allow`", "environment"))
        );
        // Nothing about the postures themselves changed: the runtime still asks, it just forgets.
        assert_eq!(closed.available(), open.available());
        unsafe { std::env::remove_var("VIKRAM_DISABLE_ALWAYS_ALLOW") };
    }

    #[test]
    fn an_environment_switch_forbids_a_posture_without_editing_anyones_config() {
        let _env = lock_env();
        unsafe { std::env::set_var("VIKRAM_DISABLE_AUTO_APPROVE", "1") };
        let availability = ModeAvailability::resolve(&SecurityConfig::default());
        for mode in [
            PermissionMode::Yolo,
            PermissionMode::Smart,
            PermissionMode::AcceptEdits,
        ] {
            assert!(
                !availability.permits(mode),
                "{mode} approves without a human"
            );
        }
        // Denying on the operator's behalf is not an auto-approval, so it survives.
        assert!(availability.permits(PermissionMode::Ask));
        assert!(availability.permits(PermissionMode::Plan));
        assert!(availability.permits(PermissionMode::Unattended));
    }

    #[test]
    fn an_allow_list_restricts_rather_than_prefers() {
        let _env = lock_env();
        unsafe { std::env::set_var("VIKRAM_ALLOWED_MODES", "ask,plan") };
        let availability = ModeAvailability::resolve(&SecurityConfig::default());
        assert_eq!(
            availability.available(),
            vec![PermissionMode::Ask, PermissionMode::Plan]
        );
        assert!(!availability.permits(PermissionMode::Smart));
    }

    #[test]
    fn a_forbidden_starting_mode_falls_back_to_the_strictest_one_left() {
        let _env = lock_env();
        let availability = ModeAvailability::resolve(&SecurityConfig {
            allowed_modes: Some(vec![PermissionMode::Yolo, PermissionMode::Smart]),
            ..Default::default()
        });
        assert_eq!(availability.fallback_mode(), PermissionMode::Smart);
        let nothing_but_yolo = ModeAvailability::resolve(&SecurityConfig {
            allowed_modes: Some(vec![PermissionMode::Yolo]),
            ..Default::default()
        });
        assert_eq!(nothing_but_yolo.fallback_mode(), PermissionMode::Yolo);
        // A layer that permits nothing at all still leaves the prompting posture.
        assert_eq!(
            ModeAvailability::empty().fallback_mode(),
            PermissionMode::Ask
        );
    }

    #[test]
    fn every_posture_is_offered_until_a_layer_refuses_it() {
        let _env = lock_env();
        let open = ModeAvailability::resolve(&SecurityConfig::default());
        assert_eq!(open.available().len(), PermissionMode::all().len());
        assert!(
            open.available().contains(&PermissionMode::AllowList),
            "the picker cannot omit a mode the runtime accepts: {:?}",
            open.available()
        );
        // `allow-list` prompts for what is not listed, so it is not an auto-approval and survives
        // the switch that forbids approving without a human.
        unsafe { std::env::set_var("VIKRAM_DISABLE_AUTO_APPROVE", "1") };
        let strict = ModeAvailability::resolve(&SecurityConfig::default());
        assert!(strict.permits(PermissionMode::AllowList));
        assert!(!strict.available().contains(&PermissionMode::Yolo));
        unsafe { std::env::remove_var("VIKRAM_DISABLE_AUTO_APPROVE") };
    }
}
