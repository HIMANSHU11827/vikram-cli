# Module SDK

Author guide for the three package kinds `vikram module` scaffolds, validates, and packs.
Everything here is local: no network, no registry upload, no signature verification.

```
vikram module init <name> [--kind plugin|skill-pack|agent-pack] [--dir <path>]
vikram module validate <path>
vikram module pack <path> [--out <file.tar.gz>]
```

`init` kebab-cases `<name>` into the directory, plugin `id`, and skill name — the only
spelling every consumer accepts. It refuses to write into a non-empty directory.

## Package kinds

### `plugin` — process-hook plugin

A directory with `plugin.json`, an entry script, and a README. The scaffold writes a
`hook.sh` stub that replies `{}` (allow, change nothing) and a manifest declaring the
`turn_start` hook point.

Install and inspect with the existing commands:

```
vikram plugins validate <dir>
vikram plugins install <dir> --yes
vikram plugins hooks        # or: vikram modules hooks
```

### `skill-pack` — SKILL.md collection

A directory whose immediate subdirectories each hold a `SKILL.md`. Frontmatter requires
`name` (must equal the directory name: lowercase letters, digits, `-`; ≤64 bytes, no `--`,
not ending in `-`) and `description` (required, ≤1024 bytes). Extra keys under `vikram:`
carry tags, permissions, activation policy, etc. — see `crates/vikram-skills/src/metadata.rs`.

Install one skill at a time:

```
vikram skills install <pack>/<skill-name>
vikram skills validate <skill-dir>
```

`vikram module validate <pack>` checks every skill subdirectory.

### `agent-pack` — agent/bot definition

The four context files the runtime loads for an entity — `SOUL.md`, `INSTRUCTIONS.md`,
`MEMORY.md`, `TOOLS.md` (the `ContextFile` filenames in
`crates/vikram-runtime/src/context.rs`) — plus `entity.example.json`, a template for the
`entity.json` manifest `vikram entities install-pack` reads. Copy it to `entity.json` and
fill in what you need; every field is optional (kind defaults to `agent`, slug to the
directory name). Unknown fields are refused.

```
vikram entities install-pack <dir>        # or <name>-<version>.tar.gz
# entity lives at $VIKRAM_HOME/agents/<slug>/ (bots: $VIKRAM_HOME/bots/<slug>/)
```

`install-pack` registers the entity in the store and copies the four context files into
place — the same row `vikram entities new` writes, with the pack's metadata.

There is no packaged "workflow" kind: scheduled workflow jobs are store rows that name a
Hive (`vikram cron add --job-mode workflow --agent <hive>`), not files, so there is nothing
to scaffold honestly.

## `plugin.json` — manifest reference

`PluginManifest` (`crates/vikram-runtime/src/plugins/manifest.rs`). Every field defaults;
the legacy `{name, version, entry, enabled}` shape still parses.

