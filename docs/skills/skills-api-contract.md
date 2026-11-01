# `vikram-skills` public API contract

Status: authoritative for parallel implementation. The crate is `crates/vikram-skills`.
Anything not listed here is private to the crate. Callers (runtime, cli, tui) must not
reach past this surface.

## Crate layout

```text
crates/vikram-skills/
├── Cargo.toml            # deps: vikram-contracts, serde, serde_json, thiserror; dev: tempfile
└── src/
    ├── lib.rs            # module wiring + re-exports
    ├── error.rs          # SkillError, Result
    ├── frontmatter.rs    # YAML subset parser, round-trip preserving
    ├── metadata.rs       # SkillMetadata, VikramExtension, validation
    ├── skill.rs          # SkillId, Scope, Compat, SkillSummary, Skill, Resource
    ├── paths.rs          # home/skills/bundles dirs, safe path resolution
    ├── trust.rs          # TrustLevel, TrustPolicy, permission mapping
    ├── discovery.rs      # DiscoveryOptions, discover(), Catalog
    ├── loader.rs         # Loader, LoadedSkill, Budget, BudgetReport
    ├── activation.rs     # Activation, ActivationSource, ActivationSet, Selector
    ├── manager.rs        # Manager: create/edit/delete/enable/disable/duplicate/export/import
    ├── bundle.rs         # Bundle, BundleIndex
    ├── install.rs        # Installer, InstallSource, provenance, atomic install
    └── history.rs        # Manifest snapshots, content hashes, update/rollback
```

## error.rs

```rust
pub type Result<T> = std::result::Result<T, SkillError>;

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    NotFound { name: String, sources: usize },
    InvalidName { name: String, reason: String },
    Io { path: PathBuf, reason: String },
    Frontmatter { path: PathBuf, reason: String },
    MissingField { path: PathBuf, field: &'static str },
    PathEscape { path: PathBuf },
    Disabled { name: String },
    AlreadyExists { name: String, path: PathBuf },
    MissingDependency { owner: String, name: String },
    CircularDependency { chain: String },
    UntrustedScript { name: String, level: TrustLevel },
    TooLarge { what: String, size: usize, limit: usize },
    MissingResource { name: String, rel: String },
    UpdateConflict { name: String, path: PathBuf },
    InvalidBundle { path: PathBuf, reason: String },
    Install(String),
    Unsupported { what: String },
    Denied { reason: String },
}
```

## frontmatter.rs

```rust
pub enum Yaml { Null, Bool(bool), Int(i64), Str(String), List(Vec<Yaml>), Map(Vec<(String, Yaml)>) }

pub struct Frontmatter {
    pub entries: Vec<(String, Yaml)>,   // source order preserved
    pub body: String,                   // markdown after the closing delimiter
    pub raw: String,                    // the exact frontmatter block
}

impl Frontmatter {
    /// Parses `---\n<yaml>\n---\n<body>`. Missing frontmatter is NOT an error:
    /// returns an empty entry list with the whole text as body.
    pub fn parse(text: &str) -> Result<Frontmatter>;
    pub fn get(&self, key: &str) -> Option<&Yaml>;
    pub fn to_yaml(&self) -> String;              // canonical block re-emit
    pub fn with(&self, key: &str, value: Yaml) -> Frontmatter;
}

impl Yaml {
    pub fn as_str(&self) -> Option<&str>;
    pub fn as_bool(&self) -> Option<bool>;
    pub fn as_list(&self) -> Option<&[Yaml]>;
    pub fn as_map(&self) -> Option<&[(String, Yaml)]>;
    pub fn strings(&self) -> Vec<String>;         // scalar -> vec![s], list -> items
}
```

