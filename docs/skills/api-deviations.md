# `vikram-skills` — deviations from the API contract

Append-only log. Each entry names what the code does differently from
`docs/skills/skills-api-contract.md` and why. Entries are never edited to look right;
a wrong entry gets a correction below it.

Verified against the implementation with `cargo test -p vikram-skills` (190 passed) and
`cargo clippy -p vikram-skills --all-targets -- -D warnings` (clean).

---

## D1 — `DiscoveryOptions` gains two fields

```rust
pub state_dir: Option<PathBuf>,     // default None -> paths::state_dir()
pub trust: Option<TrustPolicy>,     // default None -> TrustPolicy::load()
```

**Why.** `discover()` reads disabled names from `<state_dir>/disabled.json` and the trust
policy from `<state_dir>/trust.json`. With no override, every test that asserts disabled
filtering or a trust downgrade would write to (and read from) the real `~/.vikram`, which
breaks the isolation rule for the test suite and makes results depend on the developer's
machine.

**Effect.** `None` reproduces the contract behaviour byte for byte, so existing callers are
unaffected — but a caller building the struct with a literal must now write
`..Default::default()`. Convenience builders `DiscoveryOptions::roots_only()`,
`with_root()`, `with_state_dir()`, `with_trust()` exist for the same reason.

---

## D2 — `Catalog` carries two private fields

```rust
pub(crate) policies: BTreeMap<SkillId, ActivationPolicy>,
pub(crate) disabled_names: BTreeSet<SkillId>,
```

**Why.** `SkillSummary` is fixed by the contract and cannot carry the activation policy, but
`Selector::select` must exclude `ActivationPolicy::Manual` skills automatically and must
know which names are disabled. Both are discovery facts, so they live on the catalog.

