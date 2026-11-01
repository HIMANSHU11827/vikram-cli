# Agent Skills — The Open Standard

Status: research note. Labels: **DOCUMENTED** = stated by the spec/primary docs; **VERIFIED-IN-SOURCE** = read in the local `codex/` clone at a cited path; **INFERRED** = reasoned from source, not stated; **UNKNOWN** = no evidence found.

Primary source: <https://agentskills.io/specification.md> (fetched). Corroboration for how one real implementation
treats the spec: the local Codex clone at `codex/codex-rs/`.

## 1. What a skill is

A skill is a **directory** containing at minimum one `SKILL.md` file: YAML frontmatter plus Markdown body. There is no
manifest file, no registry entry, no compiled artifact. The unit of distribution is a folder.

**DOCUMENTED** — the spec's directory tree:

```
skill-name/
├── SKILL.md          # Required: metadata + instructions
├── scripts/          # Optional: executable code
├── references/       # Optional: documentation
└── assets/           # Optional: templates, resources
```

## 2. `SKILL.md` anatomy

Two parts, in order:

1. **YAML frontmatter**, delimited by `---` on its own line, opening the file. Codex's parser refuses a file whose
   first bytes are not that fence — `SkillParseError::MissingFrontmatter`
   (**VERIFIED-IN-SOURCE** `codex/codex-rs/skills/src/parser.rs`).
2. **Markdown body**. "There are no format restrictions" (**DOCUMENTED**). The spec recommends step-by-step
   instructions, input/output examples, and common edge cases.

## 3. Frontmatter fields

| Field | Required | Constraints | Label |
| - | - | - | - |
| `name` | Yes | Max 64 chars. Lowercase letters, numbers, hyphens only. No leading/trailing hyphen. | DOCUMENTED |
| `description` | Yes | Max 1024 chars, non-empty. What it does **and** when to use it. | DOCUMENTED |
| `license` | No | License name or reference to a bundled license file. | DOCUMENTED |
| `compatibility` | No | Max 500 chars. Environment requirements. | DOCUMENTED |
| `metadata` | No | Arbitrary string→string map. | DOCUMENTED |
| `allowed-tools` | No | Space-separated string of pre-approved tools. **Experimental.** | DOCUMENTED |

Minimal valid skill (**DOCUMENTED**, verbatim from spec):

```yaml
---
name: skill-name
description: A description of what this skill does and when to use it.
---
```

## 4. `name` — the strictest field

Four rules, all normative (**DOCUMENTED**):

- Must be 1–64 characters.
- May only contain lowercase alphanumeric characters (`a-z`, `0-9`) and hyphens (`-`).
- Must not start or end with a hyphen.
- Must not contain consecutive hyphens (`--`).
- Must **match the parent directory name**.

Spec's own invalid examples: `PDF-Processing` (uppercase), `-pdf` (leading hyphen), `pdf--processing` (double hyphen).

Two details worth flagging:

- The spec says "unicode lowercase alphanumeric characters" but then parenthesises only `a-z, 0-9`. Codex resolves this
  the restrictive way: `^[a-z0-9-]+$` (**VERIFIED-IN-SOURCE**
  `codex/codex-rs/skills/src/assets/samples/skill-creator/scripts/quick_validate.py:61`), and the Rust loader caps a
  bare name at `MAX_NAME_LEN = 64` with a qualified name cap of `MAX_NAME_LEN * 2 + 1` = 129
  (**VERIFIED-IN-SOURCE** `codex/codex-rs/ext/skills/src/loader/mod.rs:22-23`).
- The directory-name match rule is **not enforced by Codex's validator** — it validates `name` in isolation
  (**INFERRED** from reading `quick_validate.py`; no parent-directory comparison appears in it). Treat the rule as
  spec-mandatory but implementation-optional.

## 5. `description` — the retrieval surface

- 1–1024 characters (**DOCUMENTED**).
- "Should describe both what the skill does and when to use it" and "include specific keywords that help agents
  identify relevant tasks" (**DOCUMENTED**).

The spec's contrast is the useful part: `"Extracts text and tables from PDF files, fills PDF forms, and merges
multiple PDFs. Use when working with PDF documents…"` is good; `"Helps with PDFs."` is poor. The description is not
documentation, it is the string a model routes on.

Codex mirrors the 1024 cap twice — `MAX_DESCRIPTION_LEN: usize = 1024` for `interface` fields
(**VERIFIED-IN-SOURCE** `codex/codex-rs/skills/src/interface.rs:11`) and
`MAX_CATALOG_SKILL_DESCRIPTION_CHARS: usize = 1_024` for the catalogue listing
(**VERIFIED-IN-SOURCE** `codex/codex-rs/ext/skills/src/render.rs:27`). Codex additionally rejects `<` or `>` in a
description (**VERIFIED-IN-SOURCE** `quick_validate.py`), which the spec does not require.