YAML subset that MUST parse: plain/`'`/`"` scalars with escapes, `|` and `>` block
scalars with `-`/`+` chomping, inline `[a, b]` lists, indented maps, indented `- item`
lists (scalars and `key: value` maps), `#` comments, yes/no/true/false, integers.
Duplicate keys are an error (`SkillError::Frontmatter`). Tabs in indentation are an
error. Unknown keys must survive a `parse` -> `to_yaml` round trip.

## metadata.rs

```rust
pub enum ActivationPolicy { Auto, Suggested, Manual }   // default Suggested

pub struct VikramExtension {
    pub version: Option<String>,
    pub min_vikram: Option<String>,
    pub categories: Vec<String>,
    pub tags: Vec<String>,
    pub requires: Vec<String>,      // "name" or "name@>=1.2"
    pub recommends: Vec<String>,
    pub permissions: Vec<String>,   // "fs.write:workspace", "net.http", "shell.run"
    pub activation: ActivationPolicy,
    pub author: Option<String>,
    pub source: Option<String>,
    pub tests: Vec<String>,
}

pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Vec<String>,
    pub metadata: Vec<(String, String)>,   // standard `metadata:` map, string->string
    pub vikram: VikramExtension,
    pub unknown: Vec<(String, Yaml)>,      // preserved, never dropped
}

pub const NAME_MAX: usize = 64;
pub const DESCRIPTION_MAX: usize = 1024;

impl SkillMetadata {
    /// `dir_name` is used for the name/directory match rule and as a fallback
    /// when frontmatter has no `name` (compatibility: warn, do not fail).
    pub fn from_frontmatter(fm: &Frontmatter, dir_name: &str) -> Result<(SkillMetadata, Vec<String>)>;
    pub fn validate_name(name: &str) -> Result<()>;   // lowercase alnum + '-', no leading/trailing '-', no '--', len <= NAME_MAX
    pub fn to_frontmatter(&self, body: &str) -> Frontmatter;
}
```

`from_frontmatter` returns the metadata plus non-fatal warnings (e.g. `description`
longer than 400 chars, name/directory mismatch, missing `name`). Errors only for
invalid name characters, over-long name, over-long description, empty description.

Extension resolution: read top-level `vikram:` map; also accept dotted keys inside the
standard `metadata:` map (`vikram.tags`, `vikram.version`, `tags`, `category`,
`version`, `author`). Nothing is invented — absent means empty.

## skill.rs

```rust
pub struct SkillId(String);                 // validated via SkillMetadata::validate_name
pub enum Scope { Builtin, User, Project, Imported, External }

impl Scope {
    pub fn precedence(self) -> u8;          // Builtin 50, User 40, Project 30, Imported 20, External 10
    pub fn label(self) -> &'static str;
}

pub enum Compat { Native, AgentsSkills, Claude, Codex, OpenCode, Hermes }
impl Compat { pub fn label(self) -> &'static str; }

pub enum TrustLevel { Builtin, Verified, UserTrusted, ProjectTrusted, Community, Untrusted }
impl TrustLevel {
    pub fn label(self) -> &'static str;
    pub fn allows_scripts(self) -> bool;    // Builtin | Verified | UserTrusted
    pub fn allows_activation(self) -> bool; // everything except Untrusted requires approval -> still true, approval handled by policy
}

pub struct SkillSummary {
    pub name: SkillId,
    pub description: String,
    pub dir: PathBuf,
    pub scope: Scope,
    pub compat: Compat,
    pub trust: TrustLevel,
    pub enabled: bool,
    pub valid: bool,
    pub version: Option<String>,
    pub categories: Vec<String>,
    pub tags: Vec<String>,
}

pub struct Resource { pub rel: String, pub kind: ResourceKind, pub bytes: u64 }
pub enum ResourceKind { Script, Reference, Asset, Template, AgentConfig, Other }

pub struct Skill {
    pub summary: SkillSummary,
    pub metadata: SkillMetadata,
    pub instructions: String,
    pub frontmatter: Frontmatter,
    pub resources: Vec<Resource>,
}
```

## paths.rs

