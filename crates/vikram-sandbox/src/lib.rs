//! OS-level process isolation for commands the host agrees to run.
//!
//! Policy decides *whether* a command may run; the sandbox shrinks what a running command can do
//! to the machine around it. Three named profiles, chosen by the caller:
//!
//! - [`SandboxProfile::Off`] — no isolation; policy still applies.
//! - [`SandboxProfile::Build`] — the toolchain, `$CARGO_HOME`, `$RUSTUP_HOME` and the workspace
//!   target directory are writable and outbound network is shared for the registry; the rest of
//!   `$HOME` is replaced with an empty tmpfs so the build cannot read private files.
//! - [`SandboxProfile::Strict`] — the workspace is writable, network is unshared, the rest of
//!   `$HOME` is hidden.
//!
//! [`SandboxProfile::Strict`] is the default, so an existing call site keeps the reach it had.
//! A denial is reported as a [`SandboxDenial`] that the caller can turn into a one-command
//! escalation with [`Sandbox::escalate`] instead of switching the sandbox off.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Direct host execution; policy still enforced.
    NoSandbox,
    /// Per-session/per-agent isolation; selective.
    Selective,
    /// All applicable tool execution isolated.
    Full,
}

/// What a sandboxed command is allowed to reach. Named profiles are the only supported way to
/// pick a policy: `Off`, `Build`, `Strict`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProfile {
    /// No filesystem or network restriction.
    Off,
    /// Toolchain and cargo caches writable, network shared, the rest of `$HOME` hidden.
    ///
    /// The caller must pass `allow_network = true` (or [`SandboxProfile::wants_network`]) for the
    /// registry to be reachable: the network flag can only narrow a profile, never widen it.
    Build,
    /// Workspace read/write only, no network, the rest of `$HOME` hidden.
    #[default]
    Strict,
}

impl SandboxProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Build => "build",
            Self::Strict => "strict",
        }
    }

    /// Whether the profile is meant to reach the network. `Build` is; `Strict` and `Off` are not.
    pub fn wants_network(self) -> bool {
        matches!(self, Self::Build)
    }

    /// Accepts the profile names a config file or a CLI flag would carry.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" => Some(Self::Off),
            "build" | "cargo" => Some(Self::Build),
            "strict" | "default" => Some(Self::Strict),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMechanism {
    Bubblewrap,
    Seatbelt,
    Landlock,
    /// Windows Job Objects stub: best-effort child tracking only; no ACL enforced.
    JobObjects,
    PassThrough,
}

/// The access a denial was about. `Unknown` means the sandbox reported a failure but not which
/// operation it refused — say so rather than guessing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    Write,
    Execute,
    Unknown,
}

/// Host paths a profile is resolved against. Resolved once from the environment, or built by
/// hand in tests so the profile mapping stays pure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPaths {
    pub workspace_root: PathBuf,
    pub home: Option<PathBuf>,
    pub cargo_home: Option<PathBuf>,
    pub rustup_home: Option<PathBuf>,
    pub target_dir: PathBuf,
}

impl SandboxPaths {
    pub fn from_env(workspace_root: impl Into<PathBuf>) -> Self {
        let workspace_root = workspace_root.into();
        let home = env_path("HOME").or_else(|| env_path("USERPROFILE"));
        let cargo_home = env_path("CARGO_HOME").or_else(|| home.as_ref().map(|h| h.join(".cargo")));
        let rustup_home =
            env_path("RUSTUP_HOME").or_else(|| home.as_ref().map(|h| h.join(".rustup")));
        let target_dir = match env_path("CARGO_TARGET_DIR") {
            Some(path) if path.is_absolute() => path,
            Some(path) => workspace_root.join(path),
            None => workspace_root.join("target"),
        };
        Self {
            workspace_root,
            home,
            cargo_home,
            rustup_home,
            target_dir,
        }
    }
}

/// The concrete rules a profile resolves to. Pure: no filesystem or environment access, so the
/// mapping is testable without spawning anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxRules {
    pub profile: SandboxProfile,
    pub workspace_root: PathBuf,
    /// Paths mounted writable. The workspace is always the first entry unless the profile is Off.
    pub read_write: Vec<PathBuf>,
    /// Extra read-only mounts beyond the read-only `/` the wrapper already provides.
    pub read_only: Vec<PathBuf>,
    /// When set, this directory is replaced with an empty tmpfs (the rest of `$HOME` is hidden).
    pub hidden_home: Option<PathBuf>,
    pub network: bool,
    /// Present only for `Build`: the cargo target directory that must stay writable.
    pub target_dir: Option<PathBuf>,
}

impl SandboxRules {
    pub fn for_profile(profile: SandboxProfile, paths: &SandboxPaths) -> Self {
        let mut read_write: Vec<PathBuf> = Vec::new();
        let mut target_dir = None;
        let mut hidden_home = None;
        let mut network = true;

        match profile {
            SandboxProfile::Off => {}
            SandboxProfile::Build => {
                push_unique(&mut read_write, paths.workspace_root.clone());
                push_unique(&mut read_write, paths.target_dir.clone());
                if let Some(cargo) = &paths.cargo_home {
                    push_unique(&mut read_write, cargo.clone());
                }
                if let Some(rustup) = &paths.rustup_home {
                    push_unique(&mut read_write, rustup.clone());
                }
                target_dir = Some(paths.target_dir.clone());
                hidden_home = hidden_home_for(paths);
            }
            SandboxProfile::Strict => {
                push_unique(&mut read_write, paths.workspace_root.clone());
                network = false;
                hidden_home = hidden_home_for(paths);
            }
        }

        Self {
            profile,
            workspace_root: paths.workspace_root.clone(),
            read_write,
            read_only: Vec::new(),
            hidden_home,
            network,
            target_dir,
        }
    }

    /// True when the rules grant nothing and deny nothing — only `Off` should be this.
    pub fn is_noop(&self) -> bool {
        self.profile == SandboxProfile::Off
            || (self.read_write.is_empty()
                && self.read_only.is_empty()
                && self.hidden_home.is_none()
                && self.network)
    }

    pub fn allows_write(&self, path: &Path) -> bool {
        if self.profile == SandboxProfile::Off {
            return true;
        }
        self.read_write.iter().any(|root| path.starts_with(root))
    }

    pub fn allows_read(&self, path: &Path) -> bool {
        if self.profile == SandboxProfile::Off {
            return true;
        }
        if self
            .read_write
            .iter()
            .chain(self.read_only.iter())
            .any(|root| path.starts_with(root))
        {
            return true;
        }
        match &self.hidden_home {
            Some(home) => !path.starts_with(home),
            None => true,
        }
    }

