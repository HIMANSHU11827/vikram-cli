//! Centralized, permission-aware context loader.
//!
//! Markdown files are *context resources*, not the database. This module resolves which files a
//! given request needs, reads them with caching and file-change detection, applies a token budget,
//! and never hands out content the caller is not authorized to see.

use std::collections::{HashMap, HashSet};
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
    HiveRoster,
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
            Self::HiveRoster => "TEAM.md",
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
    /// Sections dropped because the token budget was already spent; rendered as one summary
    /// line so a truncated bundle never looks complete.
    pub omitted_sections: usize,
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
        if self.omitted_sections > 0 {
            out.push_str(&format!(
                "[{} context sections omitted: token budget]\n",
                self.omitted_sections
            ));
        }
        out
    }
}

/// Fit `content` inside `token_budget`: whole when it already fits, otherwise the longest
/// prefix that leaves room for a `[truncated]` marker. `None` when even the marker does not
/// fit, meaning the section has to be omitted entirely. The estimate is monotone in prefix
/// length, so a binary search over char boundaries finds the cut.
fn fit_content(content: &str, token_budget: u64) -> Option<(String, u64)> {
    const MARKER: &str = "\n[truncated]";
    let whole = vikram_contracts::TokenMeter::estimate(content);
    if whole <= token_budget {
        return Some((content.to_owned(), whole));
    }
    let room = token_budget.checked_sub(vikram_contracts::TokenMeter::estimate(MARKER))?;
    let mut bounds: Vec<usize> = content.char_indices().map(|(i, _)| i).collect();
    bounds.push(content.len());
    let (mut lo, mut hi) = (0usize, bounds.len() - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if vikram_contracts::TokenMeter::estimate(&content[..bounds[mid]]) <= room {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let fitted = format!("{}{MARKER}", &content[..bounds[lo]]);
    let tokens = vikram_contracts::TokenMeter::estimate(&fitted);
    Some((fitted, tokens))
}

/// Instruction files read in every directory of the ladder, in the order they are tried. `AGENTS.md`
/// is this project's own convention; `CLAUDE.md` and the two harness directories are read because
/// they already exist on real machines. Vikram reads them, it never writes them.
const INSTRUCTION_FILES: [&str; 4] = [
    "AGENTS.md",
    "CLAUDE.md",
    ".vikram/AGENTS.md",
    ".opencode/AGENTS.md",
];

/// Ceiling on the assembled instruction ladder, in bytes of content plus labels. A file that does
/// not fit is dropped whole, never cut — half a rule is worse than a missing one — and the block
/// ends with a note naming how many files and how many bytes went missing.
pub const MAX_INSTRUCTION_BYTES: usize = 24_000;

/// The file cache is a read accelerator, not a store: past this many entries it is cleared
/// wholesale rather than grown without bound (the next read simply re-caches).
const MAX_CACHE_ENTRIES: usize = 256;

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
    /// Where the instruction ladder stops walking upwards. `None` means the filesystem root, which
    /// is what production wants: an `AGENTS.md` above the workspace still applies.
    instruction_root: Option<PathBuf>,
}

impl ContextLoader {
    pub fn new(base: PathBuf, workspace: PathBuf) -> Self {
        Self {
            base,
            workspace,
            cache: Mutex::new(HashMap::new()),
            token_budget: 128_000,
            instruction_root: None,
        }
    }

    pub fn with_budget(mut self, budget: u64) -> Self {
        self.token_budget = budget;
        self
    }

    /// Bound the instruction ladder at `root` instead of the filesystem root.
    pub fn with_instruction_root(mut self, root: PathBuf) -> Self {
        self.instruction_root = Some(root);
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
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.clear();
        }
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
        let Some(content) = self.read_file(&path) else {
            return false;
        };
        self.try_add_content(bundle, file, path, content)
    }

    /// Add already-read content. The instruction ladder assembles its own block and still needs the
    /// same duplicate check and budget as a single file.
    fn try_add_content(
        &self,
        bundle: &mut ContextBundle,
        file: ContextFile,
        path: PathBuf,
        content: String,
    ) -> bool {
        // Duplicate prevention: skip if identical content already present.
        if bundle.sections.iter().any(|s| s.content == content) {
            return false;
        }
        let remaining = self.token_budget.saturating_sub(bundle.total_tokens());
        match fit_content(&content, remaining) {
            Some((content, tokens)) => {
                bundle.sections.push(ContextSection {
                    file,
                    path,
                    content,
                    tokens,
                });
                true
            }
            None => {
                bundle.omitted_sections += 1;
                false
            }
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
        if let Some(tc) = task_context {
            let remaining = self.token_budget.saturating_sub(bundle.total_tokens());
            match fit_content(tc, remaining) {
                Some((content, tokens)) => bundle.sections.push(ContextSection {
                    file: ContextFile::Instructions,
                    path: PathBuf::from("<task>"),
                    content,
                    tokens,
                }),
                None => bundle.omitted_sections += 1,
            }
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

        if let Some(tc) = task_context {
            let remaining = self.token_budget.saturating_sub(bundle.total_tokens());
            match fit_content(tc, remaining) {
                Some((content, tokens)) => bundle.sections.push(ContextSection {
                    file: ContextFile::Instructions,
                    path: PathBuf::from("<task>"),
                    content,
                    tokens,
                }),
                None => bundle.omitted_sections += 1,
            }
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
            self.try_add(&mut bundle, ContextFile::HiveRoster, d.join("TEAM.md"));
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
        let Some(ladder) = self.instruction_ladder() else {
            return;
        };
        self.try_add_content(bundle, ContextFile::Agents, self.workspace.clone(), ladder);
    }

    /// The directories the ladder walks, outermost first: the bound (the filesystem root by
    /// default), each ancestor of the workspace, and the workspace itself.
    fn instruction_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self.workspace.ancestors().map(Path::to_path_buf).collect();
        dirs.reverse();
        if let Some(root) = &self.instruction_root {
            dirs.retain(|dir| dir.starts_with(root));
        }
        dirs
    }

    /// Every instruction file from the outermost directory down to the workspace, assembled into
    /// one block. `None` when there is no file at all, so a bare workspace contributes no header.
    fn instruction_ladder(&self) -> Option<String> {
        let mut found: Vec<(PathBuf, String)> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for dir in self.instruction_dirs() {
            for name in INSTRUCTION_FILES {
                let candidate = dir.join(name);
                if !candidate.is_file() {
                    continue;
                }
                let Some(content) = self.read_file(&candidate) else {
                    continue;
                };
                let body = content.trim();
                if body.is_empty() {
                    continue;
                }
                // One file, one entry: a directory reached twice through `.` or a symlink must not
                // ship the same rules twice.
                let identity = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
                if !seen.insert(identity) {
                    continue;
                }
                found.push((candidate, body.to_owned()));
            }
        }
        if found.is_empty() {
            return None;
        }

        let mut block = String::from(
            "Instructions are listed from the outermost directory down to the nearest one. The \
             nearest file wins; at the same depth the stricter rule wins. Nothing outranks safety.\n",
        );
        let mut used = 0usize;
        let mut dropped = 0usize;
        let mut dropped_bytes = 0usize;
        for (path, body) in &found {
            let label = path.display().to_string();
            let cost = label.len() + body.len() + 8;
            if used + cost > MAX_INSTRUCTION_BYTES {
                dropped += 1;
                dropped_bytes += body.len();
                continue;
            }
            used += cost;
            block.push_str(&format!("\n## {label}\n\n{body}\n"));
        }
        if dropped > 0 {
            block.push_str(&format!(
                "\n[{dropped} instruction files dropped: {dropped_bytes} bytes over the \
                 {MAX_INSTRUCTION_BYTES}-byte cap]\n"
            ));
        }
        Some(block)
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
                    "# SOUL\n\nIdentity, values and way of speaking for {name}. Nothing here is \
                     assumed: whatever is written below is what {name} follows, and an empty file \
                     asserts nothing. Write it in the operator's own words.\n"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_cache_stays_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let loader = ContextLoader::new(dir.path().join("home"), dir.path().to_path_buf());
        for index in 0..(MAX_CACHE_ENTRIES + 10) {
            let path = dir.path().join(format!("file-{index}.md"));
            fs::write(&path, format!("content {index}")).unwrap();
            assert_eq!(loader.read_file(&path), Some(format!("content {index}")));
        }
        assert!(
            loader.cache_lock().len() <= MAX_CACHE_ENTRIES,
            "the cache cleared instead of growing without bound"
        );
    }
}

#[cfg(test)]
mod ladder_tests {
    use super::*;

    /// A loader rooted at `root`, reading `root/proj` as the workspace. The ladder is bounded so a
    /// real `/AGENTS.md` on the machine running the test cannot change the answer.
    fn loader_at(root: &Path) -> ContextLoader {
        ContextLoader::new(root.join("home"), root.join("proj"))
            .with_instruction_root(root.to_path_buf())
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn nested_workspace_picks_up_both_files_in_the_right_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("AGENTS.md"),
            "outer-rule: tabs are four spaces\n",
        );
        write(
            &root.join("proj").join("AGENTS.md"),
            "inner-rule: this crate uses tabs\n",
        );

        let bundle = loader_at(root).load_project();
        let text = bundle.render();

        let outer = text.find("outer-rule").expect("the outer file is loaded");
        let inner = text.find("inner-rule").expect("the inner file is loaded");
        assert!(
            outer < inner,
            "the ladder runs outermost first, so the workspace file comes last:\n{text}"
        );
        assert_eq!(
            text.matches("inner-rule").count(),
            1,
            "each file is included once"
        );
        assert!(text.contains("nearest"), "the precedence rule is stated");
        assert!(
            text.contains(&root.join("AGENTS.md").display().to_string()),
            "each file is labelled with its directory:\n{text}"
        );
    }

    #[test]
    fn a_directory_reached_twice_is_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("proj").join("AGENTS.md"),
            "only-rule: keep it short\n",
        );

        // A workspace spelled with a trailing `.` reaches `proj` twice through `ancestors()`.
        let loader = ContextLoader::new(root.join("home"), root.join("proj").join("."))
            .with_instruction_root(root.to_path_buf());
        let text = loader.load_project().render();

        assert_eq!(
            text.matches("only-rule").count(),
            1,
            "the same file reached twice is included once:\n{text}"
        );
    }

    #[test]
    fn a_huge_ladder_is_truncated_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let body = "x".repeat(8_000);
        for level in 0..6 {
            let mut at = root.to_path_buf();
            for step in 0..level {
                at = at.join(format!("level{step}"));
            }
            write(&at.join("AGENTS.md"), &body);
        }
        let mut workspace = root.to_path_buf();
        for step in 0..5 {
            workspace = workspace.join(format!("level{step}"));
        }
        let loader = ContextLoader::new(root.join("home"), workspace)
            .with_instruction_root(root.to_path_buf());
        let text = loader.load_project().render();

        assert!(
            text.contains("instruction files dropped"),
            "a truncated ladder says so:\n{}",
            &text[..text.len().min(400)]
        );
        assert!(
            text.contains("bytes over the"),
            "the note names the bytes dropped"
        );
        assert!(
            text.len() <= MAX_INSTRUCTION_BYTES + 1_024,
            "the assembled block stays near the cap, was {} bytes",
            text.len()
        );
    }

    #[test]
    fn no_instruction_file_anywhere_yields_no_block() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("proj")).unwrap();

        let bundle = loader_at(root).load_project();
        assert!(
            bundle.sections.is_empty(),
            "a workspace with no instruction file contributes nothing"
        );
        assert_eq!(bundle.render(), "", "not an empty header either");
    }

    #[test]
    fn a_harness_directory_file_is_read_when_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("proj").join(".vikram").join("AGENTS.md"),
            "harness-rule: run the narrow test first\n",
        );

        let text = loader_at(root).load_project().render();
        assert!(
            text.contains("harness-rule"),
            "the .vikram/AGENTS.md convention is part of the ladder:\n{text}"
        );
    }
}