```rust
pub fn home() -> PathBuf;                   // $VIKRAM_HOME else ~/.vikram
pub fn user_skills_dir() -> PathBuf;        // <home>/skills
pub fn bundles_dir() -> PathBuf;            // <home>/bundles
pub fn state_dir() -> PathBuf;              // <home>/skills/.vikram
pub fn disabled_path() -> PathBuf;          // <state_dir>/disabled.json
pub fn history_dir(name: &str) -> PathBuf;  // <state_dir>/history/<name>
pub fn provenance_path() -> PathBuf;        // <state_dir>/provenance.json
pub fn project_skill_dirs(workspace: &Path) -> Vec<(Scope, Compat, PathBuf)>;
pub fn resolve_within(root: &Path, rel: &str) -> Result<PathBuf>;   // rejects escapes, absolute, `..`
```

Project preference order: `<ws>/.vikram/skills` (Native), `<ws>/skills` (Native),
`<ws>/.agents/skills` (AgentsSkills), `<ws>/.claude/skills` (Claude),
`<ws>/.opencode/skills` (OpenCode), `<ws>/.hermes/skills` (Hermes).
User preference order: `<home>/skills` (Native), `<home>/skills/agents` (AgentsSkills),
`<home>/skills/claude`, `<home>/skills/opencode`, `<home>/skills/hermes`.
Compat dirs are only scanned when `DiscoveryOptions::include_compat` is true.

## trust.rs

```rust
pub struct TrustPolicy { pub project_trusted: bool, pub trusted_sources: Vec<String>, pub deny: Vec<String> }
impl TrustPolicy {
    pub fn load() -> TrustPolicy;                  // <state_dir>/trust.json, missing file = defaults
    pub fn save(&self) -> Result<()>;
    pub fn level_for(&self, scope: Scope, compat: Compat, dir: &Path) -> TrustLevel;
    /// Maps a declared permission string to a policy request; the caller evaluates it.
    pub fn permission_request(name: &SkillId, perm: &str) -> Result<PermissionRequest>;
}
```

`permission_request` parses `action:resource` (`fs.read:x`, `fs.write:x`, `shell.run:x`,
`net.http:host`, `delegate:x`, `admin:x`) into `PermissionRequest::new(...)` with
`Trust::External` as originating trust, so the kernel policy still decides.

## discovery.rs

```rust
pub struct DiscoveryOptions {
    pub workspace: Option<PathBuf>,
    pub include_user: bool,        // default true
    pub include_builtin: bool,     // default true; builtin dir = crate `builtin/` embedded via include_str! not fs
    pub include_compat: bool,      // default true
    pub extra: Vec<(Scope, Compat, PathBuf)>,
    pub max_depth: usize,          // default 3, 0 = unlimited within a source root
    pub follow_symlinks: bool,     // default false
    pub exclude: Vec<String>,      // directory names, e.g. ".git", "node_modules", "target"
}

pub enum DiagnosticLevel { Info, Warning, Error }
pub struct Diagnostic { pub level: DiagnosticLevel, pub path: PathBuf, pub message: String }

pub struct Catalog {
    pub entries: BTreeMap<SkillId, Vec<SkillSummary>>,  // all candidates per name, precedence order
    pub diagnostics: Vec<Diagnostic>,
    pub stats: CatalogStats,
}

pub struct CatalogStats { pub sources: usize, pub dirs_scanned: usize, pub skills: usize, pub duplicates: usize, pub invalid: usize, pub disabled: usize }

impl Catalog {
    pub fn discover(opts: &DiscoveryOptions) -> Catalog;
    pub fn resolve(&self, name: &str) -> Option<&SkillSummary>;      // precedence winner
    pub fn conflicts(&self) -> Vec<(&SkillId, &[SkillSummary])>;
    pub fn list(&self) -> Vec<&SkillSummary>;                        // resolved, sorted by name
    pub fn search(&self, query: &str) -> Vec<&SkillSummary>;         // name, description, tags, categories; case-insensitive; all terms must match
    pub fn disabled(&self) -> BTreeSet<SkillId>;
    pub fn refresh(&mut self, opts: &DiscoveryOptions);
}
```

