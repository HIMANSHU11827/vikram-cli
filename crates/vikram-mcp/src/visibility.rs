//! Visibility management — Private/Public per-server toggle.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Visibility {
    #[default]
    Private,
    Public,
}

impl Visibility {
    pub fn label(&self) -> &'static str {
        match self {
            Visibility::Private => "🔒 Private",
            Visibility::Public => "🌐 Public",
        }
    }

    pub fn is_public(&self) -> bool {
        matches!(self, Visibility::Public)
    }
}

/// Per-server visibility state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerVisibility {
    pub server_name: String,
    pub visibility: Visibility,
    pub exposed_tools: Vec<String>,
    pub authorized_clients: Vec<String>,
}

impl ServerVisibility {
    pub fn new(server_name: String) -> Self {
        Self {
            server_name,
            visibility: Visibility::Private,
            exposed_tools: Vec::new(),
            authorized_clients: Vec::new(),
        }
    }
}

/// Visibility manager — tracks and persists per-server visibility.
pub struct VisibilityManager {
    entries: std::collections::HashMap<String, ServerVisibility>,
    persist_path: Option<std::path::PathBuf>,
}

impl VisibilityManager {
    /// An in-memory manager with no persistence. Call [`VisibilityManager::save`]
    /// explicitly, or use [`VisibilityManager::with_persist_path`] to load
    /// existing entries from disk and persist every mutation.
    pub fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            persist_path: None,
        }
    }

    pub fn with_persist_path(path: std::path::PathBuf) -> Self {
        let mut vm = Self {
            entries: std::collections::HashMap::new(),
            persist_path: Some(path.clone()),
        };
        if let Ok(raw) = std::fs::read_to_string(&path)
            && let Ok(entries) =
                serde_json::from_str::<std::collections::HashMap<String, ServerVisibility>>(&raw)
        {
            vm.entries = entries;
        }
        vm
    }

    /// Load visibility state from disk.
    pub fn load(path: &std::path::Path) -> Self {
        let mut vm = Self::with_persist_path(path.to_path_buf());
        if let Ok(raw) = std::fs::read_to_string(path)
            && let Ok(entries) =
                serde_json::from_str::<std::collections::HashMap<String, ServerVisibility>>(&raw)
        {
            vm.entries = entries;
        }
        vm
    }

    /// Save visibility state to disk (if persist_path is set).
    pub fn save(&self) -> Result<(), String> {
        if let Some(path) = &self.persist_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let json = serde_json::to_string_pretty(&self.entries).map_err(|e| e.to_string())?;
            std::fs::write(path, json).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Set the persist path after construction. Existing entries on disk are
    /// loaded so a manager created with `new()` and then pointed at a file
    /// does not clobber state written by an earlier process.
    pub fn set_persist_path(&mut self, path: std::path::PathBuf) {
        self.persist_path = Some(path.clone());
        if let Ok(raw) = std::fs::read_to_string(&path)
            && let Ok(entries) =
                serde_json::from_str::<std::collections::HashMap<String, ServerVisibility>>(&raw)
        {
            self.entries = entries;
        }
    }

    pub fn get(&self, name: &str) -> Option<&ServerVisibility> {
        self.entries.get(name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut ServerVisibility> {
        self.entries.get_mut(name)
    }

    pub fn set_visibility(&mut self, name: &str, visibility: Visibility) {
        if let Some(entry) = self.entries.get_mut(name) {
            entry.visibility = visibility;
        } else {
            let mut entry = ServerVisibility::new(name.to_string());
            entry.visibility = visibility;
            self.entries.insert(name.to_string(), entry);
        }
        let _ = self.save();
    }

    /// Set the exposed-tool list for `name` and persist it, exactly like
    /// [`VisibilityManager::set_visibility`]. Previously this mutated only
    /// in-memory state, so exposed tools silently reverted on restart.
    pub fn set_exposed_tools(&mut self, name: &str, tools: Vec<String>) {
        if let Some(entry) = self.entries.get_mut(name) {
            entry.exposed_tools = tools;
        } else {
            let mut entry = ServerVisibility::new(name.to_string());
            entry.exposed_tools = tools;
            self.entries.insert(name.to_string(), entry);
        }
        let _ = self.save();
    }

    pub fn is_public(&self, name: &str) -> bool {
        self.entries
            .get(name)
            .map(|e| e.visibility.is_public())
            .unwrap_or(false)
    }

    pub fn list(&self) -> Vec<&ServerVisibility> {
        self.entries.values().collect()
    }

    /// Register a server for visibility tracking and persist the new entry.
    /// Registering an already-known server is a no-op (and does not rewrite
    /// the file), so existing visibility/exposed-tool state is preserved.
    pub fn register(&mut self, name: String) {
        if !self.entries.contains_key(&name) {
            self.entries
                .insert(name.clone(), ServerVisibility::new(name));
            let _ = self.save();
        }
    }
}

impl Default for VisibilityManager {
    fn default() -> Self {
        Self::new()
    }
}