| field | type | notes |
|---|---|---|
| `id` | string | stable id; falls back to `name`. No `/`, `\`, `.`, `..` |
| `name` | string | display name |
| `version` | string | semver `MAJOR.MINOR.PATCH` |
| `description` | string | shown in listings |
| `author`, `license` | string | |
| `homepage`, `repository` | string? | |
| `min_vikram_version` | string? | refuses install/run on older builds |
| `api_version` | string? | |
| `components` | map<string, string[]>? | declared components; known keys below |
| `dependencies` | [{name, version_req}] | resolved against local dirs only, never fetched |
| `permissions` | string[] | closed vocabulary, see below |
| `entry` | string | path inside the plugin dir; `.py`→python3, `.sh`→sh, `.js`→node |
| `enabled` | bool | absent manifests count as enabled |
| `priority` | int | hook ordering: higher first, ties by plugin id |
| `hook_rules` | HookRule[] | declarative rules evaluated in-process |
| `transforms` | TransformOp[] | file ops applied by `vikram transforms apply` |

`components` keys: `tools`, `skills`, `agents`, `hooks`, `commands`, `mcp`, `providers`,
`channels`, `services`, `ui`, `stages`, `wasm`. `components.hooks` lists the hook points a
process or wasm hook handles; `components.stages` lists the stage points a plugin handles;
`components.wasm` lists `.wasm` entries (a `.wasm` manifest `entry` is also honored).

### HookRule fields

`point` (required), `tool` (glob on tool name), `arguments_contain`, `result_contain`,
`text_contain` (substring conditions), and effects `deny`, `prepend`, `append`,
`append_result`, `replace_result`, `set_ok`, `arguments_set`, `max_output_tokens`,
`replace_text`. Conditions that cannot apply to the point are rejected at load time.

## Permissions

Closed vocabulary (`parse_permission` in
`crates/vikram-runtime/src/plugins/permission.rs`); unknown spellings are refused:

`fs.read`, `fs.write`, `network`, `process`, `env`, `secrets`, `skills`, `tools`,
`agents`, `hive`, `bots`, `ui`, `background`

- Only `process` or `network` allow the entry script to run at all.
- `env` lets a hook inherit the full environment; without it a safe subset is passed.
- A `:scope` suffix (`fs.read:workspace`) parses but is advisory — not enforced yet.
- Hooks need the permission of their surface: `tool_call`, `tool_result`,
  `memory_recall`, `memory_write` need `tools`; all other hook points need `agents`.

## Hook points (14)

`session_start`, `user_input`, `turn_start`, `context_build`, `memory_recall`,
`model_request`, `tool_call`, `memory_write`, `tool_result`, `response_delivery`,
`turn_end`, `delegation_start`, `delegation_end`, `session_end`
(`HookPoint::ALL` in `crates/vikram-runtime/src/hooks.rs`; hyphens/`.` accepted for `_`).

## Stage points

Ten ordered pipeline steps (`StagePoint` in `crates/vikram-runtime/src/pipeline.rs` —
the enum is authoritative): `context_build`, `memory_recall`, `provider_request`,
`tool_dispatch`, `result_commit`, `user_input`, `response_commit`, `turn_end`,
`session_lifecycle`, `agent_lifecycle`. A plugin declares `components.stages` and its
entry runs as a stage handler: it may mutate the payload and answer
`StageOutcome::Pass` to hand dispatch to the next registration, or resolve the stage
itself. Process entries and `.wasm` entries both work (`ProcessStageHandler`,
`WasmStageHandler`); native code can also call `RuntimeHandle::register_stage`.
Lifecycle points dispatch through `run_blocking` (no async boundary); of the lifecycle
operations only `create` may be vetoed.

## WASM entries

A `.wasm` module (declared under `components.wasm`, or as `entry` when it ends in
`.wasm`) executes in-process through wasmi — no subprocess, no WASI
(`crates/vikram-runtime/src/plugins/wasm.rs`). The guest exports
`memory`/`alloc`/`dealloc`/`handle`; the request/response is JSON over linear memory
(ABI v2). Host imports are limited to `vikram.log` (bounded diagnostics merged into
the reply as `_logs`) and `vikram.read_resource` (file reads confined to the plugin's
own directory). Each call runs under a fresh store with a fuel budget. The manifest
still needs `process` or `network` permission for the entry to execute, and wasm
hooks answer the same `HookEffect`-shaped JSON a process plugin writes to stdout.

## `validate`

Checks, per detected kind:

- **plugin**: `plugin.json` parses; `id` usable; `version` is semver; `entry` exists
  inside the directory (or hooks are declared via `components.hooks`/`hook_rules`);
  every permission parses; hook point names and rule/point consistency.
- **skill / skill-pack**: delegates to `vikram_skills::Manager::validate`.
- **agent-pack**: all four context files present.

Errors print as `error: ...`, advisories as `warning: ...`; any error exits nonzero.

## `pack`

Requires the directory to validate first, then shells out to
`tar --sort=name --owner=0 --group=0 --numeric-owner --use-compress-program='gzip -n'`,
producing a deterministic `.tar.gz` containing the module directory. The installers
accept the archive directly — each extracts it to a temp dir (removed afterwards) and
runs the same directory path, so validation is identical:

```
vikram plugins install my-plugin-0.1.0.tar.gz --yes   # plugin
vikram skills install my-pack-0.1.0.tar.gz            # skill or every skill in a pack
vikram modules install my-plugin-0.1.0.tar.gz --yes   # catalog-style install
vikram entities install-pack my-agent-0.1.0.tar.gz    # agent-pack
```

`skills install` also honors `--subdir <path>` inside the archive to pick one skill out
of a pack. Non-tar input is refused rather than guessed at.

If `tar`/`gzip` are missing, `pack` says so instead of writing a partial file.