Discovery rules: a skill directory is any directory containing `SKILL.md`
(case-insensitive fallback `skill.md`) within `max_depth` of a source root; the
directory name is the identity. Symlinks are skipped unless `follow_symlinks`;
symlinked roots are refused when they resolve outside the source root. Invalid skills
are recorded with `valid: false` and a diagnostic, never silently dropped. Disabled
names come from `<state_dir>/disabled.json` and are marked `enabled: false`.

## loader.rs

```rust
pub struct Budget {
    pub catalog_bytes: usize,        // default 16_000
    pub instruction_bytes: usize,    // default 24_000
    pub total_skill_bytes: usize,    // default 48_000
    pub resource_bytes: usize,       // default 12_000
    pub max_resources: usize,        // default 32
}

pub struct LoadedSkill {
    pub skill: Skill,
    pub instructions: String,        // possibly truncated
    pub truncated: bool,
    pub instructions_bytes: usize,
    pub omitted: Vec<String>,        // resources not listed because of the budget
}

pub struct Loader { pub budget: Budget }
impl Loader {
    pub fn new(budget: Budget) -> Loader;
    /// Full activation (Stage B). Reads SKILL.md, parses metadata, enumerates resources.
    pub fn load(&self, summary: &SkillSummary) -> Result<LoadedSkill>;
    /// Stage C. Resolves a reference inside the skill dir, refusing escapes.
    pub fn read_resource(&self, skill: &Skill, rel: &str) -> Result<String>;
    pub fn read_script(&self, skill: &Skill, rel: &str) -> Result<String>;
}
```

Truncation keeps whole lines and appends `\n… (truncated at N bytes; read
references/<file> for the rest)`.

## activation.rs

```rust
pub enum ActivationSource { Explicit, Automatic, Bundle(String), Role(String), Pinned }
pub struct Activation {
    pub name: SkillId,
    pub source: ActivationSource,
    pub reason: String,
    pub at_ms: u64,
    pub agent: Option<String>,
    pub scope: Scope,
    pub loaded_resources: Vec<String>,
}

pub struct ActivationSet { pub active: Vec<Activation> }
impl ActivationSet {
    pub fn activate(&mut self, act: Activation) -> bool;      // false when already active
    pub fn deactivate(&mut self, name: &SkillId) -> bool;
    pub fn contains(&self, name: &SkillId) -> bool;
    pub fn names(&self) -> Vec<&SkillId>;
}

pub struct SelectionInput<'a> {
    pub task: &'a str,
    pub project: Option<&'a str>,
    pub agent_role: Option<&'a str>,
    pub pinned: Vec<SkillId>,
    pub exclude: Vec<SkillId>,
    pub limit: usize,               // default 3
}

pub struct Selection { pub name: SkillId, pub score: f32, pub reason: String }

pub struct Selector { pub min_score: f32 }   // default 1.0
impl Selector {
    pub fn select(&self, catalog: &Catalog, input: &SelectionInput<'_>) -> Vec<Selection>;
}
```

Scoring is deterministic and explainable: exact name mention = 10, name token overlap
= 3 per token, description token overlap = 1 per token, tag/category match = 2, pinned
= +100, `ActivationPolicy::Manual` = excluded from automatic selection, untrusted =
excluded. Reason string names the matched terms. No LLM calls, no embeddings.

## manager.rs