**Effect.** No public surface change (`Catalog` still derives `Default`, `Clone`, `Debug`; the
contract's four fields are public and unchanged). A caller cannot build the catalog by struct
literal — it could not meaningfully before either, since `entries` is produced by discovery.

---

## D3 — `History` owns its state directory, and `record` merges into the log on disk

```rust
pub struct History {
    pub name: SkillId,
    entries: Vec<Manifest>,
    state_dir: PathBuf,   // private
}
```

- `History::load(name)` keeps using the real `<state_dir>` (`paths::state_dir()`).
- `History::load_in(state_dir, name)` reads from an explicit directory, and the returned value
  keeps it, so `record`, `snapshot` and `rollback` write there.
- `record(&self, manifest)` re-reads the log from disk, appends, caps at the newest 20, then
  writes it back. It does **not** append to `self.entries`.

**Why.** `History::load` hardcoding `paths::state_dir()` would make `Manager::at(tmp)` record
into the real home directory — the exact isolation failure D1 exists to prevent. Merging on
disk instead of appending in memory means two writers (an install and a rollback) cannot
overwrite each other's entries with a stale vector.

**Effect on callers.** The receiver of `record` is now logically behind the disk: `latest()`
on that instance does not include the manifest just recorded. Callers must reload
(`History::load` / `load_in`) to observe their own write. This is stated on the method.

---

## D4 — `export` produces a directory archive, not a zip

**Why.** Non-negotiable 2 forbids new dependencies and a zip writer is one. A directory is
also inspectable and diffable.

**Shape.**

- `dest` is the skill directory itself, unless `dest` already exists as a directory, in which
  case the export lands at `dest/<name>`.
- The skill's files are copied with `default_excludes` applied, then `EXPORT.json` is written
  next to them: `{ name, version, content_hash, exported_ms, source }`.
- `content_hash` is `history::hash_dir` — the same FNV-1a value `install` and
  `detect_local_changes` use, so an export can be compared against a live directory.
- `import` accepts any directory, skips `EXPORT.json`, re-parses `SKILL.md` and re-derives the
  provenance hash from the copied bytes. The marker is metadata for humans, never trusted
  input.
- An existing target is refused with `AlreadyExists`; the write itself is staged and renamed
  like every other mutation.

---

## D5 — Additive helper surface (no contract signature changed)

These exist in addition to the contract. Every one is a superset: the contract's own
signatures still compile against this crate.

| Item | Purpose |
| --- | --- |
| `SkillId::new(&str)` | validate-and-construct without going through metadata |
| `Catalog::{len, is_empty, diagnostics_at}` | size and level-filtered diagnostics |
| `Diagnostic::new`, `DiagnosticLevel::label` | construct/render a diagnostic |
| `TrustPolicy::{load_in, save_in, permission_requests}` | `state_dir`-scoped IO; plural permission mapping |
| `BundleIndex::{load_in, resolve_in}` | read bundles from an explicit dir and resolve against a supplied catalog (`MissingDependency`, `Disabled`) |
| `History::{load_in, entries, snapshot, previous}` | explicit state dir, full manifest list, the manifest `rollback` would restore |
| `Loader::{report, catalog_listing}` | `BudgetReport` for a loaded skill; whole-line catalog rendering with `… (catalog truncated at N bytes)` |
| `Activation::{new, for_agent, in_scope, with_resources}`, `ActivationSet::{get, len, is_empty, clear}`, `SelectionInput::new`, `ActivationSource::label` | builder/inspection surface |
| `DiscoveryOptions::{roots_only, with_root, with_state_dir, with_trust}` | see D1 |
| `Manager` crate-private `state_dir()` / `disabled_path()` | see D9 |

`frontmatter::parse_at`, `render_document` and `parse_document` are `pub(crate)`, so they are
not part of the public surface.

---

## D6 — Selector scoring: exact-name match is exclusive; `project` and `agent_role` score

**Exact name is 10 total, not 10 + 3 per token.** The contract lists exact name = 10 and name
token overlap = 3 per token. Read literally, a one-token name like `deploy` scores 13 for a
task containing the word "deploy" and outranks a skill whose two-token name matched exactly.
The implementation awards 10 and skips name-token scoring when the name matched exactly.

**`SelectionInput::project` and `agent_role` contribute +2 each.** The contract defines the
fields but its scoring table never says what they are worth. A skill whose `dir` is inside
`project`, and a skill tagged with `agent_role`, each score +2, and the reason string names the
signal (`project`, `role:<name>`). Without this the two fields would be inert.

Everything else follows the contract exactly: description token 1, tag/category 2, pinned
+100 (pinned also bypasses `Manual` exclusion), `Manual` excluded from automatic selection,
`TrustLevel::Untrusted` excluded, invalid and disabled names skipped.

---

## D7 — Git install pins a ref with `--branch`, not an arbitrary SHA

```
git clone --depth 1 --branch <rev> <url> <staging>
```

with `GIT_TERMINAL_PROMPT=0` in the environment.

**Why.** `--depth 1` plus an explicit rev is the only combination that both avoids fetching
full history and pins the checkout to a known commit. `git clone --branch` accepts branches
and tags only, so a raw commit SHA fails with git's own message, surfaced as
`SkillError::Install { reason }`. Cloning fully and then checking out a SHA would work but
turns a bounded install into an unbounded network fetch, which the contract's staging rules
do not sanction. `GIT_TERMINAL_PROMPT=0` makes a private URL fail immediately instead of
hanging on a credential prompt with no terminal attached.

Network is still refused before any command runs unless `Installer::allow_network` is true.

---

## D8 — `enable`/`disable` do not require the skill to exist

`disabled.json` is a preference list, not a mirror of the catalog. Disabling a name that is
currently absent succeeds and prevents it from being discovered later; enabling an absent
name is a no-op at discovery time. `delete` and `edit` do require a source
(`NotFound { sources: 1 }`), because they operate on files.

---

## D9 — `Manager` state lives at `<root>/.vikram`

`Manager::state_dir()` is `self.root.join(".vikram")`, not `paths::state_dir()`. Trash,
`disabled.json`, history snapshots and provenance therefore travel with the root being
managed, so `Manager::at(tmp)` is fully self-contained.

For the real default, `Manager::new()` uses root = `user_skills_dir()` = `<home>/skills`, and
`paths::state_dir()` is `<home>/skills/.vikram` — identical path, so production behaviour is
unchanged.

---

## D10 — Builtin skills have a virtual directory

Builtins are compiled in with `include_str!` and exposed under the prefix `<builtin>/<name>`;
nothing is materialised on disk.

`SkillSummary::dir` for a builtin is therefore a path that does not exist. Callers must load
builtin instructions through `Loader::load` (which resolves them via `builtin::find`) and must
never `fs::read` a builtin `dir`. This follows the contract's "embedded via `include_str!` not
fs" rule; it is recorded here because the non-existent path is the surprising consequence.

---

## D11 — Stage B budget accounting counts `bytes + rel.len()`

`Loader` charges each listed resource `bytes + rel.len()` (`listing_cost`), because a listing
line is the relative path *and* the resource's size — charging the size alone lets a catalog of
many tiny files overshoot `total_skill_bytes`.

---

## D12 — `SkillError` derives more than the contract shows

`#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]`. `Clone` and the equality traits are
additive and let callers assert on errors in tests without string matching; `Debug` plus the
`Display` from `thiserror` are unchanged from the contract.

---

## Enforcement note (not a deviation)

`lib.rs` sets `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used,
clippy::panic, clippy::todo, clippy::unimplemented))]`, and `Cargo.toml` sets
`unsafe_code = "forbid"`. Non-negotiable 1 is therefore enforced by the compiler rather than by
review.
