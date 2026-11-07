//! Lazy platform registry (module 8 of the foundation).
//!
//! Port of hermes-agent `gateway/platform_registry.py`, `gateway/platform_activation.py` and the
//! plugin discovery rules in `gateway/plugins_discovery.py`.
//!
//! Two invariants from the hermes gateway are load-bearing, and conflating them caused boot loops
//! hermes fixed the hard way (#79812, #64674):
//!
//! 1. **Registration is lazy.** A platform registers a [`PlatformEntry`] with a deferred loader.
//!    Nothing is imported, constructed or connected until the platform is actually activated, so
//!    forty plugins cost nothing at boot.
//! 2. **Probing is passive, installing is active.** [`PlatformRegistry::check`] runs the entry's
//!    `check` probe and *must never install anything* — it answers "are the dependencies present?"
//!    for a status command or an activation decision. Only
//!    [`PlatformRegistry::ensure_dependencies`] (reached from [`PlatformRegistry::connect`] at
//!    connect time) may run the installer.
//!
//! Activation rules:
//!
//! - **Bundled** platforms ship with the gateway and activate without any opt-in; only an explicit
//!   `plugins.disabled` entry keeps them off.
//! - **User** and **project** platforms activate only with an explicit `plugins.enabled` opt-in
//!   ([`ActivationContext::enabled_explicit`] is the hermes `_enabled_explicit` marker: a default
//!   value in the schema does not count as the operator enabling anything).
//! - `requires_env` names gate activation: a platform missing its token is registered but inactive,
//!   which is what lets `gateway doctor` report it instead of crashing the boot.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::access::EnvLookup;
use crate::platform::PlatformAdapter;

/// Where a platform entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlatformSource {
    /// Shipped inside the gateway crate/binary. No opt-in required.
    Bundled,
    /// Installed by the user under `~/.vikram/plugins/<name>/`. Opt-in required.
    User,
    /// Discovered in the project's `.vikram/plugins/`. Opt-in required.
    Project,
}

impl PlatformSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlatformSource::Bundled => "bundled",
            PlatformSource::User => "user",
            PlatformSource::Project => "project",
        }
    }

    /// Does this source require an explicit `plugins.enabled` opt-in?
    pub fn requires_opt_in(&self) -> bool {
        !matches!(self, PlatformSource::Bundled)
    }
}

impl fmt::Display for PlatformSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Result of the passive dependency probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyStatus {
    /// Nothing is missing.
    Ready,
    /// Missing pieces (requirement names or env vars), in discovery order.
    Missing(Vec<String>),
    /// The entry declares no probe at all; treated as ready.
    Undeclared,
}

impl DependencyStatus {
    /// Ready or nothing declared — i.e. "go ahead".
    pub fn is_ready(&self) -> bool {
        matches!(self, DependencyStatus::Ready | DependencyStatus::Undeclared)
    }

    /// What is missing, if anything.
    pub fn missing(&self) -> &[String] {
        match self {
            DependencyStatus::Missing(items) => items,
            _ => &[],
        }
    }
}

/// A loaded platform adapter. Shared, so the registry hands out one instance per platform.
pub type LoadedPlatform = Arc<dyn PlatformAdapter>;

/// Deferred loader: constructs the adapter. Runs at most once per registry.
pub type PlatformLoader = Arc<dyn Fn() -> Result<LoadedPlatform, RegistryError> + Send + Sync>;

/// Passive dependency probe. **Must never install anything.**
pub type DependencyCheck = Arc<dyn Fn() -> DependencyStatus + Send + Sync>;

/// Active dependency installer. **Only ever called at connect time.**
pub type DependencyInstaller = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Activation predicate: given the boot context, should this platform be brought up?
pub type ActivationPredicate = Arc<dyn Fn(&ActivationContext<'_>) -> bool + Send + Sync>;

/// What the registry knows about the operator's intent at activation time.
pub struct ActivationContext<'a> {
    /// Names listed in `plugins.enabled` (may contain `*`).
    pub enabled: &'a BTreeSet<String>,
    /// The config file literally declared `plugins.enabled` (hermes `_enabled_explicit`).
    pub enabled_explicit: bool,
    /// Names listed in `plugins.disabled`.
    pub disabled: &'a BTreeSet<String>,
    /// Environment view for `requires_env` probes.
    pub env: &'a dyn EnvLookup,
}