```rust
pub struct Manager { pub root: PathBuf }        // root = user skills dir by default
impl Manager {
    pub fn new() -> Manager;
    pub fn at(root: PathBuf) -> Manager;
    pub fn create(&self, name: &str, description: &str, body: &str) -> Result<PathBuf>;
    pub fn create_from(&self, name: &str, source_dir: &Path) -> Result<PathBuf>;
    pub fn edit(&self, name: &str, body: &str) -> Result<()>;
    pub fn set_metadata(&self, name: &str, key: &str, value: Yaml) -> Result<()>;
    pub fn delete(&self, name: &str) -> Result<()>;
    pub fn enable(&self, name: &str) -> Result<()>;
    pub fn disable(&self, name: &str) -> Result<()>;
    pub fn duplicate(&self, name: &str, as_name: &str) -> Result<PathBuf>;
    pub fn export(&self, name: &str, dest: &Path) -> Result<PathBuf>;
    pub fn import(&self, src: &Path) -> Result<SkillId>;
    pub fn validate(&self, dir: &Path) -> Result<Vec<Diagnostic>>;
}
```

Every mutation is atomic: write to a sibling temp dir/file, then rename; on failure the
previous state remains. `delete` moves to `<state_dir>/trash/<name>-<ts>` rather than
`rm -rf`. Existing skills are never overwritten without `AlreadyExists`.

## bundle.rs

```rust
pub struct Bundle { pub name: String, pub description: String, pub skills: Vec<SkillId>, pub optional: Vec<SkillId>, pub path: PathBuf }
pub struct BundleIndex { pub bundles: BTreeMap<String, Bundle>, pub diagnostics: Vec<Diagnostic> }
impl BundleIndex {
    pub fn load() -> BundleIndex;                       // <home>/bundles/*.{yaml,json}
    pub fn resolve(&self, name: &str) -> Result<Vec<SkillId>>;   // includes required members, reports missing
}
```

Bundle files never copy member skills. `skills:` is required and non-empty.

## install.rs

```rust
pub enum InstallSource {
    Directory(PathBuf),
    Git { url: String, rev: Option<String>, subdir: Option<String> },
}
pub struct InstallOutcome { pub name: SkillId, pub path: PathBuf, pub source: String, pub rev: Option<String>, pub content_hash: String, pub replaced: bool }

pub struct Installer { pub manager: Manager, pub allow_network: bool }
impl Installer {
    pub fn new(manager: Manager) -> Installer;
    pub fn install(&self, source: &InstallSource) -> Result<InstallOutcome>;
}
```

Git installs shell out to `git clone --depth 1` into a staging dir under
`<state_dir>/staging`, never into `root` directly, then validate and rename. Failures
leave `root` untouched. Network is refused unless `allow_network` (callers set it from
config/explicit user consent). Provenance is recorded via `history.rs`.

## history.rs

```rust
pub struct Manifest { pub name: SkillId, pub version: Option<String>, pub content_hash: String, pub source: Option<String>, pub rev: Option<String>, pub installed_ms: u64, pub files: Vec<(String, String)> }
pub struct History { pub name: SkillId, entries: Vec<Manifest> }

pub fn hash_dir(dir: &Path) -> Result<String>;          // FNV-1a over sorted (rel, bytes), hex
impl History {
    pub fn load(name: &SkillId) -> Result<History>;
    pub fn record(&self, manifest: Manifest) -> Result<()>;    // appends, keeps newest 20
    pub fn latest(&self) -> Option<&Manifest>;
    pub fn detect_local_changes(&self, dir: &Path) -> Result<bool>;
    pub fn rollback(&self, dir: &Path) -> Result<Manifest>;    // restores files from the previous manifest snapshot
}
```

## Non-negotiables for implementation

1. No `unwrap`/`expect`/`panic!` outside `#[cfg(test)]`; every IO error becomes a
   typed `SkillError` with the path.
