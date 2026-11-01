# Frontmatter Fields Deep-Dive

Detailed reference for each `SKILL.md` frontmatter field, beyond the summary in `frontmatter-reference.md`.

## `name`

- **Type:** string
- **Required:** yes
- **Constraint:** 1–64 chars, `^[a-z0-9-]+$`, no leading/trailing hyphen, no consecutive hyphens, must match parent directory name.
- **On disk:** used as the skill's identity. `SkillId` wraps this string.
- **Validation:** `SkillMetadata::validate_name()` returns `Err(SkillError::InvalidName)` on failure.
- **Mismatch:** name != directory is a `DiagnosticLevel::Warning`, not an error (R4). The skill still loads.

## `description`

- **Type:** string
- **Required:** yes
- **Constraint:** 1–1024 chars, non-empty.
- **On disk:** the routing surface. The model uses this to decide whether to activate the skill.
- **Best practice:** state capability AND trigger keywords. "Extracts text from PDFs. Use when working with PDF documents."
- **Budget:** truncated to 1,024 chars in the catalog listing (`MAX_CATALOG_SKILL_DESCRIPTION_CHARS`).

## `license`

- **Type:** string
- **Required:** no
- **Constraint:** none specified by VIKRAM (open standard: license name or reference to bundled file).
- **On disk:** stored in `SkillMetadata.license`, surfaced in `vikram skills show`.

## `compatibility`

- **Type:** string
- **Required:** no
- **Constraint:** 1–500 chars (open standard). VIKRAM does not enforce a length cap.
- **On disk:** stored in `SkillMetadata.compatibility`. Used in trust mapping? No — compat is deliberately excluded from `level_for` (R3). Format is not a trust signal.
- **Codex note:** Codex's validator rejects `compatibility` as an unknown key. VIKRAM accepts it.

## `metadata`

- **Type:** map (string → string, or string → any scalar)
- **Required:** no
- **Constraint:** none on keys. Values are stringified via `Yaml::strings()` (R7).
- **On disk:** stored in `SkillMetadata.metadata: BTreeMap<String, Yaml>`.
- **Common keys:** `author`, `version`, `tags`, `categories`, `activation` (see `metadata.md` for the `vikram:` extension).

## `allowed-tools`

- **Type:** string (space-separated) or array of strings
- **Required:** no
- **On disk:** canonical emit is `allowed-tools` (hyphen, matches open standard). Accepts `allowed_tools` as alias (R6).
- **Enforcement:** VIKRAM parses but does not enforce `allowed-tools`. It is a declaration, not a permission grant (non-negotiable #6).

## `vikram:` extension

See `metadata.md` for the full `vikram:` extension reference (activation, tags, categories, min_vikram, allowed-tools).