impl<'a> ActivationContext<'a> {
    pub fn new(
        enabled: &'a BTreeSet<String>,
        enabled_explicit: bool,
        disabled: &'a BTreeSet<String>,
        env: &'a dyn EnvLookup,
    ) -> Self {
        Self {
            enabled,
            enabled_explicit,
            disabled,
            env,
        }
    }

    fn is_enabled(&self, name: &str) -> bool {
        self.enabled_explicit && (self.enabled.contains(name) || self.enabled.contains("*"))
    }

    fn is_disabled(&self, name: &str) -> bool {
        self.disabled.contains(name)
    }
}

impl fmt::Debug for ActivationContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActivationContext")
            .field("enabled", &self.enabled)
            .field("enabled_explicit", &self.enabled_explicit)
            .field("disabled", &self.disabled)
            .finish()
    }
}

/// Everything that can go wrong registering, activating or loading a platform.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("platform `{0}` is already registered")]
    Duplicate(String),
    #[error("platform `{0}` is not registered")]
    Unknown(String),
    #[error("platform `{0}` has an empty name")]
    EmptyName(String),
    #[error("dependency installation failed for `{name}`: {detail}")]
    DependencyInstall { name: String, detail: String },
    #[error("platform `{name}` failed to load: {detail}")]
    Load { name: String, detail: String },
}

impl RegistryError {
    /// Convenience for loader implementations.
    pub fn load_failed(name: impl Into<String>, detail: impl Into<String>) -> Self {
        RegistryError::Load {
            name: name.into(),
            detail: detail.into(),
        }
    }
}

/// One registered platform: how to activate it, how to probe it, and how to build it.
pub struct PlatformEntry {
    /// Registry key, lowercase platform name (`telegram`, `discord`, ...).
    pub name: String,
    /// Human label for `gateway status` output.
    pub label: String,
    /// Bundled / user / project.
    pub source: PlatformSource,
    /// Env vars that must be present for the platform to be usable.
    pub required_env: Vec<String>,
    check: Option<DependencyCheck>,
    ensure_dependencies: Option<DependencyInstaller>,
    loader: PlatformLoader,
    activation: ActivationPredicate,
}

impl PlatformEntry {
    /// A new entry with the source's default activation rule.
    pub fn new(
        name: impl Into<String>,
        label: impl Into<String>,
        source: PlatformSource,
        loader: PlatformLoader,
    ) -> Self {
        let name = name.into();
        Self {
            activation: default_activation(&name, source),
            name,
            label: label.into(),
            source,
            required_env: Vec::new(),
            check: None,
            ensure_dependencies: None,
            loader,
        }
    }

    /// A bundled platform: activates without opt-in.
    pub fn bundled(
        name: impl Into<String>,
        label: impl Into<String>,
        loader: PlatformLoader,
    ) -> Self {
        Self::new(name, label, PlatformSource::Bundled, loader)
    }

    /// A user-installed platform under `~/.vikram/plugins/`: needs explicit opt-in.
    pub fn user(name: impl Into<String>, label: impl Into<String>, loader: PlatformLoader) -> Self {
        Self::new(name, label, PlatformSource::User, loader)
    }

    /// A project-local platform under `.vikram/plugins/`: needs explicit opt-in.
    pub fn project(
        name: impl Into<String>,
        label: impl Into<String>,
        loader: PlatformLoader,
    ) -> Self {
        Self::new(name, label, PlatformSource::Project, loader)
    }

    /// Attach the passive probe (import-affordability / package presence).
    pub fn with_check(mut self, probe: DependencyCheck) -> Self {
        self.check = Some(probe);
        self
    }