2. No new third-party dependencies beyond serde, serde_json, thiserror (dev: tempfile).
3. Every module ships `#[cfg(test)] mod tests` with `tempfile`-backed cases.
4. Tests must cover: missing frontmatter, malformed YAML, duplicate keys, over-long
   name/description, name/dir mismatch, unknown-key round trip, blake-free hashing
   determinism, duplicate precedence across scopes, disabled filtering, path-escape
   rejection, symlink refusal, budget truncation, selection reasons, atomic create
   collision, delete-to-trash, disable/enable persistence, bundle resolve with a
   missing member, git-install refusal when network is disabled.
5. `cargo test -p vikram-skills` and `cargo clippy -p vikram-skills -- -D warnings`
   must pass before the work is reported done.

## Resolutions

Decisions taken after reviewing the first implementation pass against this contract. Each one
closes an ambiguity that the sections above left open. Where a decision contradicts the text
above, **this section wins** and the earlier text is the part that is stale.

### R1 — `Frontmatter.raw` means the inner block

`raw` is the exact bytes **between** the opening and closing `---` delimiters, with the
delimiter lines and the newline that precedes the closing fence excluded. It is not the
source block including fences.

`Frontmatter::with` rebuilds the struct through `to_yaml()`, so after any mutation `raw` holds
the canonical re-emit, not the source bytes. Callers must not treat `raw` as a byte-faithful
copy for hashing or diffing after an edit; hash `frontmatter.entries` plus `body` (via
`to_yaml()`) instead, and say which of the two a hash covers wherever the value is stored.

### R2 — Extra public items are now part of the contract

These exist in the first pass and are useful. They are promoted to contract rather than
deleted:

```rust
impl Yaml {
    pub fn as_i64(&self) -> Option<i64>;
    pub fn is_null(&self) -> bool;
}
impl SkillError {
    pub fn is_content_error(&self) -> bool;   // true for frontmatter/content errors, false for IO
}
pub fn user_skill_dirs() -> Vec<(Scope, Compat, PathBuf)>;   // mirrors project_skill_dirs
```

`user_skill_dirs()` returns the user preference order from `paths.rs` — `<home>/skills` (Native),
then `agents`, `claude`, `opencode`, `hermes` subdirectories — each mapped to `Scope::User`
with its `Compat`. It is the single source of truth for user roots: `discovery.rs` and
`trust.rs` must both derive user roots from it, and no module may hardcode a user subdirectory
name.

Anything not in this contract and not in R2 stays private.

### R3 — `TrustPolicy::level_for` is fully enumerated

Trust is a property of the **root**, not of the file format. `compat` selects the parser and
never changes the level. The algorithm is deterministic and total:

```text
level_for(scope, compat, dir):
  1. scope == Builtin                                  -> TrustLevel::Builtin
  2. deny matches the source id, the dir, or the name   -> TrustLevel::Untrusted
  3. scope == User                                      -> TrustLevel::UserTrusted
  4. scope == Project, project_trusted                  -> TrustLevel::ProjectTrusted
  5. scope == Project, !project_trusted                 -> TrustLevel::Community
  6. scope == Imported, dir is under a trusted_sources  -> TrustLevel::Verified
     entry (canonicalized prefix match)
  7. scope == Imported                                  -> TrustLevel::Community
  8. scope == External                                  -> TrustLevel::Community
```

`deny` entries are matched in this order: an exact source id (`<scope>:<compat>`, lowercased,
e.g. `project:claude`), then a canonicalized absolute directory path, then a bare skill name.
First match wins. A deny entry that matches nothing is not an error — policy files go stale.

`compat` is deliberately excluded so that a `.claude/skills` directory and a `.agents/skills`
directory in the same workspace get the same level. Format is not a trust signal; location is.

`TrustLevel::Verified` is reachable **only** through step 6. Nothing else may produce it.

### R4 — Name/directory mismatch stays a warning, and install prevents it

The open standard makes `name == parent directory` mandatory. VIKRAM records a
`DiagnosticLevel::Warning` and loads the skill, because:

