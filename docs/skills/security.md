# Security — threat model and mitigations

## Core principle

A skill is **instructions, not code**. The crate parses, validates, copies, and describes skills. Nothing in
`vikram-skills` executes skill content. Execution happens only when the model follows the instructions and invokes
a tool, or when the user explicitly runs a script.

## Trust levels

Six levels, ordered from most to least trusted:

| Level | Meaning |
| - | - |
| `Builtin` | Embedded in the binary. Reviewed by the VIKRAM team. |
| `ProjectTrusted` | Project skill in a trusted workspace (project_trusted = true). |
| `UserTrusted` | User-installed skill in `<VIKRAM_HOME>/skills`. |
| `Verified` | Imported skill under a trusted source prefix. |
| `Community` | Everything else (untrusted project, untrusted imported, external). |
| `Untrusted` | Explicitly denied by policy. |

## What a skill can and cannot do

**Can do:**
- Provide instructions to the model (injected as a `user`-role message).
- Declare permission requests (subject to policy).
- Reference resources and scripts (model must explicitly read/run them).

**Cannot do:**
- Override system instructions (skill body is `user` role, not `system`).
- Grant itself permissions (declared permissions are requests, not grants).
- Execute code automatically (scripts are never auto-run).
- Escape the workspace (path traversal is blocked by `resolve_checked`).
- Bypass the trust policy (deny rules are evaluated before loading).

## Threat: prompt injection via skill body

A malicious skill's body is injected as a `user`-role message. The model is instructed to follow matching skills,
so a malicious skill can inject instructions at user privilege. Mitigations:

- Per-turn activation only ("Do not carry skills across turns unless re-mentioned").
- Catalogue is a `developer`-role message; body is `user` role — the model can distinguish.
- Budget caps limit how much skill text enters the context.
- Trust policy can deny specific skills or entire sources.

## Threat: path traversal

A skill's `name` or resource path could contain `..` or absolute paths. Mitigations:

- `resolve_checked(root, rel, follow_symlinks)` rejects paths that escape the root.
- Symlinks are refused unless explicitly allowed.
- `Manager::install` derives the directory name from the validated `name` field.

## Threat: untrusted scripts

A skill's `scripts/` directory may contain executable code. Mitigations:

- Scripts are never auto-executed. The model must explicitly invoke a tool to run them.
- Script execution inherits the user's shell permissions (no sandbox).
- `allowed-tools` is a declaration, not enforcement — do not rely on it for safety.

## Operator controls

- `vikram skills disable <name>` — disable a skill (persists in `disabled.json`).
- `vikram skills delete <name>` — remove a skill (moves to trash, recoverable).
- Trust policy deny rules — block specific skills, directories, or entire compat sources.
- Project trust flag — controls whether project skills get `ProjectTrusted` or `Community`.

## What is NOT defended

- No code signing or integrity verification.
- No sandbox for script execution.
- No `allowed-tools` enforcement.
- No publisher identity or reputation system.
- A skill from an untrusted repository is code review you owe yourself.