    /// Attach the active installer, only ever run at connect time.
    pub fn with_ensure_dependencies(mut self, installer: DependencyInstaller) -> Self {
        self.ensure_dependencies = Some(installer);
        self
    }

    /// Replace the default activation rule.
    pub fn with_activation(mut self, predicate: ActivationPredicate) -> Self {
        self.activation = predicate;
        self
    }

    /// Declare env vars the platform needs before it can be activated.
    pub fn with_required_env<S: Into<String>>(
        mut self,
        names: impl IntoIterator<Item = S>,
    ) -> Self {
        self.required_env = names.into_iter().map(Into::into).collect();
        self
    }

    /// Does this platform declare a passive probe?
    pub fn has_check(&self) -> bool {
        self.check.is_some()
    }

    /// Does this platform declare an installer?
    pub fn has_installer(&self) -> bool {
        self.ensure_dependencies.is_some()
    }

    /// Access the deferred loader (the registry normally calls it).
    pub fn loader(&self) -> &PlatformLoader {
        &self.loader
    }

    /// Env vars that `env` does not provide (absent or empty).
    pub fn missing_env(&self, env: &dyn EnvLookup) -> Vec<String> {
        self.required_env
            .iter()
            .filter(|key| env.get(key).is_none_or(|value| value.trim().is_empty()))
            .cloned()
            .collect()
    }

    fn activation_predicate(&self, ctx: &ActivationContext<'_>) -> bool {
        (self.activation)(ctx)
    }
}

impl fmt::Debug for PlatformEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlatformEntry")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("source", &self.source)
            .field("required_env", &self.required_env)
            .field("has_check", &self.has_check())
            .field("has_installer", &self.has_installer())
            .finish()
    }
}

/// Bundled → on unless disabled; user/project → on only with an explicit opt-in.
fn default_activation(name: &str, source: PlatformSource) -> ActivationPredicate {
    let name = name.to_string();
    Arc::new(move |ctx: &ActivationContext<'_>| {
        if ctx.is_disabled(&name) {
            return false;
        }
        if source.requires_opt_in() {
            ctx.is_enabled(&name)
        } else {
            true
        }
    })
}

/// The lazy registry: entries plus a one-shot cache of loaded adapters.
#[derive(Default)]
pub struct PlatformRegistry {
    entries: BTreeMap<String, PlatformEntry>,
    loaded: Mutex<BTreeMap<String, LoadedPlatform>>,
    load_counts: Mutex<BTreeMap<String, usize>>,
    probe_counts: Mutex<BTreeMap<String, usize>>,
    install_counts: Mutex<BTreeMap<String, usize>>,
}