## 6. `license`, `compatibility`, `metadata`

**`license`** (**DOCUMENTED**): "either the name of a license or the name of a bundled license file". Short is
recommended. Example: `license: Proprietary. LICENSE.txt has complete terms`.

**`compatibility`** (**DOCUMENTED**): 1–500 chars, only when the skill has real environment requirements — intended
product, system packages, network access. Examples: `Designed for Claude Code (or similar products)`,
`Requires Python 3.14+ and uv`. The spec adds a note: "Most skills do not need the `compatibility` field."

**Contract divergence, live in this repo:** Codex's validator allow-list is
`{"name", "description", "license", "allowed-tools", "metadata"}` — `compatibility` is **absent**, so a
spec-valid skill using it fails Codex validation with "unexpected keys"
(**VERIFIED-IN-SOURCE** `quick_validate.py:40-44`). This is a real spec-vs-implementation gap, not a typo: the field
is documented on agentskills.io and unimplemented in Codex's shipped checker.

**`metadata`** (**DOCUMENTED**): a map from string keys to string values, for properties the spec does not define.
"Make your key names reasonably unique to avoid accidental conflicts." Example: `author: example-org`,
`version: "1.0"`.

## 7. `allowed-tools`

**DOCUMENTED**: "A space-separated string of tools that are pre-approved to run. Experimental. Support for this field
may vary between agent implementations." Example: `allowed-tools: Bash(git:*) Bash(jq:*) Read`.

The honest reading: this is a **declaration, not an enforcement primitive**. The spec explicitly disclaims uniform
support. In the Codex clone the field appears in the validator allow-list but no enforcement path was found
(**INFERRED** — grep of the skills crates found no consumer beyond validation). Do not describe a skill's
`allowed-tools` as a sandbox guarantee.

## 8. Optional directories

**DOCUMENTED** conventions, not requirements — the spec says "a skill directory may contain any files and
directories beyond the required `SKILL.md`", and the three names below are "recommendations":

- **`scripts/`** — executable code. Should be self-contained or document dependencies, carry helpful error messages,
  handle edge cases. "Supported languages depend on the agent implementation."
- **`references/`** — documentation read on demand. Named examples: `REFERENCE.md`, `FORMS.md`, domain files like
  `finance.md`. "Agents load these on demand, so smaller files mean less use of context."
- **`assets/`** — static resources: templates, images, data files, schemas.

Codex narrows `assets/` from convention to rule for one purpose: `interface.icon_small` / `icon_large` must be a
relative path under `assets/`, absolute paths are ignored, and a `..` path must lexically resolve inside the plugin's
`assets/` directory (**VERIFIED-IN-SOURCE** `codex/codex-rs/skills/src/interface.rs:88-144`).

## 9. Progressive disclosure

The mechanism that makes hundreds of skills affordable. Three stages (**DOCUMENTED**, quoted):

1. **Metadata** (~100 tokens) — "The `name` and `description` fields are loaded at startup for all skills".
2. **Instructions** (< 5000 tokens recommended) — "The full `SKILL.md` body is loaded when the skill is activated".
3. **Resources** (as needed) — "Files (e.g. those in `scripts/`, `references/`, or `assets/`) are loaded only when
   required".

Plus a size instruction with a number attached: "Keep your main `SKILL.md` under 500 lines. Move detailed reference
material to separate files."

Consequence for authors: frontmatter is paid for on every turn, body only on activation, resources almost never. The
100-token figure is the reason `description` quality dominates skill quality.

## 10. File references

**DOCUMENTED**: use relative paths from the skill root.

```markdown
See [the reference guide](references/REFERENCE.md) for details.

Run the extraction script:
scripts/extract.py
```

And: "Keep file references one level deep from `SKILL.md`. Avoid deeply nested reference chains." The reason is
progressive disclosure — a chain of references defeats the point, because the model cannot see what it is choosing
not to load.

## 11. Validation

**DOCUMENTED**: `skills-ref`, the reference library, at `github.com/agentskills/agentskills/tree/main/skills-ref`:

```bash
skills-ref validate ./my-skill
```

"This checks that your `SKILL.md` frontmatter is valid and follows all naming conventions."

Who performs validation is deliberately unspecified: the spec defines a format, not a gate. Implementations decide
whether an invalid skill is rejected, skipped, or repaired. Codex, for example, ships a **repair path** for
malformed third-party YAML — it will recover from a value like `description: Build for AWS: ECS`, which strict YAML
rejects (**VERIFIED-IN-SOURCE** `codex/codex-rs/skills/src/parser.rs`). A conforming validator would reject that
file. Both behaviours are legitimate under the spec.

## 12. Versioning