    /// The argv bubblewrap needs to realise these rules. Pure apart from reading the environment
    /// variables that must be forwarded to the child.
    pub fn bwrap_args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "--die-with-parent",
            "--new-session",
            "--ro-bind",
            "/",
            "/",
            "--dev-bind-try",
            "/dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        if let Some(home) = &self.hidden_home {
            push_one(&mut args, "--tmpfs", home);
        }
        push_bind(&mut args, "--bind", &self.workspace_root);
        for path in &self.read_write {
            if path != &self.workspace_root {
                push_bind(&mut args, "--bind-try", path);
            }
        }
        for path in &self.read_only {
            push_bind(&mut args, "--ro-bind-try", path);
        }
        if !self.network {
            args.push("--unshare-net".into());
        }

        // The child must not inherit provider keys, tokens, or SSH agent sockets — a sandboxed
        // command with `env` or `/proc/self/environ` would exfiltrate them when network is on.
        args.push("--clearenv".into());
        for var in self.env_passthrough() {
            if let Ok(value) = std::env::var(&var) {
                args.push("--setenv".into());
                args.push(var);
                args.push(value);
            }
        }
        args
    }

    fn env_passthrough(&self) -> Vec<String> {
        let mut vars: Vec<String> = [
            "PATH",
            "HOME",
            "LANG",
            "LC_ALL",
            "TERM",
            "TMPDIR",
            "USER",
            "SHELL",
            "TZ",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        if self.profile == SandboxProfile::Build {
            vars.extend(
                [
                    "CARGO_HOME",
                    "RUSTUP_HOME",
                    "CARGO_TARGET_DIR",
                    "RUSTUP_TOOLCHAIN",
                ]
                .into_iter()
                .map(str::to_owned),
            );
        }
        if self.network {
            vars.extend(
                [
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "NO_PROXY",
                    "http_proxy",
                    "https_proxy",
                ]
                .into_iter()
                .map(str::to_owned),
            );
        }
        vars
    }
}

/// One capability the operator can grant to a single command, without disabling the sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxGrant {
    /// Read-only access to a path.
    ReadPath(PathBuf),
    /// Read-write access to a path.
    WritePath(PathBuf),
    /// Outbound network.
    Network,
}

impl SandboxGrant {
    pub fn describe(&self) -> String {
        match self {
            Self::ReadPath(path) => format!("read {}", path.display()),
            Self::WritePath(path) => format!("write {}", path.display()),
            Self::Network => "outbound network".to_string(),
        }
    }
}

/// What a denial was about. A path is `None` when the sandbox reported a refusal without naming
/// the file — in that case the [`DenialEvidence`] is all the detail that exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeniedCapability {
    Path {
        path: Option<PathBuf>,
        access: Access,
    },
    Network {
        host: Option<String>,
        port: Option<u16>,
    },
    Other {
        capability: String,
    },
}

/// What the sandbox actually told us. Errno is preferred because it is a real observation;
/// `ExitCode` and `Text` are weaker but never invented.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialEvidence {
    Errno {
        errno: i32,
        symbol: String,
        message: String,
    },
    ExitCode {
        code: i32,
    },
    Text {
        message: String,
    },
}

/// A refusal the caller can show the operator and escalate for one command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxDenial {
    pub capability: DeniedCapability,
    /// Always non-empty: what was refused, in words the operator can act on.
    pub reason: String,
    pub evidence: DenialEvidence,
    pub exit_code: Option<i32>,
}

impl SandboxDenial {
    /// Classify a finished command's result. Returns `None` when the failure is not a sandbox
    /// refusal: rules that deny nothing, a write inside an allowed root, or a failure whose text
    /// carries no denial marker. Heuristic on purpose — the markers are the errno strings the
    /// kernel prints, not a parser for every tool's error format.
    pub fn classify(rules: &SandboxRules, exit_code: Option<i32>, stderr: &str) -> Option<Self> {
        if rules.is_noop() || stderr.trim().is_empty() {
            return None;
        }
        let text = stderr.to_ascii_lowercase();
        let errno = errno_for(&text);

        if !rules.network && network_marker(&text) {
            let (host, port) = extract_endpoint(stderr);
            return Some(Self {
                capability: DeniedCapability::Network { host, port },
                reason: format!(
                    "outbound network was refused by the {} sandbox profile{}",
                    rules.profile.as_str(),
                    errno_suffix(errno)
                ),
                evidence: evidence_for(errno, exit_code, stderr),
                exit_code,
            });
        }

        if filesystem_marker(&text) {
            let path = extract_path(stderr);
            if let Some(found) = &path
                && rules.allows_write(found)
            {
                // The path is inside a root the sandbox grants, so this is the command's own
                // failure, not a sandbox refusal.
                return None;
            }
            let access = if text.contains("read-only file system") {
                Access::Write
            } else {
                Access::Unknown
            };
            let reason = match &path {
                Some(found) => format!(
                    "{} access to {} was refused by the {} sandbox profile{}",
                    access_word(access),
                    found.display(),
                    rules.profile.as_str(),
                    errno_suffix(errno)
                ),
                None => format!(
                    "a filesystem {} was refused by the {} sandbox profile; the sandbox did not \
                     report which path{}",
                    access_word(access),
                    rules.profile.as_str(),
                    errno_suffix(errno)
                ),
            };
            return Some(Self {
                capability: DeniedCapability::Path { path, access },
                reason,
                evidence: evidence_for(errno, exit_code, stderr),
                exit_code,
            });
        }

        None
    }

    pub fn path(&self) -> Option<&Path> {
        match &self.capability {
            DeniedCapability::Path { path, .. } => path.as_deref(),
            _ => None,
        }
    }

    /// The single capability that would let this command through, when it can be named. `None`
    /// when the denial did not report enough detail — escalate by re-running outside the sandbox
    /// rather than granting a guess.
    pub fn escalation(&self) -> Option<SandboxGrant> {
        match &self.capability {
            DeniedCapability::Network { .. } => Some(SandboxGrant::Network),
            DeniedCapability::Path {
                path: Some(path),
                access,
            } => match access {
                Access::Read => Some(SandboxGrant::ReadPath(path.clone())),
                Access::Write | Access::Unknown => Some(SandboxGrant::WritePath(path.clone())),
                Access::Execute => None,
            },
            DeniedCapability::Path { path: None, .. } | DeniedCapability::Other { .. } => None,
        }
    }