impl PlatformRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a platform. Re-registering the same name is an error: duplicate registration is
    /// how two plugins silently fight over one platform.
    pub fn register(&mut self, entry: PlatformEntry) -> Result<(), RegistryError> {
        let name = entry.name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(RegistryError::EmptyName(entry.name));
        }
        if self.entries.contains_key(&name) {
            return Err(RegistryError::Duplicate(name));
        }
        if name == entry.name {
            self.entries.insert(name, entry);
        } else {
            let mut entry = entry;
            entry.name = name.clone();
            self.entries.insert(name, entry);
        }
        Ok(())
    }

    /// One entry.
    pub fn get(&self, name: &str) -> Option<&PlatformEntry> {
        self.entries.get(&normalize(name))
    }

    /// Registered names, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// All entries, sorted by name.
    pub fn entries(&self) -> impl Iterator<Item = &PlatformEntry> {
        self.entries.values()
    }

    /// How many platforms are registered.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// **Passive** dependency probe. Never installs anything, ever.
    ///
    /// Unknown names report [`DependencyStatus::Undeclared`] so a status command can render them.
    pub fn check(&self, name: &str, ctx: &ActivationContext<'_>) -> DependencyStatus {
        let Some(entry) = self.entries.get(&normalize(name)) else {
            return DependencyStatus::Undeclared;
        };
        let mut missing = entry.missing_env(ctx.env);
        let mut status = match &entry.check {
            Some(probe) => {
                self.count(&self.probe_counts, &entry.name);
                probe()
            }
            // Declaring `requires_env` and satisfying it leaves nothing unprobed.
            None if entry.required_env.is_empty() => DependencyStatus::Undeclared,
            None => DependencyStatus::Ready,
        };
        if !missing.is_empty() {
            missing.extend(status.missing().iter().cloned());
            status = DependencyStatus::Missing(missing);
        }
        status
    }

    /// Would this platform be brought up under `ctx`? Pure decision, no side effects.
    pub fn is_activated(&self, name: &str, ctx: &ActivationContext<'_>) -> bool {
        let Some(entry) = self.entries.get(&normalize(name)) else {
            return false;
        };
        entry.activation_predicate(ctx) && entry.missing_env(ctx.env).is_empty()
    }

    /// Every entry that would be brought up under `ctx`, sorted by name.
    pub fn activated(&self, ctx: &ActivationContext<'_>) -> Vec<&PlatformEntry> {
        self.entries
            .values()
            .filter(|entry| {
                entry.activation_predicate(ctx) && entry.missing_env(ctx.env).is_empty()
            })
            .collect()
    }

    /// **Active** dependency provisioning: the only place an installer may run.
    ///
    /// Call from the connect path only. Returns the status after the attempt so the caller can
    /// decide whether to fail the connect or retry later.
    pub fn ensure_dependencies(
        &self,
        name: &str,
        ctx: &ActivationContext<'_>,
    ) -> Result<DependencyStatus, RegistryError> {
        let key = normalize(name);
        let Some(entry) = self.entries.get(&key) else {
            return Err(RegistryError::Unknown(key));
        };
        let status = self.check(&key, ctx);
        if status.is_ready() {
            return Ok(status);
        }
        let Some(installer) = &entry.ensure_dependencies else {
            return Ok(status);
        };
        self.count(&self.install_counts, &key);
        installer().map_err(|detail| RegistryError::DependencyInstall {
            name: key.clone(),
            detail,
        })?;
        Ok(self.check(&key, ctx))
    }

    /// Run the deferred loader. The loader runs at most once; later calls hand back the same
    /// adapter.
    ///
    /// The loader runs while the cache lock is held so two concurrent connects cannot build two
    /// adapters. Loaders therefore must not call back into this registry.
    pub fn load(&self, name: &str) -> Result<LoadedPlatform, RegistryError> {
        let key = normalize(name);
        let entry = self
            .entries
            .get(&key)
            .ok_or_else(|| RegistryError::Unknown(key.clone()))?;
        let mut loaded = lock(&self.loaded);
        if let Some(adapter) = loaded.get(&key) {
            return Ok(adapter.clone());
        }
        let adapter = (entry.loader)()?;
        loaded.insert(key.clone(), adapter.clone());
        drop(loaded);
        lock(&self.load_counts).insert(key, 1);
        Ok(adapter)
    }

    /// Connect-time entry point: provision dependencies, then build the adapter.
    ///
    /// This is the only path that may install, and it is only reached once the operator decided to
    /// connect (activation already said yes).
    pub fn connect(
        &self,
        name: &str,
        ctx: &ActivationContext<'_>,
    ) -> Result<LoadedPlatform, RegistryError> {
        self.ensure_dependencies(name, ctx)?;
        self.load(name)
    }

    /// Is an adapter already built for `name`?
    pub fn is_loaded(&self, name: &str) -> bool {
        lock(&self.loaded).contains_key(&normalize(name))
    }

    /// Number of times the deferred loader actually ran for `name` (0 or 1).
    pub fn load_count(&self, name: &str) -> usize {
        lock(&self.load_counts)
            .get(&normalize(name))
            .copied()
            .unwrap_or(0)
    }

    /// Number of passive probes made for `name`.
    pub fn probe_count(&self, name: &str) -> usize {
        lock(&self.probe_counts)
            .get(&normalize(name))
            .copied()
            .unwrap_or(0)
    }

    /// Number of installer runs for `name`.
    pub fn install_count(&self, name: &str) -> usize {
        lock(&self.install_counts)
            .get(&normalize(name))
            .copied()
            .unwrap_or(0)
    }

    fn count(&self, counters: &Mutex<BTreeMap<String, usize>>, name: &str) {
        *lock(counters).entry(name.to_string()).or_insert(0) += 1;
    }
}