**UNKNOWN as a spec concept.** The specification defines no version field, no semver requirement, and no
compatibility/migration rules for skill formats. The only versioning affordances available are `metadata`, which
the spec's own example uses for exactly this (`version: "1.0"`), and whatever the hosting implementation adds —
Codex plugins, for instance, carry versions in their own manifest and cache path, not in `SKILL.md`.
(**DOCUMENTED** absence: the frontmatter table has no version row; **INFERRED** conclusion.)

Practical rule for authors: put a version in `metadata.version` and treat it as advisory. Nothing enforces it.

## 13. Where the spec stops

Worth stating plainly, because the temptation is to assume more structure than exists:

- **No required ordering, sections, or headings in the body.** Only recommendations.
- **No execution model.** The spec says scripts exist and that supported languages vary. It does not say how they run,
  with what permissions, or whether they run at all.
- **No trust model.** No signature, no publisher identity, no integrity hash.
- **No conflict resolution.** What happens when two skills share a `name` is unspecified — genuinely an
  implementation decision (Codex documents that both appear in selectors rather than merging; see the companion
  document).
- **No activation syntax.** `$name`, `/skills`, and automatic trigger are all outside the spec.

That thinness is a feature for portability and a hazard for security: everything that matters operationally is
delegated to the host.

## 14. Authoring checklist (derived)

Derived from the rules above; not a spec artefact.

- [ ] Directory name == `name` field, lowercase, hyphens, ≤64 chars, no `--`.
- [ ] `description` states capability **and** trigger keywords, ≤1024, no `<`/`>` if targeting Codex.
- [ ] Body under 500 lines; detail pushed to `references/`, one level deep.
- [ ] No `compatibility` if Codex compatibility matters (its validator rejects the key).
- [ ] `scripts/` self-contained with real error messages; `assets/` for anything an interface can reference.
- [ ] `allowed-tools` only if the host is known to honour it; never relied on for safety.

## Evidence index

| # | Claim | Source | Label |
| - | - | - | - |
| 1 | Skill = directory + `SKILL.md`, optional `scripts/references/assets` | agentskills.io/specification.md § Directory structure | DOCUMENTED |
| 2 | Frontmatter table: name/description required; license/compatibility/metadata/allowed-tools optional | same, § Frontmatter | DOCUMENTED |
| 3 | `name` 1–64, `a-z0-9-`, no leading/trailing hyphen, no `--`, matches parent dir | same, § `name` field | DOCUMENTED |
| 4 | `description` 1–1024, must carry both capability and trigger | same, § `description` field | DOCUMENTED |
| 5 | `compatibility` 1–500; "Most skills do not need" it | same, § `compatibility` field | DOCUMENTED |
| 6 | `metadata` = string→string map | same, § `metadata` field | DOCUMENTED |
| 7 | `allowed-tools` space-separated, Experimental, varies by implementation | same, § `allowed-tools` field | DOCUMENTED |
| 8 | Progressive disclosure: ~100 tokens / <5000 tokens / as-needed | same, § Progressive disclosure | DOCUMENTED |
| 9 | "Keep your main `SKILL.md` under 500 lines" | same, § Progressive disclosure | DOCUMENTED |
| 10 | References one level deep, relative paths | same, § File references | DOCUMENTED |
| 11 | `skills-ref validate ./my-skill` | same, § Validation | DOCUMENTED |
| 12 | No version field in the spec | same, § Frontmatter (absence) | INFERRED |
| 13 | Codex validator allow-list excludes `compatibility` | codex/codex-rs/skills/src/assets/samples/skill-creator/scripts/quick_validate.py:40 | VERIFIED-IN-SOURCE |
| 14 | Codex name regex `^[a-z0-9-]+$` | .../quick_validate.py:61 | VERIFIED-IN-SOURCE |
| 15 | `MAX_NAME_LEN = 64`, qualified cap 129 | codex/codex-rs/ext/skills/src/loader/mod.rs:22-23 | VERIFIED-IN-SOURCE |
| 16 | `MAX_DESCRIPTION_LEN = 1024` | codex/codex-rs/skills/src/interface.rs:11 | VERIFIED-IN-SOURCE |
| 17 | `MAX_CATALOG_SKILL_DESCRIPTION_CHARS = 1_024` | codex/codex-rs/ext/skills/src/render.rs:27 | VERIFIED-IN-SOURCE |
| 18 | Icons must be relative, under `assets/`, `..` resolves under plugin assets | codex/codex-rs/skills/src/interface.rs:88-144 | VERIFIED-IN-SOURCE |
| 19 | Frontmatter must open with `---` (`MissingFrontmatter`) | codex/codex-rs/skills/src/parser.rs | VERIFIED-IN-SOURCE |
| 20 | Prose-repair path for malformed third-party YAML | codex/codex-rs/skills/src/parser.rs | VERIFIED-IN-SOURCE |
| 21 | `allowed-tools` has no enforcement path in Codex | grep of codex-rs skills crates | INFERRED |
| 22 | Directory-name match not enforced by Codex validator | .../quick_validate.py (no comparison present) | INFERRED |