    pub fn describe(&self) -> String {
        match &self.capability {
            DeniedCapability::Path { path: Some(p), .. } => {
                format!("{} ({})", self.reason, p.display())
            }
            DeniedCapability::Network {
                host: Some(h),
                port: Some(p),
            } => format!("{} ({h}:{p})", self.reason),
            DeniedCapability::Network { host: Some(h), .. } => format!("{} ({h})", self.reason),
            DeniedCapability::Other { capability } => format!("{} ({capability})", self.reason),
            _ => self.reason.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceLimits {
    pub max_memory_bytes: Option<u64>,
    pub max_cpu_seconds: Option<u64>,
    pub max_open_files: Option<u64>,
    pub max_processes: Option<u64>,
}

/// What this host can actually enforce. `enforcing_wrapper` is false when the kernel supports
/// Landlock but nothing in this crate applies it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LandlockStatus {
    pub lsm_present: bool,
    /// Highest ABI the kernel release is known to provide. Derived from the kernel release, not
    /// queried from the kernel — reported as a floor, never as an exact ABI.
    pub abi_at_least: Option<u32>,
    pub kernel_release: Option<String>,
    pub enforcing_wrapper: bool,
}

#[derive(Clone, Debug)]
pub struct Sandbox {
    pub mode: SandboxMode,
    pub profile: SandboxProfile,
    mechanism: SandboxMechanism,
    paths: SandboxPaths,
    network_requested: bool,
    extra_read_only: Vec<PathBuf>,
    grants: Vec<SandboxGrant>,
    passthrough_reason: Option<String>,
    profile_path: Option<PathBuf>,
    resource_limits: ResourceLimits,
}

impl Sandbox {
    pub fn detect(mode: SandboxMode, allow_network: bool, extra_read_only: Vec<PathBuf>) -> Self {
        Self::detect_with_limits(
            mode,
            allow_network,
            extra_read_only,
            ResourceLimits::default(),
        )
    }

    pub fn detect_with_limits(
        mode: SandboxMode,
        allow_network: bool,
        extra_read_only: Vec<PathBuf>,
        limits: ResourceLimits,
    ) -> Self {
        Self::detect_with_profile(
            profile_for_mode(mode),
            allow_network,
            extra_read_only,
            limits,
        )
    }

    /// The entry point for a caller that picks a profile. `allow_network` can only narrow what
    /// the profile grants: `Build` with `false` loses the network, `Strict` with `true` keeps it
    /// unshared. Nothing here widens a profile.
    pub fn detect_with_profile(
        profile: SandboxProfile,
        allow_network: bool,
        extra_read_only: Vec<PathBuf>,
        limits: ResourceLimits,
    ) -> Self {
        let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut paths = SandboxPaths::from_env(workspace_root);
        // A tmpfs mount needs an existing directory; an unset or bogus HOME is simply not hidden.
        if paths.home.as_ref().is_some_and(|home| !home.is_dir()) {
            paths.home = None;
        }

        if profile == SandboxProfile::Off {
            return Self {
                mode: SandboxMode::NoSandbox,
                profile,
                mechanism: SandboxMechanism::PassThrough,
                paths,
                network_requested: true,
                extra_read_only: Vec::new(),
                grants: Vec::new(),
                passthrough_reason: Some("sandbox disabled by profile (off)".into()),
                profile_path: None,
                resource_limits: limits,
            };
        }

        let (mechanism, passthrough_reason) = detect_mechanism();
        let profile_path = (mechanism == SandboxMechanism::Seatbelt)
            .then(seatbelt_profile_file)
            .flatten();

        Self {
            mode: mode_for_profile(profile),
            profile,
            mechanism,
            paths,
            network_requested: allow_network,
            extra_read_only,
            grants: Vec::new(),
            passthrough_reason,
            profile_path,
            resource_limits: limits,
        }
    }

    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            mode: SandboxMode::NoSandbox,
            profile: SandboxProfile::Off,
            mechanism: SandboxMechanism::PassThrough,
            paths: SandboxPaths::from_env("."),
            network_requested: true,
            extra_read_only: Vec::new(),
            grants: Vec::new(),
            passthrough_reason: Some(reason.into()),
            profile_path: None,
            resource_limits: ResourceLimits::default(),
        }
    }

    pub fn mechanism(&self) -> SandboxMechanism {
        self.mechanism
    }
    pub fn mode(&self) -> SandboxMode {
        self.mode
    }
    pub fn profile(&self) -> SandboxProfile {
        self.profile
    }
    pub fn reason(&self) -> Option<&str> {
        self.passthrough_reason.as_deref()
    }
    pub fn limits(&self) -> ResourceLimits {
        self.resource_limits
    }
    pub fn escalated(&self) -> &[SandboxGrant] {
        &self.grants
    }

    /// Rules as they apply to the workspace this sandbox was detected in. `wrap_command` rebuilds
    /// them for the root the caller actually passes.
    pub fn rules(&self) -> SandboxRules {
        self.rules_for(&self.paths.workspace_root)
    }

    /// Rules for an explicit workspace root. Applies the caller's network narrowing, the extra
    /// read-only mounts, and any one-command escalations.
    pub fn rules_for(&self, workspace_root: &Path) -> SandboxRules {
        let mut paths = self.paths.clone();
        if paths.workspace_root != workspace_root {
            if paths.target_dir == paths.workspace_root.join("target") {
                paths.target_dir = workspace_root.join("target");
            }
            paths.workspace_root = workspace_root.to_path_buf();
        }

        let mut rules = SandboxRules::for_profile(self.profile, &paths);
        rules.read_only = self.extra_read_only.clone();
        if self.profile != SandboxProfile::Off {
            rules.network = rules.network && self.network_requested;
        }
        for grant in &self.grants {
            match grant {
                SandboxGrant::ReadPath(path) => push_unique(&mut rules.read_only, path.clone()),
                SandboxGrant::WritePath(path) => push_unique(&mut rules.read_write, path.clone()),
                SandboxGrant::Network => rules.network = true,
            }
        }
        rules
    }

    /// Grant one capability for this sandbox's lifetime. Returns a copy so the original policy is
    /// untouched — the caller hands the escalated copy to exactly one command.
    pub fn escalate(&self, grant: SandboxGrant) -> Self {
        let mut next = self.clone();
        if !next.grants.contains(&grant) {
            next.grants.push(grant);
        }
        next
    }

    /// Classify a finished command. Only an enforcing mechanism can produce a denial: when the
    /// sandbox is pass-through, a failure belongs to the command.
    pub fn classify_denial(&self, exit_code: Option<i32>, stderr: &str) -> Option<SandboxDenial> {
        if !self.is_enforced() {
            return None;
        }
        SandboxDenial::classify(&self.rules(), exit_code, stderr)
    }