impl fmt::Debug for PlatformRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlatformRegistry")
            .field("entries", &self.entries.keys().collect::<Vec<_>>())
            .field("loaded", &lock(&self.loaded).keys().collect::<Vec<_>>())
            .finish()
    }
}

fn normalize(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

/// Mutex access that ignores poisoning: a panic in a probe must not poison the registry.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::Value;
    use std::collections::HashMap;

    use crate::platform::{ChatInfo, SendResult};

    struct StubAdapter {
        name: String,
    }

    #[async_trait]
    impl PlatformAdapter for StubAdapter {
        async fn connect(&mut self, _is_reconnect: bool) -> bool {
            true
        }

        async fn disconnect(&mut self) {}

        async fn send(
            &self,
            chat_id: &str,
            content: &str,
            _reply_to: Option<&str>,
            _metadata: Option<Value>,
        ) -> SendResult {
            SendResult::ok(format!("{}:{chat_id}:{content}", self.name))
        }

        async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
            ChatInfo {
                name: chat_id.to_string(),
                chat_type: "dm".to_string(),
            }
        }
    }

    fn loader(name: &'static str, runs: Arc<Mutex<usize>>) -> PlatformLoader {
        Arc::new(move || {
            *runs.lock().expect("lock") += 1;
            Ok(Arc::new(StubAdapter {
                name: name.to_string(),
            }) as LoadedPlatform)
        })
    }

    fn counting_loader(runs: Arc<Mutex<usize>>) -> PlatformLoader {
        loader("stub", runs)
    }

    struct Fixture {
        enabled: BTreeSet<String>,
        disabled: BTreeSet<String>,
        env: HashMap<String, String>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                enabled: BTreeSet::new(),
                disabled: BTreeSet::new(),
                env: HashMap::new(),
            }
        }

        fn enabled(mut self, names: &[&str]) -> Self {
            self.enabled = names.iter().map(|name| name.to_string()).collect();
            self
        }

        fn disabled(mut self, names: &[&str]) -> Self {
            self.disabled = names.iter().map(|name| name.to_string()).collect();
            self
        }

        fn env(mut self, key: &str, value: &str) -> Self {
            self.env.insert(key.to_string(), value.to_string());
            self
        }

        fn ctx(&self, explicit: bool) -> ActivationContext<'_> {
            ActivationContext::new(&self.enabled, explicit, &self.disabled, &self.env)
        }
    }

    #[test]
    fn deferred_loader_runs_only_once() {
        let runs = Arc::new(Mutex::new(0usize));
        let mut registry = PlatformRegistry::new();
        registry
            .register(PlatformEntry::bundled(
                "telegram",
                "Telegram",
                counting_loader(runs.clone()),
            ))
            .expect("register");

        assert_eq!(*runs.lock().expect("lock"), 0, "registration is lazy");
        assert!(!registry.is_loaded("telegram"));

        let first = registry.load("telegram").expect("first load");
        let second = registry.load("telegram").expect("second load");
        assert!(Arc::ptr_eq(&first, &second), "one adapter per platform");
        assert_eq!(*runs.lock().expect("lock"), 1);
        assert_eq!(registry.load_count("telegram"), 1);
        assert!(registry.is_loaded("telegram"));
    }

    #[test]
    fn loader_error_is_reported_and_retried() {
        let runs = Arc::new(Mutex::new(0usize));
        let attempts = runs.clone();
        let mut registry = PlatformRegistry::new();
        registry
            .register(PlatformEntry::bundled(
                "broken",
                "Broken",
                Arc::new(move || {
                    *attempts.lock().expect("lock") += 1;
                    Err(RegistryError::load_failed("broken", "no adapter"))
                }),
            ))
            .expect("register");

        let err = match registry.load("broken") {
            Ok(_) => panic!("load must fail"),
            Err(err) => err,
        };
        assert!(matches!(err, RegistryError::Load { .. }));
        assert!(!registry.is_loaded("broken"));
        // A failed load is not cached: the next connect may retry.
        assert!(registry.load("broken").is_err());
        assert_eq!(*runs.lock().expect("lock"), 2);
    }

    #[test]
    fn check_never_installs() {
        let installs = Arc::new(Mutex::new(0usize));
        let installer_runs = installs.clone();
        let ready = Arc::new(Mutex::new(false));
        let probe_state = ready.clone();
        let install_state = ready.clone();

        let mut registry = PlatformRegistry::new();
        registry
            .register(
                PlatformEntry::new(
                    "matrix",
                    "Matrix",
                    PlatformSource::Bundled,
                    counting_loader(Arc::new(Mutex::new(0))),
                )
                .with_check(Arc::new(move || {
                    if *probe_state.lock().expect("lock") {
                        DependencyStatus::Ready
                    } else {
                        DependencyStatus::Missing(vec!["matrix-nio".to_string()])
                    }
                }))
                .with_ensure_dependencies(Arc::new(move || {
                    *installer_runs.lock().expect("lock") += 1;
                    *install_state.lock().expect("lock") = true;
                    Ok(())
                })),
            )
            .expect("register");

        let fixture = Fixture::new();
        let ctx = fixture.ctx(true);

        let status = registry.check("matrix", &ctx);
        assert_eq!(
            status,
            DependencyStatus::Missing(vec!["matrix-nio".to_string()])
        );
        assert!(!status.is_ready());
        assert_eq!(registry.probe_count("matrix"), 1);
        assert_eq!(
            registry.install_count("matrix"),
            0,
            "the passive check must never install"
        );
        assert_eq!(*installs.lock().expect("lock"), 0);

        // Activation stays passive too.
        assert!(registry.is_activated("matrix", &ctx));
        assert_eq!(registry.install_count("matrix"), 0);

        // Only the active path installs.
        let after = registry
            .ensure_dependencies("matrix", &ctx)
            .expect("ensure dependencies");
        assert_eq!(after, DependencyStatus::Ready);
        assert_eq!(registry.install_count("matrix"), 1);
        assert_eq!(*installs.lock().expect("lock"), 1);

        // Nothing left to install once ready.
        registry.ensure_dependencies("matrix", &ctx).expect("again");
        assert_eq!(registry.install_count("matrix"), 1);
    }

    #[test]
    fn install_failure_is_reported() {
        let mut registry = PlatformRegistry::new();
        registry
            .register(
                PlatformEntry::new(
                    "wecom",
                    "WeCom",
                    PlatformSource::Bundled,
                    counting_loader(Arc::new(Mutex::new(0))),
                )
                .with_check(Arc::new(|| {
                    DependencyStatus::Missing(vec!["cryptography".into()])
                }))
                .with_ensure_dependencies(Arc::new(|| Err("pip is offline".to_string()))),
            )
            .expect("register");
        let fixture = Fixture::new();
        let err = registry
            .ensure_dependencies("wecom", &fixture.ctx(true))
            .expect_err("install must fail");
        assert!(matches!(err, RegistryError::DependencyInstall { .. }));
        assert!(err.to_string().contains("pip is offline"));
    }

    #[test]
    fn bundled_platform_activates_without_opt_in() {
        let runs = Arc::new(Mutex::new(0usize));
        let mut registry = PlatformRegistry::new();
        registry
            .register(PlatformEntry::bundled(
                "telegram",
                "Telegram",
                counting_loader(runs),
            ))
            .expect("register");

        // No config at all: `plugins.enabled` was never declared.
        let fixture = Fixture::new();
        let ctx = fixture.ctx(false);
        assert!(registry.is_activated("telegram", &ctx));
        assert_eq!(registry.activated(&ctx).len(), 1);

        // Only an explicit disable keeps a bundled platform off.
        let fixture = Fixture::new().disabled(&["telegram"]);
        assert!(!registry.is_activated("telegram", &fixture.ctx(true)));
        assert!(registry.activated(&fixture.ctx(true)).is_empty());
    }

    #[test]
    fn user_platform_needs_explicit_opt_in() {
        let mut registry = PlatformRegistry::new();
        registry
            .register(PlatformEntry::user(
                "mattermost",
                "Mattermost",
                counting_loader(Arc::new(Mutex::new(0))),
            ))
            .expect("register");

        // Not listed at all: off.
        let fixture = Fixture::new();
        assert!(!registry.is_activated("mattermost", &fixture.ctx(true)));

        // Listed but `plugins.enabled` was not explicitly declared (schema default): still off.
        let fixture = Fixture::new().enabled(&["mattermost"]);
        assert!(!registry.is_activated("mattermost", &fixture.ctx(false)));

        // Explicit opt-in: on.
        assert!(registry.is_activated("mattermost", &fixture.ctx(true)));

        // `*` opts everything in, as long as it is explicit.
        let fixture = Fixture::new().enabled(&["*"]);
        assert!(registry.is_activated("mattermost", &fixture.ctx(true)));
    }

    #[test]
    fn disabled_user_platform_is_not_activated() {
        let mut registry = PlatformRegistry::new();
        registry
            .register(PlatformEntry::user(
                "discord",
                "Discord",
                counting_loader(Arc::new(Mutex::new(0))),
            ))
            .expect("register");
        let fixture = Fixture::new().enabled(&["discord"]).disabled(&["discord"]);
        assert!(!registry.is_activated("discord", &fixture.ctx(true)));
        assert!(registry.activated(&fixture.ctx(true)).is_empty());
    }

    #[test]
    fn required_env_gates_activation() {
        let mut registry = PlatformRegistry::new();
        registry
            .register(
                PlatformEntry::bundled("slack", "Slack", counting_loader(Arc::new(Mutex::new(0))))
                    .with_required_env(["SLACK_BOT_TOKEN"]),
            )
            .expect("register");

        let empty = Fixture::new();
        let ctx = empty.ctx(true);
        assert!(!registry.is_activated("slack", &ctx));
        assert_eq!(
            registry.check("slack", &ctx),
            DependencyStatus::Missing(vec!["SLACK_BOT_TOKEN".to_string()])
        );

        let configured = Fixture::new().env("SLACK_BOT_TOKEN", "xoxb-1");
        let ctx = configured.ctx(true);
        assert!(registry.is_activated("slack", &ctx));
        assert_eq!(registry.check("slack", &ctx), DependencyStatus::Ready);
    }

    #[test]
    fn custom_activation_predicate_overrides_the_default() {
        let mut registry = PlatformRegistry::new();
        registry
            .register(
                PlatformEntry::user("relay", "Relay", counting_loader(Arc::new(Mutex::new(0))))
                    .with_activation(Arc::new(|ctx: &ActivationContext<'_>| {
                        ctx.env.get("RELAY_URL").is_some()
                    })),
            )
            .expect("register");

        let without = Fixture::new();
        assert!(!registry.is_activated("relay", &without.ctx(true)));
        let with = Fixture::new().env("RELAY_URL", "wss://relay");
        assert!(registry.is_activated("relay", &with.ctx(true)));
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let mut registry = PlatformRegistry::new();
        registry
            .register(PlatformEntry::bundled(
                "Telegram",
                "Telegram",
                counting_loader(Arc::new(Mutex::new(0))),
            ))
            .expect("first");
        let err = registry
            .register(PlatformEntry::bundled(
                "telegram",
                "Telegram again",
                counting_loader(Arc::new(Mutex::new(0))),
            ))
            .expect_err("duplicate");
        assert!(matches!(err, RegistryError::Duplicate(_)));
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.names(), vec!["telegram"]);
        // The original entry survives untouched.
        assert_eq!(registry.get("telegram").expect("entry").label, "Telegram");
    }

    #[test]
    fn empty_and_unknown_names_are_rejected() {
        let mut registry = PlatformRegistry::new();
        let err = registry
            .register(PlatformEntry::bundled(
                "  ",
                "blank",
                counting_loader(Arc::new(Mutex::new(0))),
            ))
            .expect_err("empty name");
        assert!(matches!(err, RegistryError::EmptyName(_)));

        assert!(registry.get("nope").is_none());
        assert!(matches!(
            registry.load("nope"),
            Err(RegistryError::Unknown(_))
        ));
        assert!(matches!(
            registry.ensure_dependencies("nope", &Fixture::new().ctx(true)),
            Err(RegistryError::Unknown(_))
        ));
        assert_eq!(
            registry.check("nope", &Fixture::new().ctx(true)),
            DependencyStatus::Undeclared
        );
        assert!(!registry.is_activated("nope", &Fixture::new().ctx(true)));
    }

    #[test]
    fn connect_installs_then_loads_once() {
        let runs = Arc::new(Mutex::new(0usize));
        let installs = Arc::new(Mutex::new(0usize));
        let installs_clone = installs.clone();
        let mut registry = PlatformRegistry::new();
        registry
            .register(
                PlatformEntry::bundled("signal", "Signal", loader("signal", runs.clone()))
                    .with_check(Arc::new(|| {
                        DependencyStatus::Missing(vec!["signal-cli".into()])
                    }))
                    .with_ensure_dependencies(Arc::new(move || {
                        *installs_clone.lock().expect("lock") += 1;
                        Ok(())
                    })),
            )
            .expect("register");

        let fixture = Fixture::new();
        let ctx = fixture.ctx(true);
        let adapter = registry.connect("signal", &ctx).expect("connect");
        assert_eq!(registry.install_count("signal"), 1);
        assert_eq!(registry.load_count("signal"), 1);
        assert_eq!(*runs.lock().expect("lock"), 1);
        // Brand new: connect twice reuses the same adapter and does not install again.
        let again = registry.connect("signal", &ctx).expect("connect again");
        assert!(Arc::ptr_eq(&adapter, &again));
        assert_eq!(*installs.lock().expect("lock"), 2, "installer still asks");
        assert_eq!(*runs.lock().expect("lock"), 1, "adapter built once");
    }

    #[test]
    fn activated_lists_entries_sorted_and_skips_inactive() {
        let mut registry = PlatformRegistry::new();
        let runs = Arc::new(Mutex::new(0usize));
        for name in ["telegram", "discord", "slack"] {
            registry
                .register(PlatformEntry::bundled(
                    name,
                    name.to_uppercase(),
                    counting_loader(runs.clone()),
                ))
                .expect("register");
        }
        registry
            .register(PlatformEntry::user(
                "mattermost",
                "Mattermost",
                counting_loader(runs),
            ))
            .expect("register");

        let fixture = Fixture::new()
            .enabled(&["mattermost"])
            .disabled(&["discord"]);
        let names: Vec<&str> = registry
            .activated(&fixture.ctx(true))
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["mattermost", "slack", "telegram"]);
        assert!(registry.is_activated("mattermost", &fixture.ctx(true)));
        // Loading works asynchronously of activation; nothing was built by listing.
        assert_eq!(registry.load_count("telegram"), 0);
    }

    #[test]
    fn source_opt_in_rules_and_debug() {
        assert!(!PlatformSource::Bundled.requires_opt_in());
        assert!(PlatformSource::User.requires_opt_in());
        assert!(PlatformSource::Project.requires_opt_in());
        assert_eq!(PlatformSource::Bundled.to_string(), "bundled");

        let entry = PlatformEntry::bundled(
            "telegram",
            "Telegram",
            counting_loader(Arc::new(Mutex::new(0))),
        );
        let debug = format!("{entry:?}");
        assert!(debug.contains("telegram"));
        assert!(debug.contains("has_check: false"));
        assert!(!entry.has_check());
        assert!(!entry.has_installer());
    }
}
