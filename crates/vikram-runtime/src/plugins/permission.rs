/// A capability a plugin declares in its manifest's `permissions` list.
///
/// The vocabulary is deliberately closed. [`parse_permission`] is the only way in, so a plugin
/// cannot invent a capability by typo, and [`PluginRegistry::validate`] reports spellings outside
/// this set instead of quietly trusting them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluginPermission {
    FilesystemRead,
    FilesystemWrite,
    Network,
    Process,
    Environment,
    Secrets,
    Skills,
    Tools,
    Agents,
    Hive,
    Bots,
    Ui,
    Background,
}

impl PluginPermission {
    /// Every permission, in the canonical order.
    pub const ALL: [PluginPermission; 13] = [
        PluginPermission::FilesystemRead,
        PluginPermission::FilesystemWrite,
        PluginPermission::Network,
        PluginPermission::Process,
        PluginPermission::Environment,
        PluginPermission::Secrets,
        PluginPermission::Skills,
        PluginPermission::Tools,
        PluginPermission::Agents,
        PluginPermission::Hive,
        PluginPermission::Bots,
        PluginPermission::Ui,
        PluginPermission::Background,
    ];

    /// The canonical spelling, which [`parse_permission`] accepts back.
    pub fn as_str(self) -> &'static str {
        match self {
            PluginPermission::FilesystemRead => "fs.read",
            PluginPermission::FilesystemWrite => "fs.write",
            PluginPermission::Network => "network",
            PluginPermission::Process => "process",
            PluginPermission::Environment => "env",
            PluginPermission::Secrets => "secrets",
            PluginPermission::Skills => "skills",
            PluginPermission::Tools => "tools",
            PluginPermission::Agents => "agents",
            PluginPermission::Hive => "hive",
            PluginPermission::Bots => "bots",
            PluginPermission::Ui => "ui",
            PluginPermission::Background => "background",
        }
    }

    /// Does this permission allow a plugin's entry point to run at all?
    ///
    /// Only `process` and `network` do: a plugin that merely reads files or exposes components is
    /// not allowed to spawn its entry script.
    pub fn allows_execution(self) -> bool {
        matches!(self, PluginPermission::Process | PluginPermission::Network)
    }
}

/// Parse a declared permission string into the closed vocabulary.
///
/// Accepted spellings are case-insensitive, tolerate `_`/`:` in place of `.`, and ignore a
/// trailing `:scope` suffix (`fs.read:workspace` is `fs.read`). An unrecognised spelling is
/// `None` — never a grant.
pub fn parse_permission(raw: &str) -> Option<PluginPermission> {
    canonical_permission(raw).or_else(|| {
        let bare = raw.split(':').next().unwrap_or_default();
        canonical_permission(bare)
    })
}

fn canonical_permission(spelling: &str) -> Option<PluginPermission> {
    let normalized = spelling
        .trim()
        .to_ascii_lowercase()
        .replace(['_', ':'], ".");
    match normalized.as_str() {
        "fs.read" | "filesystem.read" => Some(PluginPermission::FilesystemRead),
        "fs.write" | "filesystem.write" => Some(PluginPermission::FilesystemWrite),
        "network" => Some(PluginPermission::Network),
        "process" | "exec" => Some(PluginPermission::Process),
        "env" | "environment" => Some(PluginPermission::Environment),
        "secrets" => Some(PluginPermission::Secrets),
        "skills" => Some(PluginPermission::Skills),
        "tools" => Some(PluginPermission::Tools),
        "agents" => Some(PluginPermission::Agents),
        "hive" => Some(PluginPermission::Hive),
        "bots" => Some(PluginPermission::Bots),
        "ui" | "tui" => Some(PluginPermission::Ui),
        "background" => Some(PluginPermission::Background),
        _ => None,
    }
}

/// Parse a manifest's declared permissions, dropping duplicates and unknown spellings.
///
/// Unknown spellings are surfaced by [`PluginRegistry::validate`], not here: this function only
/// answers "what was actually granted".
pub fn parse_permissions(declared: &[String]) -> Vec<PluginPermission> {
    let mut granted: Vec<PluginPermission> = Vec::new();
    for raw in declared {
        if let Some(permission) = parse_permission(raw)
            && !granted.contains(&permission)
        {
            granted.push(permission);
        }
    }
    granted
}

/// Refuse execution unless the manifest declares `process` or `network`.
///
/// Silence is not consent: a manifest with no `permissions` key at all is refused, because the
/// legacy three-field shape never asked for the right to run.
pub(crate) fn ensure_execution_permitted(
    plugin_name: &str,
    manifest: &PluginManifest,
) -> Result<(), String> {
    if manifest.permissions.is_empty() {
        return Err(format!(
            "plugin `{plugin_name}` declares no permissions; execution refused \
             (add `process` to plugin.json permissions to allow it)"
        ));
    }
    if !parse_permissions(&manifest.permissions)
        .iter()
        .any(|permission| permission.allows_execution())
    {
        return Err(format!(
            "plugin `{plugin_name}` declares no execution permission (add `process`)"
        ));
    }
    Ok(())
}

use crate::plugins::*;