    /// What is actually enforced on this platform, including the parts that are not.
    pub fn describe(&self) -> String {
        let rules = self.rules();
        let roots = |paths: &[PathBuf]| {
            if paths.is_empty() {
                "none".to_string()
            } else {
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        let mut lines = vec![format!(
            "mode={:?} profile={} mechanism={:?} enforced={}",
            self.mode,
            self.profile.as_str(),
            self.mechanism,
            self.is_enforced()
        )];
        lines.push(format!(
            "network={}{}",
            if rules.network { "shared" } else { "denied" },
            if rules.network {
                ""
            } else if self.is_enforced() {
                " (unshared netns)"
            } else {
                " (unenforced: pass-through)"
            }
        ));
        lines.push(format!("filesystem rw: {}", roots(&rules.read_write)));
        lines.push(format!("filesystem ro: {}", roots(&rules.read_only)));
        lines.push(format!(
            "home: {}",
            match &rules.hidden_home {
                Some(home) if self.is_enforced() => {
                    format!(
                        "{} hidden (rest of $HOME replaced with an empty tmpfs)",
                        home.display()
                    )
                }
                Some(home) => format!("{} (not hidden: no enforcing mechanism)", home.display()),
                None => "not restricted".to_string(),
            }
        ));
        if !self.grants.is_empty() {
            lines.push(format!(
                "escalated: {}",
                self.grants
                    .iter()
                    .map(SandboxGrant::describe)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines.push(landlock_line());
        if let Some(reason) = &self.passthrough_reason {
            lines.push(format!(
                "unenforced: {reason} — commands run on the host; policy still applies"
            ));
        }
        lines.join("\n")
    }

    /// True only when an OS mechanism is actually armed, not a pass-through or the
    /// Windows Job-Objects stub. Callers gate degraded messaging on this.
    pub fn is_enforced(&self) -> bool {
        matches!(
            self.mechanism,
            SandboxMechanism::Bubblewrap | SandboxMechanism::Seatbelt
        )
    }

    /// Human-readable degradation notice, or None when enforced / no reason given.
    pub fn enforcement_notice(&self) -> Option<String> {
        if self.is_enforced() {
            return None;
        }
        let reason = self.passthrough_reason.clone()?;
        Some(format!(
            "OS sandbox not enforced ({reason}): running on host; policy still applies."
        ))
    }

    /// Name of the platform this binary runs on, for diagnostics.
    pub fn platform() -> &'static str {
        if cfg!(target_os = "android") {
            "android"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else {
            "other"
        }
    }

    /// Wrap `cmd` so it runs inside the sandbox.
    pub fn wrap_command(&self, cmd: &mut Command, workspace_root: &Path) {
        let rules = self.rules_for(workspace_root);
        match self.mechanism {
            SandboxMechanism::Bubblewrap => self.wrap_bubblewrap(cmd, &rules),
            SandboxMechanism::Seatbelt => {
                let Some(profile) = self.profile_path.clone() else {
                    return;
                };
                if self.write_seatbelt_profile(&rules, &profile).is_err() {
                    return;
                }
                rewrap(
                    cmd,
                    "/usr/bin/sandbox-exec",
                    vec!["-f".to_string(), profile.display().to_string()],
                );
            }
            SandboxMechanism::Landlock => {
                // Landlock is applied from inside a wrapper binary that this crate does not ship.
                // Falling through runs on the host, and `describe`/`enforcement_notice` say so.
            }
            SandboxMechanism::JobObjects => {
                // Stub: Windows cannot enforce an ACL from Command; the child runs on the host.
                // Degradation is surfaced via enforcement_notice().
            }
            SandboxMechanism::PassThrough => {}
        }
    }

    fn wrap_bubblewrap(&self, cmd: &mut Command, rules: &SandboxRules) {
        rewrap(cmd, "bwrap", rules.bwrap_args());
    }

    fn write_seatbelt_profile(&self, rules: &SandboxRules, path: &Path) -> std::io::Result<()> {
        let network = if rules.network {
            "\n(allow network*)"
        } else {
            ""
        };
        let writable = rules
            .read_write
            .iter()
            .map(|p| format!("(subpath \"{}\")", p.display()))
            .chain([
                "(subpath \"/tmp\")".to_string(),
                "(subpath \"/private/tmp\")".to_string(),
            ])
            .collect::<Vec<_>>()
            .join(" ");
        let extras = rules
            .read_only
            .iter()
            .map(|p| format!("(allow file-read* (subpath \"{}\"))", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        let profile = format!(
            "(version 1)\n(deny default)\n(allow process-fork)\n(allow sysctl-read)\n(allow file-read*)\n(allow file-write* {writable})\n(allow signal (target self))\n(allow iokit-open)\n{network}\n{extras}\n"
        );
        std::fs::write(path, profile)
    }

    /// Verify that isolation is actually enforced by checking the mechanism is available.
    pub fn verify(&self) -> (bool, String) {
        match self.mechanism {
            SandboxMechanism::Bubblewrap => (bwrap_available(), "bubblewrap available".into()),
            SandboxMechanism::Seatbelt => {
                (sandbox_exec_available(), "sandbox-exec available".into())
            }
            SandboxMechanism::Landlock => {
                let status = landlock_status();
                (
                    false,
                    format!(
                        "landlock lsm present={} but no enforcing wrapper is shipped",
                        status.lsm_present
                    ),
                )
            }
            SandboxMechanism::JobObjects => (
                false,
                self.passthrough_reason
                    .clone()
                    .unwrap_or_else(|| "windows job objects stub only".into()),
            ),
            SandboxMechanism::PassThrough => (
                false,
                self.passthrough_reason
                    .clone()
                    .unwrap_or_else(|| "no isolation".into()),
            ),
        }
    }
}

fn profile_for_mode(mode: SandboxMode) -> SandboxProfile {
    match mode {
        SandboxMode::NoSandbox => SandboxProfile::Off,
        SandboxMode::Selective | SandboxMode::Full => SandboxProfile::Strict,
    }
}

fn mode_for_profile(profile: SandboxProfile) -> SandboxMode {
    match profile {
        SandboxProfile::Off => SandboxMode::NoSandbox,
        SandboxProfile::Build => SandboxMode::Selective,
        SandboxProfile::Strict => SandboxMode::Full,
    }
}

fn detect_mechanism() -> (SandboxMechanism, Option<String>) {
    if cfg!(any(target_os = "linux", target_os = "android")) {
        if bwrap_available() {
            (SandboxMechanism::Bubblewrap, None)
        } else {
            let landlock = landlock_status();
            let detail = if landlock.lsm_present {
                "landlock lsm is present but no enforcing wrapper is shipped"
            } else {
                "landlock is not present in this kernel's LSM list"
            };
            (
                SandboxMechanism::PassThrough,
                Some(format!(
                    "bubblewrap not installed and {detail}; OS isolation inactive"
                )),
            )
        }
    } else if cfg!(target_os = "macos") {
        if sandbox_exec_available() {
            (SandboxMechanism::Seatbelt, None)
        } else {
            (
                SandboxMechanism::PassThrough,
                Some("sandbox-exec not available; OS isolation inactive".into()),
            )
        }
    } else if cfg!(target_os = "windows") {
        // Windows has no supported OS isolation here; Job objects only track the child so it does
        // not outlive the host. Degrade gracefully, never fake it.
        (
            SandboxMechanism::JobObjects,
            Some(
                "windows: Job Objects stub only (child tracking, no ACL); degraded to pass-through"
                    .into(),
            ),
        )
    } else {
        (
            SandboxMechanism::PassThrough,
            Some("platform offers no supported isolation mechanism".into()),
        )
    }
}

fn hidden_home_for(paths: &SandboxPaths) -> Option<PathBuf> {
    let home = paths.home.as_ref()?;
    if home == &paths.workspace_root {
        return None;
    }
    Some(home.clone())
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn push_one(args: &mut Vec<String>, flag: &str, value: &Path) {
    args.push(flag.to_string());
    args.push(value.display().to_string());
}

/// A bind mount takes SRC and DEST; both are the same host path here.
fn push_bind(args: &mut Vec<String>, flag: &str, path: &Path) {
    args.push(flag.to_string());
    args.push(path.display().to_string());
    args.push(path.display().to_string());
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn landlock_line() -> String {
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return "landlock: not applicable on this platform".to_string();
    }
    let status = landlock_status();
    if !status.lsm_present {
        return "landlock: lsm absent".to_string();
    }
    let abi = match status.abi_at_least {
        Some(abi) => format!("abi>={abi}"),
        None => "abi unknown".to_string(),
    };
    let kernel = status
        .kernel_release
        .map(|release| format!(" (derived from kernel {release}, not queried)"))
        .unwrap_or_default();
    format!(
        "landlock: lsm present, {abi}{kernel}, no enforcing wrapper shipped -> not used for enforcement"
    )
}

/// What this host's kernel offers for Landlock. The ABI is derived from the kernel release
/// because querying it needs a syscall this crate does not link; it is reported as a floor.
pub fn landlock_status() -> LandlockStatus {
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return LandlockStatus::default();
    }
    let lsm_present = std::fs::read_to_string("/sys/kernel/security/lsm")
        .map(|list| list.split(',').any(|name| name.trim() == "landlock"))
        .unwrap_or(false);
    let kernel_release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|release| release.trim().to_string())
        .filter(|release| !release.is_empty());
    let abi_at_least = kernel_release.as_deref().and_then(landlock_abi_floor);
    LandlockStatus {
        lsm_present,
        abi_at_least,
        kernel_release,
        enforcing_wrapper: false,
    }
}

/// Landlock ABI introduction points, from the kernel's own documentation. A kernel newer than
/// the last row is reported at the last known ABI.
fn landlock_abi_floor(release: &str) -> Option<u32> {
    const ABI: &[(u32, u32, u32)] = &[
        (5, 13, 1),
        (5, 19, 2),
        (6, 2, 3),
        (6, 7, 4),
        (6, 10, 5),
        (6, 12, 6),
    ];
    let mut parts = release.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let mut floor = None;
    for (maj, min, abi) in ABI {
        if (major, minor) >= (*maj, *min) {
            floor = Some(*abi);
        } else {
            break;
        }
    }
    floor
}

fn errno_for(text: &str) -> Option<(i32, &'static str)> {
    const MARKERS: &[(&str, i32, &str)] = &[
        ("read-only file system", 30, "EROFS"),
        ("operation not permitted", 1, "EPERM"),
        ("permission denied", 13, "EACCES"),
        ("network is unreachable", 101, "ENETUNREACH"),
        ("no route to host", 113, "EHOSTUNREACH"),
        ("connection refused", 111, "ECONNREFUSED"),
    ];
    MARKERS
        .iter()
        .find(|(marker, _, _)| text.contains(marker))
        .map(|(_, errno, symbol)| (*errno, *symbol))
}

fn errno_suffix(errno: Option<(i32, &'static str)>) -> String {
    match errno {
        Some((code, symbol)) => format!(" ({symbol}/{code})"),
        None => String::new(),
    }
}

fn network_marker(text: &str) -> bool {
    [
        "network is unreachable",
        "could not resolve host",
        "temporary failure in name resolution",
        "name or service not known",
        "failed to connect",
        "connection refused",
        "no route to host",
        "enetunreach",
        "eai_again",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn filesystem_marker(text: &str) -> bool {
    [
        "read-only file system",
        "operation not permitted",
        "permission denied",
        "erofs",
        "eperm",
        "eacces",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn evidence_for(
    errno: Option<(i32, &'static str)>,
    exit_code: Option<i32>,
    stderr: &str,
) -> DenialEvidence {
    let message = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string();
    match errno {
        Some((code, symbol)) => DenialEvidence::Errno {
            errno: code,
            symbol: symbol.to_string(),
            message,
        },
        None => match exit_code {
            Some(code) => DenialEvidence::ExitCode { code },
            None => DenialEvidence::Text { message },
        },
    }
}

fn access_word(access: Access) -> &'static str {
    match access {
        Access::Read => "read",
        Access::Write => "write",
        Access::Execute => "execute",
        Access::Unknown => "filesystem access",
    }
}

/// Best-effort: the longest absolute-looking token in the message. Returns `None` rather than a
/// guess when the message names no path.
fn extract_path(stderr: &str) -> Option<PathBuf> {
    let mut best: Option<&str> = None;
    for raw in stderr.split(|c: char| {
        c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '(' | ')' | ',' | ';' | '[' | ']')
    }) {
        let token = raw.trim_end_matches(':');
        if token.starts_with('/') && token.len() > 1 {
            let longer = best.is_none_or(|current| token.len() > current.len());
            if longer {
                best = Some(token);
            }
        }
    }
    best.map(PathBuf::from)
}

fn extract_endpoint(stderr: &str) -> (Option<String>, Option<u16>) {
    let lower = stderr.to_ascii_lowercase();
    let mut host = None;
    let mut port = None;

    // curl: "Failed to connect to <host> port <n> ..."
    if let Some(index) = lower.find(" port ") {
        port = stderr[index + 6..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok();
    }

    // curl: "Failed to connect to <host>:<n>", "Could not resolve host: <host>"
    let anchor = ["connect to ", "resolve host: ", "resolve host "]
        .iter()
        .filter_map(|needle| lower.find(needle).map(|index| (index, needle.len())))
        .min_by_key(|(index, _)| *index);
    if let Some((index, len)) = anchor {
        let token = stderr[index + len..]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_end_matches(['.', ',', ')', ';']);
        match token.rsplit_once(':') {
            Some((name, digits))
                if !name.is_empty()
                    && !name.contains('/')
                    && !digits.is_empty()
                    && digits.chars().all(|c| c.is_ascii_digit()) =>
            {
                host = Some(name.to_string());
                if port.is_none() {
                    port = digits.parse().ok();
                }
            }
            _ if !token.is_empty() && !token.contains('/') => {
                host = Some(token.to_string());
            }
            _ => {}
        }
    }
    (host, port)
}

fn rewrap<S: Into<String>>(cmd: &mut Command, wrapper: &str, preargs: Vec<S>) {
    let previous = std::mem::replace(cmd, Command::new(wrapper));
    let program_ok = previous.get_program().to_str().map(str::to_owned);
    let args: Vec<std::ffi::OsString> = previous.get_args().map(|a| a.to_os_string()).collect();
    let dir = previous.get_current_dir().map(|d| d.to_path_buf());

    let new_cmd = cmd;
    for arg in preargs {
        new_cmd.arg(arg.into());
    }
    match program_ok {
        Some(program) => {
            new_cmd.arg(program);
        }
        None => {
            *new_cmd = previous;
            return;
        }
    }
    for arg in args {
        new_cmd.arg(arg);
    }
    if let Some(dir) = dir
        && dir.to_str().is_some()
    {
        new_cmd.current_dir(dir);
    }
}

fn bwrap_available() -> bool {
    Command::new("bwrap")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn sandbox_exec_available() -> bool {
    Path::new("/usr/bin/sandbox-exec").exists()
}

fn seatbelt_profile_file() -> Option<PathBuf> {
    let name = format!("vikram-seatbelt-{}.sb", std::process::id());
    Some(std::env::temp_dir().join(name))
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(path) = &self.profile_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> SandboxPaths {
        SandboxPaths {
            workspace_root: PathBuf::from("/work/project"),
            home: Some(PathBuf::from("/home/dev")),
            cargo_home: Some(PathBuf::from("/home/dev/.cargo")),
            rustup_home: Some(PathBuf::from("/home/dev/.rustup")),
            target_dir: PathBuf::from("/work/project/target"),
        }
    }

    #[test]
    fn strict_is_the_default_profile() {
        assert_eq!(SandboxProfile::default(), SandboxProfile::Strict);
        assert_eq!(profile_for_mode(SandboxMode::Full), SandboxProfile::Strict);
        assert_eq!(
            profile_for_mode(SandboxMode::NoSandbox),
            SandboxProfile::Off
        );
    }

    #[test]
    fn only_build_wants_the_network() {
        assert!(SandboxProfile::Build.wants_network());
        assert!(!SandboxProfile::Strict.wants_network());
        assert!(!SandboxProfile::Off.wants_network());
    }

    #[test]
    fn profile_parses_from_a_config_value() {
        assert_eq!(SandboxProfile::parse("Build"), Some(SandboxProfile::Build));
        assert_eq!(
            SandboxProfile::parse(" strict "),
            Some(SandboxProfile::Strict)
        );
        assert_eq!(SandboxProfile::parse("off"), Some(SandboxProfile::Off));
        assert_eq!(SandboxProfile::parse("yolo"), None);
    }

    #[test]
    fn off_profile_denies_nothing() {
        let rules = SandboxRules::for_profile(SandboxProfile::Off, &paths());
        assert!(rules.is_noop());
        assert!(rules.network);
        assert!(rules.read_write.is_empty());
        assert!(rules.hidden_home.is_none());
        assert!(rules.allows_write(Path::new("/etc/shadow")));
        assert!(!rules.bwrap_args().contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn build_profile_grants_cargo_home_target_and_network() {
        let rules = SandboxRules::for_profile(SandboxProfile::Build, &paths());
        assert!(rules.network);
        assert_eq!(
            rules.read_write,
            vec![
                PathBuf::from("/work/project"),
                PathBuf::from("/work/project/target"),
                PathBuf::from("/home/dev/.cargo"),
                PathBuf::from("/home/dev/.rustup"),
            ]
        );
        assert_eq!(
            rules.target_dir.as_deref(),
            Some(Path::new("/work/project/target"))
        );
        assert_eq!(rules.hidden_home.as_deref(), Some(Path::new("/home/dev")));
        assert!(rules.allows_write(Path::new("/home/dev/.cargo/registry/cache/x.crate")));
        assert!(rules.allows_write(Path::new("/work/project/target/debug/deps")));
        assert!(!rules.allows_write(Path::new("/home/dev/Documents/notes.md")));
        assert!(!rules.allows_read(Path::new("/home/dev/Documents/notes.md")));
        assert!(rules.allows_read(Path::new("/home/dev/.cargo/config.toml")));
        assert!(rules.allows_read(Path::new("/usr/lib/libc.so")));
    }

    #[test]
    fn strict_profile_grants_only_the_workspace_and_no_network() {
        let rules = SandboxRules::for_profile(SandboxProfile::Strict, &paths());
        assert_eq!(rules.read_write, vec![PathBuf::from("/work/project")]);
        assert!(!rules.network);
        assert!(rules.target_dir.is_none());
        assert!(!rules.allows_write(Path::new("/home/dev/.cargo")));
        assert!(
            !rules.allows_write(Path::new("/home/dev/target")),
            "a target dir outside the workspace is not granted"
        );
        assert_eq!(rules.hidden_home.as_deref(), Some(Path::new("/home/dev")));
        assert!(rules.bwrap_args().contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn build_profile_binds_cargo_home_after_hiding_home() {
        let rules = SandboxRules::for_profile(SandboxProfile::Build, &paths());
        let args = rules.bwrap_args();
        let tmpfs_home = args
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && w[1] == "/home/dev")
            .expect("home hidden with a tmpfs");
        let cargo = args
            .windows(3)
            .position(|w| {
                w[0] == "--bind-try" && w[1] == "/home/dev/.cargo" && w[2] == "/home/dev/.cargo"
            })
            .expect("cargo home bound as SRC and DEST");
        assert!(
            cargo > tmpfs_home,
            "cargo home must be bound over the tmpfs"
        );
        assert!(!args.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn workspace_inside_home_is_still_writable_after_the_home_tmpfs() {
        let mut p = paths();
        p.workspace_root = PathBuf::from("/home/dev/code/app");
        p.target_dir = PathBuf::from("/home/dev/code/app/target");
        let rules = SandboxRules::for_profile(SandboxProfile::Strict, &p);
        assert_eq!(rules.hidden_home.as_deref(), Some(Path::new("/home/dev")));
        let args = rules.bwrap_args();
        let home = args
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && w[1] == "/home/dev")
            .expect("home hidden");
        let workspace = args
            .windows(3)
            .position(|w| {
                w[0] == "--bind" && w[1] == "/home/dev/code/app" && w[2] == "/home/dev/code/app"
            })
            .expect("workspace bound as SRC and DEST");
        assert!(workspace > home);
    }

    #[test]
    fn caller_can_only_narrow_the_profile() {
        let strict = Sandbox::detect_with_profile(
            SandboxProfile::Strict,
            true,
            vec![],
            ResourceLimits::default(),
        );
        assert!(!strict.rules().network, "strict must not be widened");

        let build_narrowed = Sandbox::detect_with_profile(
            SandboxProfile::Build,
            false,
            vec![],
            ResourceLimits::default(),
        );
        assert!(!build_narrowed.rules().network, "build may be narrowed");

        let build = Sandbox::detect_with_profile(
            SandboxProfile::Build,
            true,
            vec![],
            ResourceLimits::default(),
        );
        assert!(build.rules().network);
    }

    #[test]
    fn extra_read_only_mounts_are_applied() {
        let sandbox = Sandbox::detect_with_profile(
            SandboxProfile::Strict,
            false,
            vec![PathBuf::from("/opt/vendor")],
            ResourceLimits::default(),
        );
        assert_eq!(
            sandbox.rules().read_only,
            vec![PathBuf::from("/opt/vendor")]
        );
        assert!(!sandbox.rules().allows_write(Path::new("/opt/vendor/x")));
        assert!(sandbox.rules().allows_read(Path::new("/opt/vendor/x")));
    }

    #[test]
    fn escalation_grants_exactly_one_capability() {
        let strict = Sandbox::detect_with_profile(
            SandboxProfile::Strict,
            false,
            vec![],
            ResourceLimits::default(),
        );
        let networked = strict.escalate(SandboxGrant::Network);
        assert!(networked.rules().network);
        assert!(!strict.rules().network, "the original policy is untouched");
        assert_eq!(networked.escalated(), &[SandboxGrant::Network]);

        let with_cache =
            strict.escalate(SandboxGrant::WritePath(PathBuf::from("/home/dev/.cargo")));
        assert!(
            with_cache
                .rules()
                .allows_write(Path::new("/home/dev/.cargo"))
        );
        assert!(!with_cache.rules().network);
        assert_eq!(with_cache.rules().read_write.len(), 2);
    }

    #[test]
    fn a_denial_carries_a_reason_and_the_path() {
        let rules = SandboxRules::for_profile(SandboxProfile::Strict, &paths());
        let denial = SandboxDenial::classify(
            &rules,
            Some(101),
            "error: failed to write /home/dev/.cargo/registry/index: \
             Read-only file system (os error 30)",
        )
        .expect("a denial");
        assert!(matches!(
            denial.capability,
            DeniedCapability::Path {
                access: Access::Write,
                ..
            }
        ));
        assert_eq!(
            denial.path(),
            Some(Path::new("/home/dev/.cargo/registry/index"))
        );
        assert!(denial.reason.contains("strict"));
        assert!(denial.reason.contains("EROFS"));
        assert!(!denial.reason.is_empty());
        assert!(matches!(
            denial.evidence,
            DenialEvidence::Errno { errno: 30, .. }
        ));
        assert_eq!(
            denial.escalation(),
            Some(SandboxGrant::WritePath(PathBuf::from(
                "/home/dev/.cargo/registry/index"
            )))
        );
    }

    #[test]
    fn a_network_denial_names_the_endpoint_and_escalates_to_network() {
        let rules = SandboxRules::for_profile(SandboxProfile::Strict, &paths());
        let denial = SandboxDenial::classify(
            &rules,
            Some(7),
            "curl: (7) Failed to connect to index.crates.io port 443 after 0 ms: \
             Network is unreachable",
        )
        .expect("a denial");
        match &denial.capability {
            DeniedCapability::Network { host, port } => {
                assert_eq!(host.as_deref(), Some("index.crates.io"));
                assert_eq!(*port, Some(443));
            }
            other => panic!("expected a network denial, got {other:?}"),
        }
        assert_eq!(denial.escalation(), Some(SandboxGrant::Network));
        assert!(!denial.reason.is_empty());
    }

    #[test]
    fn a_denial_without_a_reported_path_says_so() {
        let rules = SandboxRules::for_profile(SandboxProfile::Strict, &paths());
        let denial = SandboxDenial::classify(&rules, Some(1), "sh: 1: Operation not permitted")
            .expect("a denial");
        assert_eq!(denial.path(), None);
        assert!(denial.reason.contains("did not report which path"));
        assert!(matches!(
            denial.evidence,
            DenialEvidence::Errno {
                errno: 1,
                ref symbol,
                ..
            } if symbol == "EPERM"
        ));
        assert_eq!(denial.escalation(), None, "no path means no honest grant");
    }

    #[test]
    fn failures_inside_an_allowed_root_are_not_sandbox_denials() {
        let rules = SandboxRules::for_profile(SandboxProfile::Strict, &paths());
        assert!(
            SandboxDenial::classify(
                &rules,
                Some(1),
                "failed to write /work/project/src/main.rs: Read-only file system"
            )
            .is_none()
        );
        assert!(
            SandboxDenial::classify(&rules, Some(1), "error: could not compile `serde`").is_none()
        );
    }

    #[test]
    fn an_unenforced_sandbox_never_reports_a_denial() {
        let sandbox = Sandbox::disabled("off for the test");
        assert!(
            sandbox
                .classify_denial(
                    Some(1),
                    "failed to write /home/dev/.cargo: Read-only file system"
                )
                .is_none()
        );
    }

    #[test]
    fn describe_is_honest_about_enforcement() {
        let sandbox = Sandbox::disabled("test");
        let report = sandbox.describe();
        assert!(report.contains("profile=off"));
        assert!(report.contains("enforced=false"));
        assert!(report.contains("unenforced:"));
        assert!(report.contains("mode=NoSandbox"));
        assert!(!report.contains("enforced=true"));
    }

    #[test]
    fn describe_names_the_profile_and_network_state() {
        let sandbox = Sandbox::detect_with_profile(
            SandboxProfile::Build,
            true,
            vec![],
            ResourceLimits::default(),
        );
        let report = sandbox.describe();
        assert!(report.contains("profile=build"));
        assert!(report.contains("network=shared"));
        assert!(report.contains("filesystem rw:"));
        assert!(report.contains("enforced="));
        let strict = Sandbox::detect_with_profile(
            SandboxProfile::Strict,
            false,
            vec![],
            ResourceLimits::default(),
        );
        assert!(strict.describe().contains("network=denied"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn describe_reports_landlock_as_available_but_unused() {
        let status = landlock_status();
        let report = Sandbox::detect_with_profile(
            SandboxProfile::Strict,
            false,
            vec![],
            ResourceLimits::default(),
        )
        .describe();
        if status.lsm_present {
            assert!(report.contains("landlock: lsm present"));
            assert!(report.contains("not used for enforcement"));
        } else {
            assert!(report.contains("landlock: lsm absent"));
        }
        if let Some(release) = &status.kernel_release {
            assert!(release.starts_with(|c: char| c.is_ascii_digit()));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_abi_floor_tracks_the_kernel_release() {
        assert_eq!(landlock_abi_floor("5.10.0"), None);
        assert_eq!(landlock_abi_floor("5.13.0"), Some(1));
        assert_eq!(landlock_abi_floor("6.2.1"), Some(3));
        assert_eq!(landlock_abi_floor("7.2.15-1-omarchy"), Some(6));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_matches_the_mechanism() {
        let sandbox = Sandbox::detect_with_profile(
            SandboxProfile::Strict,
            false,
            vec![],
            ResourceLimits::default(),
        );
        let (ok, message) = sandbox.verify();
        assert_eq!(ok, sandbox.is_enforced());
        assert!(!message.is_empty());
    }

    #[test]
    fn verify_reports_honestly() {
        let sandbox = Sandbox::disabled("test");
        let (ok, _) = sandbox.verify();
        assert!(!ok);
    }

    #[test]
    fn disabled_is_not_enforced_and_has_notice() {
        let sandbox = Sandbox::disabled("off");
        assert!(!sandbox.is_enforced());
        assert!(
            sandbox
                .enforcement_notice()
                .unwrap()
                .contains("not enforced")
        );
    }

    #[test]
    fn no_sandbox_mode_is_passthrough() {
        let sandbox = Sandbox::detect(SandboxMode::NoSandbox, true, vec![]);
        assert_eq!(sandbox.mechanism(), SandboxMechanism::PassThrough);
        assert_eq!(sandbox.profile(), SandboxProfile::Off);
        assert!(sandbox.reason().is_some());
    }

    #[test]
    fn full_mode_detects_something_honest() {
        let sandbox = Sandbox::detect(SandboxMode::Full, false, vec![]);
        assert!(!sandbox.is_enforced() || sandbox.enforcement_notice().is_none());
        let (ok, msg) = sandbox.verify();
        assert_eq!(ok, sandbox.is_enforced());
        assert!(!msg.is_empty());
    }

    #[test]
    fn no_sandbox_mode_reports_platform() {
        assert!(matches!(
            Sandbox::platform(),
            "linux" | "macos" | "windows" | "other"
        ));
    }

    #[test]
    fn endpoint_extraction_handles_the_shapes_tools_actually_print() {
        assert_eq!(
            extract_endpoint("Could not resolve host: crates.io"),
            (Some("crates.io".to_string()), None)
        );
        assert_eq!(
            extract_endpoint(
                "curl: (7) Failed to connect to index.crates.io port 443 after 0 ms: \
                 Network is unreachable"
            ),
            (Some("index.crates.io".to_string()), Some(443))
        );
        assert_eq!(
            extract_endpoint(
                "curl: (7) Failed to connect to index.crates.io:443 after 3 ms: \
                 Could not connect to server"
            ),
            (Some("index.crates.io".to_string()), Some(443))
        );
        assert_eq!(extract_endpoint("no endpoint here"), (None, None));
    }

    #[test]
    fn wrap_command_rewrites_the_argv_for_the_caller_workspace() {
        let sandbox = Sandbox::detect_with_profile(
            SandboxProfile::Strict,
            false,
            vec![],
            ResourceLimits::default(),
        );
        if sandbox.mechanism() != SandboxMechanism::Bubblewrap {
            return;
        }
        let mut cmd = Command::new("true");
        sandbox.wrap_command(&mut cmd, Path::new("/work/project"));
        assert_eq!(cmd.get_program(), "bwrap");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(3)
                .any(|w| w[0] == "--bind" && w[1] == "/work/project" && w[2] == "/work/project")
        );
    }

    /// Real jail, run by hand: `cargo test -p vikram-sandbox -- --ignored`.
    #[test]
    #[ignore = "spawns a real bubblewrap jail; run with --ignored"]
    fn cargo_home_is_writable_inside_the_jail() {
        let sandbox = Sandbox::detect_with_profile(
            SandboxProfile::Build,
            true,
            vec![],
            ResourceLimits::default(),
        );
        if !sandbox.is_enforced() {
            eprintln!("no enforcing mechanism on this host; nothing to test");
            return;
        }
        let home = std::env::var("HOME").expect("HOME");
        let marker = PathBuf::from(&home).join(".vikram-sandbox-marker");
        std::fs::write(&marker, b"x").expect("write marker on the host");

        let script = "set -e; \
            mkdir -p \"$HOME/.cargo\"; \
            echo probe > \"$HOME/.cargo/vikram-jail-probe\"; \
            rm -f \"$HOME/.cargo/vikram-jail-probe\"; \
            echo CARGO_HOME_WRITABLE; \
            if [ -e \"$HOME/.vikram-sandbox-marker\" ]; then echo HOME_VISIBLE; else echo HOME_HIDDEN; fi";
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        sandbox.wrap_command(&mut cmd, &std::env::current_dir().expect("cwd"));
        let out = cmd.output().expect("spawn the jailed shell");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        eprintln!(
            "status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            out.status
        );
        let _ = std::fs::remove_file(&marker);

        assert!(out.status.success(), "jail command failed: {stderr}");
        assert!(stdout.contains("CARGO_HOME_WRITABLE"), "stdout: {stdout}");
        assert!(stdout.contains("HOME_HIDDEN"), "stdout: {stdout}");
    }

    /// Real jail, run by hand: `cargo test -p vikram-sandbox -- --ignored`.
    #[test]
    #[ignore = "spawns a real bubblewrap jail; run with --ignored"]
    fn strict_profile_denies_network_inside_the_jail() {
        let sandbox = Sandbox::detect_with_profile(
            SandboxProfile::Strict,
            false,
            vec![],
            ResourceLimits::default(),
        );
        if !sandbox.is_enforced() {
            eprintln!("no enforcing mechanism on this host; nothing to test");
            return;
        }
        let script = "curl -sS --max-time 3 https://index.crates.io/ -o /dev/null \
            && echo NET_OK || echo NET_DENIED";
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        sandbox.wrap_command(&mut cmd, &std::env::current_dir().expect("cwd"));
        let out = cmd.output().expect("spawn the jailed shell");
        let stdout = String::from_utf8_lossy(&out.stdout);
        eprintln!("status={:?}\nstdout:\n{stdout}", out.status);
        assert!(stdout.contains("NET_DENIED"), "stdout: {stdout}");
    }
}