- the directory name is the identity (`discovery.rs`), so a mismatch cannot cause a wrong-skill
  load — the summary carries both, and `SkillSummary::name` is the parsed field,
- refusing the skill would hide a working skill over a cosmetic defect, and the standard has no
  enforcement story of its own.

This is a deliberate deviation from the standard, recorded here so it is not "fixed" later by
accident. Enforcement moves to the write path: `Manager::install` writes the directory name from
the validated `name` field, so VIKRAM can never **create** a mismatch. Only hand-authored or
third-party directories can be mismatched, and those get the warning.

### R5 — `Scope::Imported` gets a real root

`Scope::Imported` had a `precedence()` of 20 and no directory that produced it. It now has one:

```text
<home>/skills/imported   (Native, Scope::Imported)
```

`Manager::import` installs there, and `user_skill_dirs()` lists it last among user roots. It stays
below `Project` in precedence so an imported third-party skill never shadows a repository skill,
and above `External` so it still resolves inside the workspace-less case.

The root exists so an imported skill is honestly labelled rather than masquerading as
user-authored. `provenance.json` records *where it came from*; the root records *what it is*.
Neither replaces the other, and `import` must write both.

### R6 — On-disk key spelling

Canonical spellings, as emitted by `to_yaml()`:

| Key | Canonical on disk | Accepted aliases |
| - | - | - |
| `allowed-tools` | `allowed-tools` (hyphen) | `allowed_tools` |
| `min_vikram` | `min_vikram` (underscore) | `min-vikram` |

Rationale: `allowed-tools` is the open standard's spelling and must round-trip literally for
interoperability with skills written for other agents. `min_vikram` is VIKRAM's own key with no
external spelling to match, so it uses the crate's own convention. Accepting both for both costs
one alias lookup at parse time and avoids rejecting a file over a hyphen.

Alias resolution happens before unknown-key detection, so an alias must never also be reported as
an unknown key.

### R7 — `Yaml::strings` contract

`strings()` is total and never panics:

| Input | Result |
| - | - |
| `Null` | `vec![]` |
| `Bool(b)` | `vec!["true"]` / `vec!["false"]` |
| `Int(i)` | `vec![i.to_string()]` |
| `Str(s)` | `vec![s.clone()]` |
| `List(items)` | `items.iter().flat_map(strings).collect()` |
| `Map(pairs)` | `vec!["key:value", ...]` in source order |

The `Map` arm exists because `metadata:` is a string→string map whose values are sometimes read
as a list; flattening to `key:value` keeps that read lossless instead of dropping it.

### R8 — Parsing edges, resolved

| Case | Behaviour |
| - | - |
| Non-scalar value under `metadata:` (list, map) | Stringified via `strings()` and joined with `, `. One `Warning` diagnostic naming the key. |
| Unknown key under `vikram:` | `Warning` diagnostic; the entry is preserved in `frontmatter.entries` so it survives `to_yaml()`. Never an error. |
| Both `vikram.tags` and `metadata.tags` | Union, deduplicated, `vikram.tags` order first. One `Info` diagnostic. Neither source is dropped. |
| `metadata:` present but empty | Treated as absent. No diagnostic. |
| `vikram.min_vikram` newer than the crate version | `Warning`. Loading is not blocked: the field is advisory, and a hard failure would brick a skill on a patch bump. |

### R9 — What must change in the code to match these resolutions

1. `frontmatter.rs`: `raw` semantics per R1; hyphen/underscore alias table per R6; edge cases per
   R8.
2. `paths.rs`: add the `<home>/skills/imported` root per R5; expose `user_skill_dirs()` per R2.
3. `trust.rs`: implement `level_for` exactly as R3, with a unit test per step.
4. `discovery.rs`: derive user roots from `user_skill_dirs()`; scan the imported root.
5. `manager.rs`: `import` targets the imported root and writes provenance per R5.
6. Anywhere a hash covers frontmatter: hash the canonical emit, not `raw`, per R1.
