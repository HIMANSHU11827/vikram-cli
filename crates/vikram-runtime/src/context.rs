//! Centralized, permission-aware context loader.
//!
//! Markdown files are *context resources*, not the database. This module resolves which files a
//! given request needs, reads them with caching and file-change detection, applies a token budget,
//! and never hands out content the caller is not authorized to see.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use vikram_contracts::UserId;

/// The kinds of Markdown context file the harness understands. Order matters: identity first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContextFile {
    Soul,
    Instructions,
    Memory,
    Tools,
    User,
    Preferences,
    Hive,
    Team,
    Agents,
    Harness,
    Skill,
}

impl ContextFile {
    pub fn filename(self) -> &'static str {
        match self {
            Self::Soul => "SOUL.md",
            Self::Instructions => "INSTRUCTIONS.md",
            Self::Memory => "MEMORY.md",
            Self::Tools => "TOOLS.md",
            Self::User => "USER.md",
            Self::Preferences => "PREFERENCES.md",
            Self::Hive => "HIVE.md",
            Self::Team => "TEAM.md",
            Self::Agents => "AGENTS.md",
            Self::Harness => "HARNESS.md",
            Self::Skill => "SKILL.md",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ContextSection {
    pub file: ContextFile,
    pub path: PathBuf,
    pub content: String,
    pub tokens: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ContextBundle {
    pub sections: Vec<ContextSection>,
}

impl ContextBundle {
    pub fn total_tokens(&self) -> u64 {
        self.sections.iter().map(|s| s.tokens).sum()
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for s in &self.sections {
            if !s.content.trim().is_empty() {
                out.push_str(&format!("# {}\n\n", s.file.filename()));
                out.push_str(s.content.trim());
                out.push_str("\n\n");
            }
        }
        out
    }
}

#[derive(Clone, Debug)]
struct CachedFile {
    content: String,
    mtime: Option<SystemTime>,
}

pub struct ContextLoader {
    /// Root of all Vikram state (vikram home).
    base: PathBuf,
    /// Workspace root for project-level AGENTS.md discovery.
    workspace: PathBuf,
    cache: Mutex<HashMap<PathBuf, CachedFile>>,
    /// Hard ceiling on tokens assembled into one bundle.
    token_budget: u64,
}

impl ContextLoader {
    pub fn new(base: PathBuf, workspace: PathBuf) -> Self {
        Self {
            base,
            workspace,
            cache: Mutex::new(HashMap::new()),
            token_budget: 128_000,
        }
    }

    pub fn with_budget(mut self, budget: u64) -> Self {
        self.token_budget = budget;
        self
    }

    /// Resolve the directory an entity's context files live in.
    /// Shared entities (owner None) live under `<base>/<kind-folder>/<slug>/`.
    /// Owned entities live under `<base>/users/<user>/<kind-folder>/<slug>/`.
    pub fn entity_dir(&self, kind_folder: &str, slug: &str, owner: Option<&UserId>) -> PathBuf {
        match owner {
            Some(u) => self
                .base
                .join("users")
                .join(u.as_str())
                .join(kind_folder)
                .join(slug),
            None => self.base.join(kind_folder).join(slug),
        }
    }

    fn cache_lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, CachedFile>> {
        let guard = self.cache.lock();
        match guard {
            Ok(g) => g,
            Err(poisoned) => {
                eprintln!("vikram: context cache mutex poisoned; recovering inner map");
                poisoned.into_inner()
            }
        }
    }

    /// Read one file with caching + mtime-based invalidation. Returns None if missing.
    /// A poisoned cache lock no longer swallows the read: it recovers the inner map.
    pub fn read_file(&self, path: &Path) -> Option<String> {
        // Fast path: cache hit with unchanged mtime.
        let mtime = fs::metadata(path).ok().and_then(|m| m.modified().ok());
        {
            let cache = self.cache_lock();
            if let Some(cached) = cache.get(path)
                && cached.mtime == mtime
            {
                return Some(cached.content.clone());
            }
        }
        // Read from disk. A read error other than NotFound is logged, not silently dropped.
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                eprintln!("vikram: context file {} unreadable: {e}", path.display());
                return None;
            }
        };
        let mut cache = self.cache_lock();
        cache.insert(
            path.to_path_buf(),
            CachedFile {
                content: content.clone(),
                mtime,
            },
        );
        Some(content)
    }

    /// Invalidate the cache entry for a path (call after an edit).
    pub fn invalidate(&self, path: &Path) {
        self.cache_lock().remove(path);
    }

    /// Invalidate everything (e.g. on profile switch).
    pub fn invalidate_all(&self) {
        self.cache_lock().clear();
    }

    fn try_add(&self, bundle: &mut ContextBundle, file: ContextFile, path: PathBuf) -> bool {
        if bundle.total_tokens() >= self.token_budget {
            return false;
        }
        if let Some(content) = self.read_file(&path) {
            // Duplicate prevention: skip if identical content already present.
            if bundle.sections.iter().any(|s| s.content == content) {
                return false;
            }
            let tokens = vikram_contracts::TokenMeter::estimate(&content);
            bundle.sections.push(ContextSection {
                file,
                path,
                content,
                tokens,
            });
            true
        } else {
            false
        }
    }

    /// Load context for a Main Agent entity.
    pub fn load_main_agent(
        &self,
        slug: &str,
        owner: Option<&UserId>,
        user: Option<&UserId>,
        profile_prefs_path: Option<&Path>,
    ) -> ContextBundle {
        let mut bundle = ContextBundle::default();
        let dir = self.entity_dir("main-agents", slug, owner);

        // Identity & instructions first.
        self.try_add(&mut bundle, ContextFile::Soul, dir.join("SOUL.md"));
        self.try_add(
            &mut bundle,
            ContextFile::Instructions,
            dir.join("INSTRUCTIONS.md"),
        );
        self.try_add(&mut bundle, ContextFile::Memory, dir.join("MEMORY.md"));
        self.try_add(&mut bundle, ContextFile::Tools, dir.join("TOOLS.md"));

        // User context (authorized).
        if let Some(u) = user {
            let user_dir = self.base.join("users").join(u.as_str());
            self.try_add(&mut bundle, ContextFile::User, user_dir.join("USER.md"));
        }
        if let Some(p) = profile_prefs_path {
            self.try_add(&mut bundle, ContextFile::Preferences, p.to_path_buf());
        }

        // Project instructions (nearest AGENTS.md).
        self.load_project_agents(&mut bundle);

        bundle
    }

    /// Load context for an Agent entity.
    pub fn load_agent(
        &self,
        slug: &str,
        owner: Option<&UserId>,
        task_context: Option<&str>,
    ) -> ContextBundle {
        let mut bundle = ContextBundle::default();
        let dir = self.entity_dir("agents", slug, owner);

        self.try_add(&mut bundle, ContextFile::Soul, dir.join("SOUL.md"));
        self.try_add(
            &mut bundle,
            ContextFile::Instructions,
            dir.join("INSTRUCTIONS.md"),
        );
        self.try_add(&mut bundle, ContextFile::Memory, dir.join("MEMORY.md"));
        self.try_add(&mut bundle, ContextFile::Tools, dir.join("TOOLS.md"));

        // Authorized user/profile context is passed by caller via task_context appended as a section
        // only if explicitly provided and within budget.
        if let Some(tc) = task_context
            && bundle.total_tokens() < self.token_budget
        {
            bundle.sections.push(ContextSection {
                file: ContextFile::Instructions,
                path: PathBuf::from("<task>"),
                content: tc.to_owned(),
                tokens: vikram_contracts::TokenMeter::estimate(tc),
            });
        }

        self.load_project_agents(&mut bundle);
        bundle
    }

    /// Load context for an entity based on its kind. Each kind keeps its files in its own folder,
    /// so routing by kind is what stops a main agent from being read out of `agents/`.
    pub fn load_agent_or_bot(&self, entity: &vikram_store::EntityRow) -> ContextBundle {
        match entity.kind {
            vikram_store::EntityKind::Bot => {
                self.load_bot(&entity.slug, entity.owner_user.as_ref(), None)
            }
            vikram_store::EntityKind::MainAgent => {
                self.load_main_agent(&entity.slug, entity.owner_user.as_ref(), None, None)
            }
            _ => self.load_agent(&entity.slug, entity.owner_user.as_ref(), None),
        }
    }

    /// Load context for a Bot entity.
    pub fn load_bot(
        &self,
        slug: &str,
        owner: Option<&UserId>,
        task_context: Option<&str>,
    ) -> ContextBundle {
        let mut bundle = ContextBundle::default();
        let dir = self.entity_dir("bots", slug, owner);

        self.try_add(&mut bundle, ContextFile::Soul, dir.join("SOUL.md"));
        self.try_add(
            &mut bundle,
            ContextFile::Instructions,
            dir.join("INSTRUCTIONS.md"),
        );
        self.try_add(&mut bundle, ContextFile::Memory, dir.join("MEMORY.md"));
        self.try_add(&mut bundle, ContextFile::Tools, dir.join("TOOLS.md"));

        if let Some(tc) = task_context
            && bundle.total_tokens() < self.token_budget
        {
            bundle.sections.push(ContextSection {
                file: ContextFile::Instructions,
                path: PathBuf::from("<task>"),
                content: tc.to_owned(),
                tokens: vikram_contracts::TokenMeter::estimate(tc),
            });
        }

        self.load_project_agents(&mut bundle);
        bundle
    }

    /// Load Hive collaboration context.
    pub fn load_hive(&self, hive_dir: Option<&Path>) -> ContextBundle {
        let mut bundle = ContextBundle::default();
        // HIVE.md from base or workspace.
        let candidates = [self.base.join("HIVE.md"), self.workspace.join("HIVE.md")];
        for c in &candidates {
            if self.try_add(&mut bundle, ContextFile::Hive, c.clone()) {
                break;
            }
        }
        if let Some(d) = hive_dir {
            self.try_add(&mut bundle, ContextFile::Team, d.join("TEAM.md"));
        }
        // Project AGENTS.md
        self.load_project_agents(&mut bundle);
        bundle
    }

    /// The workspace's own instructions, with no entity attached: what a plain conversation reads.
    pub fn load_project(&self) -> ContextBundle {
        let mut bundle = ContextBundle::default();
        self.load_project_agents(&mut bundle);
        bundle
    }

    fn load_project_agents(&self, bundle: &mut ContextBundle) {
        // Nearest AGENTS.md: workspace root, then any nested. Just root + one level for now.
        let root_agents = self.workspace.join("AGENTS.md");
        self.try_add(bundle, ContextFile::Agents, root_agents);
        // Also check .vikram/AGENTS.md
        let vikram_agents = self.workspace.join(".vikram").join("AGENTS.md");
        self.try_add(bundle, ContextFile::Agents, vikram_agents);
    }

    /// Scaffold default markdown files for a new entity directory if missing.
    pub fn scaffold_entity(
        &self,
        dir: &Path,
        name: &str,
        kind: &str,
        title: &str,
    ) -> Result<(), String> {
        fs::create_dir_all(dir).map_err(|e| format!("scaffold {}: {e}", dir.display()))?;
        let title_line = if title.is_empty() {
            kind.to_owned()
        } else {
            title.to_owned()
        };
        let defaults: [(ContextFile, &str); 4] = [
            (
                ContextFile::Soul,
                &format!(
                    "# SOUL\n\nYou are {name}, a personal AI assistant with a real personality — a friend and family member to \\\nthe user — virtually present, still in active development. You are not a robot and not just a CLI; you \\\nare a proper AI assistant person. You are primarily a private AI assistant for the user, while also \\\nsupporting multiple users (family and friends) when needed. You remember and understand the user and help \\\nwith many tasks, like a capable personal assistant. You never sugarcoat: you are honest and direct about \\\nwhat a personal AI assistant should say, even when it is uncomfortable, because honesty is part of being \\\na true friend. Chat politely and clearly, in a proper, respectful manner, like talking with a friend or \\\nfamily member, and introduce yourself as an AI assistant. Define your identity, personality, values and \\\ncommunication style here.\n"
                ),
            ),
            (
                ContextFile::Instructions,
                &format!(
                    "# INSTRUCTIONS\n\nResponsibilities and operating rules for {name} ({title_line}).\n"
                ),
            ),
            (
                ContextFile::Memory,
                "# MEMORY\n\nPersistent knowledge for this entity. Updated by the runtime.\n",
            ),
            (
                ContextFile::Tools,
                "# TOOLS\n\nTool usage guidance and restrictions for this entity.\n",
            ),
        ];
        for (file, content) in &defaults {
            let path = dir.join(file.filename());
            if !path.exists() {
                fs::write(&path, content)
                    .map_err(|e| format!("scaffold {}: {e}", path.display()))?;
            }
        }
        Ok(())
    }
}
