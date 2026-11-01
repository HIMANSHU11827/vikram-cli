# `SKILL.md` frontmatter reference

Status: reference for authors and reviewers. Authoritative source is
`docs/skills/skills-api-contract.md`; the parse behaviour is checked against
`crates/vikram-skills/src/frontmatter.rs` and `crates/vikram-skills/src/error.rs` as they stand
today. The crate is mid-build: `metadata.rs` (which turns frontmatter into `SkillMetadata`) does not
exist yet, so the field rules below come from the contract, not from a run.

## Label your claims

Same vocabulary as `docs/research/skills/agent-skills-standard.md`, so a claim can be audited later:

- **DOCUMENTED**: stated by the open Agent Skills specification at `agentskills.io/specification.md`.
- **CONTRACT**: stated by `docs/skills/skills-api-contract.md`. Authoritative for VIKRAM.
- **VERIFIED-IN-SOURCE**: read in this repository at a cited path and line.
- **INFERRED**: reasoned from the contract or source, not stated by either.
- **UNKNOWN**: no evidence found. Not used here as a polite word for a guess.

A row with no label is CONTRACT, because the tables below are restatements of the contract unless
marked otherwise.

## Document skeleton

```markdown
---
name: example-skill
description: What it does and when to use it.
---

Body markdown.
```

The frontmatter block is `---`, then YAML, then a second `---` line, then the body. The parser
accepts the YAML subset below and nothing wider (CONTRACT, `frontmatter.rs`):

| Construct | Accepted |
| - | - |
| Plain, single-quoted, double-quoted scalars | yes, with escapes in double quotes |
| Literal block scalars (pipe header) and folded ones (`>` header) | yes, with `-` and `+` chomping |
| Inline lists `[a, b]` | yes, including nesting |
| Indented maps and indented `- item` lists | yes, scalars and `key: value` items |
| `#` comments | yes, outside quotes |
| `yes`/`no`/`true`/`false`, `~`, `null`, integers | typed, not strings |
| Duplicate keys | rejected |
| Tabs in indentation | rejected |
| Unknown keys | kept, never dropped |

Anything outside the subset is kept as a string rather than guessed at
(VERIFIED-IN-SOURCE, `frontmatter.rs:5-8`). That matters for values like `3.5`, which stays a
string, and for a URL such as `http://example.com/x`, which stays one string because a colon is only
a key separator when a space or line end follows it.

## Top-level fields

| Field | Type | Required | Constraint | When absent | When invalid |
| - | - | - | - | - | - |
| `name` | string | no, with a warning | 1 to 64 characters; lowercase `a-z`, digits `0-9`, hyphen only; no leading or trailing hyphen; no `--` | the directory name is used and a warning is recorded | `SkillError::InvalidName` for bad characters, over-length, leading or trailing hyphen, or `--` |
| `description` | string | yes | non-empty, at most 1024 characters; over 400 characters is a warning, not a failure | `SkillError::MissingField` naming the field (INFERRED from the variant's `field` member) | rejected: over-length and empty are both listed as errors by the contract |
| `license` | string | no | none stated | `None` | no validation stated; stored verbatim |
| `compatibility` | string | no | none stated by the contract (see the standard's 500-character cap below) | `None` | no validation stated; stored verbatim |
| `allowed_tools` | list of strings | no | no validation stated | empty list | no validation stated |
| `metadata` | map of string to string | no | string keys, string values | empty | not specified (see "Known drift") |
| `vikram` | map | no | see the extension table | every extension field takes its default | unknown subkeys are not read (see "Known drift") |
| any other key | any YAML | no | none | not applicable | never rejected; preserved in `SkillMetadata.unknown` and in `Skill.frontmatter.entries` |

`NAME_MAX` is 64 and `DESCRIPTION_MAX` is 1024 (CONTRACT, the `metadata.rs` constants). The
400-character description threshold is a warning only: the skill still loads.

No other top-level key maps to a field. A key the contract does not name is preserved, printed by
`vikram skills show` under `unknown keys:`, and survives a `parse` then `to_yaml` round trip.

## The `vikram:` extension

`vikram:` is VIKRAM's own namespace. The open standard does not define it.

| Key | Type | Required | Constraint | Default when absent |
| - | - | - | - | - |
| `version` | string | no | none stated | `None` |
| `min_vikram` | string | no | none stated | `None` |
| `categories` | list of strings | no | none stated | empty |
| `tags` | list of strings | no | none stated | empty |
| `requires` | list of strings | no | each item is `name` or `name@>=1.2` | empty |
| `recommends` | list of strings | no | same spelling as `requires`; advisory | empty |
| `permissions` | list of strings | no | each item is `action:resource` | empty |
| `activation` | one of `auto`, `suggested`, `manual` | no | the enum is `ActivationPolicy` | `suggested` |
| `author` | string | no | none stated | `None` |
| `source` | string | no | none stated | `None` |
| `tests` | list of strings | no | none stated | empty |

Behaviour that follows from those fields:

| Field | What VIKRAM does with it |
| - | - |
| `min_vikram` | carried in metadata; the contract defines no compatibility check |
| `requires` | a missing member is reported as `SkillError::MissingDependency { owner, name }`, whose message says `requires ... which is not available` (`error.rs:34`); a cycle is `SkillError::CircularDependency { chain }` |
| `recommends` | advisory only, never an error |
| `permissions` | each item is parsed by `TrustPolicy::permission_request` and becomes a `PermissionRequest` with `Trust::External`; the kernel policy decides |
| `activation` | `manual` removes the skill from automatic selection; `suggested` is the default; `auto` is the permissive tier. No LLM and no embedding is involved: the selector scores tokens (CONTRACT, `activation.rs`) |
| `version` | surfaced in the catalog listing and in `SkillSummary.version`; nothing enforces it |

`permissions` accepts these action prefixes, each parsed as `action:resource` (CONTRACT, `trust.rs`):

| Spelling | Meaning |
| - | - |
| `fs.read:<path>` | read a path |
| `fs.write:<path>` | write a path |
| `shell.run:<command>` | run a command |
| `net.http:<host>` | reach a host |
| `delegate:<agent>` | hand work to another agent |
| `admin:<scope>` | administrative action |

An unrecognised action prefix is not described by the contract. Treat a permission the parser does
not know as ineffective, not as granted.

## Dotted-key aliases inside `metadata:`

The standard's `metadata` map is also read for VIKRAM fields, so a skill written for another host
keeps working (CONTRACT, `metadata.rs`):

| Key inside `metadata:` | Read as |
| - | - |
| `vikram.tags` | `vikram.tags` |
| `vikram.version` | `vikram.version` |
| `tags` | `vikram.tags` |
| `category` | `vikram.categories` (INFERRED: the contract says the key is accepted for categories but not how one value becomes a list) |
| `version` | `vikram.version` |
| `author` | `vikram.author` |

Nothing is invented: when none of these keys is present, the field is empty. What the contract does
not say is which side wins when both `vikram.tags` and `metadata.tags` are set. Pick one spelling
per skill.

## Fields from the open standard that VIKRAM does not implement

All six frontmatter fields the standard defines are accepted as fields: `name`, `description`,
`license`, `compatibility`, `metadata`, `allowed-tools` (DOCUMENTED for the set; CONTRACT for
VIKRAM accepting each). What VIKRAM does not implement is the semantics of three of them.

| Field or rule | The standard says | VIKRAM does | Result for the skill |
| - | - | - | - |
| `allowed-tools` enforcement | a space-separated list of pre-approved tools; experimental, support varies by host (DOCUMENTED) | declared as `allowed_tools: Vec<String>` and consumed by nothing in the contract: not the loader, not activation, not discovery | **ignored**: no warning, no rejection, no enforcement |
| `compatibility` length | 1 to 500 characters (DOCUMENTED) | accepted as `Option<String>`, and the contract's error list covers only name and description | **accepted silently**: an over-500-character value is not rejected |
| `name` equals the directory name | must match (DOCUMENTED) | a mismatch is a warning, not an error | **warned**: the skill loads under the directory's name |

The honest reading of the first row: `allowed-tools` is a declaration, not a capability. Writing
`allowed-tools: Bash(git:*)` does not create a sandbox, and does not widen one either. Tool access is
decided by the policy engine, outside this crate.

There is one more divergence worth naming here because it is a field-level spelling question: the
standard writes `allowed-tools` with a hyphen, while the contract names the Rust field
`allowed_tools`. The contract does not state the on-disk key. See "Known drift".

## How the body is stored and injected

| Property | Behaviour | Source |
| - | - | - |
| Stored as | `Frontmatter.body`, everything after the closing `---` line, verbatim, line endings included | CONTRACT, VERIFIED-IN-SOURCE (`crlf_documents_parse`) |
| Reaches the skill record as | `Skill.instructions`, the Markdown body | CONTRACT, `skill.rs` |
| Missing frontmatter | not an error: empty entry list, whole file becomes the body | CONTRACT |
| `---` with no closing delimiter | treated as a Markdown horizontal rule; whole file becomes the body | VERIFIED-IN-SOURCE, `split_document`, `frontmatter.rs:164-188` |
| Hard cap | `instruction_bytes` 24 000 by default; truncation keeps whole lines and appends `\n… (truncated at N bytes; read references/<file> for the rest)` | CONTRACT, `loader.rs` |
| Visible truncation | `LoadedSkill.truncated` is true and `instructions_bytes` records what was kept | CONTRACT |
| Total per skill | `total_skill_bytes` 48 000 by default; the contract does not say which parts it sums | CONTRACT |
| Resources | `resource_bytes` 12 000 and `max_resources` 32 by default; files over budget are listed in `LoadedSkill.omitted` | CONTRACT |
| Injected when | on activation; `Activation` records `source`, `reason`, `agent` and `loaded_resources` | CONTRACT, `activation.rs` |
| Required structure | none. No headings, no sections, no ordering is required by the standard or by VIKRAM | DOCUMENTED, CONTRACT |

The body is read on activation, not at discovery. Discovery reads only enough to build a
`SkillSummary`, "without reading the instructions" (VERIFIED-IN-SOURCE, `skill.rs:163-164`): name,
description, scope, compat, trust, enabled, valid, version, categories, tags. A body that costs
24 000 bytes is charged to every activation of that skill and to no other skill.

## Validation error catalogue

Every variant of `SkillError` (VERIFIED-IN-SOURCE, `crates/vikram-skills/src/error.rs:9-67`):

| Variant | What caused it | Fix |
| - | - | - |
| `NotFound { name, sources }` | no skill resolves to that name across the `sources` roots that were scanned | check the directory name, then `vikram skills list --all` |
| `InvalidName { name, reason }` | the name breaks a character or length rule | rename the directory and the `name` field to lowercase, hyphenated, 64 characters or fewer |
| `Io { path, reason }` | a filesystem read or write failed | fix permissions on the named path |
| `Frontmatter { path, reason }` | the frontmatter is not in the accepted YAML subset; the reason carries the line | see the parse-failure table below |
| `MissingField { path, field }` | a required field is absent, typically `description` | add the field |
| `PathEscape { path }` | a resource path resolves outside the skill directory: absolute path or `..` | reference the file relative to the skill root, inside it |
| `Disabled { name }` | the name appears in `<state_dir>/disabled.json` | re-enable the skill, or use a different name |
| `AlreadyExists { name, path }` | a create, import or install would overwrite an existing directory | choose another name, or delete the existing skill first |
| `MissingDependency { owner, name }` | a skill's `requires` names a skill that is not installed | install the named skill, or drop it from `requires` |
| `CircularDependency { chain }` | `requires` closes a loop; the chain names the members | remove one edge of the loop |
| `UntrustedScript { name, level }` | a script was requested at a trust level where `allows_scripts()` is false | run the skill from a `Builtin`, `Verified` or `UserTrusted` root, or drop the script |
| `TooLarge { what, size, limit }` | an item exceeded a hard size limit; `what` names it, `size` and `limit` give the numbers | move detail into `references/` and shorten the item |
| `MissingResource { name, rel }` | the body referenced a file that is not in the skill directory | fix the relative path, or add the file |
| `UpdateConflict { name, path }` | a rollback or update found local edits since the recording | commit or discard the local edits first |
| `InvalidBundle { path, reason }` | a bundle file is malformed or has an empty `skills:` list | fix the bundle file; bundles are not skill frontmatter |
| `Install(String)` | an install failed; the string is the underlying message | read the message; a git install needs explicit network consent |
| `Unsupported { what }` | the request names something the crate does not implement | not a skill-authoring error |
| `Denied { reason }` | policy refused the operation | a skill declaration cannot override this |

`SkillError::is_content_error()` is true for `Frontmatter`, `MissingField`, `InvalidName` and
`TooLarge` (VERIFIED-IN-SOURCE, `error.rs:86-94`). Those four are the ones that mean "this skill is
broken": discovery keeps such a skill in the catalog with `valid: false` and a diagnostic instead of
dropping it.

### Reasons inside the `Frontmatter` variant

Each of these fills `reason`, and the file that broke is in `path` (VERIFIED-IN-SOURCE,
`frontmatter.rs`):

| Reason | Cause | Fix |
| - | - | - |
| `duplicate key` (the key is named in the message) | the same key appears twice in one map, including nested maps | delete one |
| `tab characters are not allowed in indentation` | a tab opened a line | use spaces |
| `expected key: value on line N` | a line inside the block is not a mapping entry | quote the value, or indent it as part of the previous key |
| `unexpected indentation on line N` | a child is indented less than its parent expects | align children two spaces deeper |
| `empty mapping key` | a line begins with `:` | give the key a name |
| `frontmatter must be a mapping, found a sequence` | the block starts with `- item` | the top level is a map; move the list under a key |
| `unterminated double-quoted scalar` | a `"` has no partner | close the quote |
| `unterminated single-quoted scalar` | a `'` has no partner | close the quote |
| `unterminated escape in double-quoted scalar` | the string ends on a backslash | finish the escape |
| `truncated \u escape` | `\u` with fewer than four hex digits | supply four |
| `invalid \u escape` | `\u` followed by non-hex characters | use hex |
| `invalid code point` | the `\u` value is not a character | use a valid code point |
| `unexpected content after quoted scalar` | text follows a closing quote | move it inside the quotes or onto its own line |
| `unterminated inline list` | `[a, b` with no `]` | close the bracket |

## Known drift

Recorded, not smoothed over. Each is a real disagreement between the contract, the source, or the
standard at the time of writing.

1. **`Frontmatter.raw` is not the block the contract describes.** Contract: "the exact frontmatter
   block". Source: "the exact frontmatter text between the delimiters, without them"
   (`frontmatter.rs:92-93`), and `Frontmatter::with` rebuilds `raw` from the canonical re-emit
   (`frontmatter.rs:139-147`), so after any edit `raw` is no longer the bytes that were on disk.
2. **Two `Yaml` methods and one `SkillError` method are public but not in the contract.** `as_i64`
   (`frontmatter.rs:32`), `is_null` (`frontmatter.rs:81`) and `SkillError::is_content_error`
   (`error.rs:86`) extend the surface, while the contract says anything it does not list is private.
3. **`user_skill_dirs` is re-exported and is not in the contract.** `lib.rs:52` re-exports it from
   `paths`; the contract's `paths.rs` section lists `user_skills_dir` only.
4. **The on-disk spelling of two contract fields is unstated.** The contract names Rust fields
   `allowed_tools` and `min_vikram`. Whether the YAML keys are `allowed-tools` and `min-vikram` (the
   standard and the usual style) or the snake_case forms is not written in the contract. This
   reference uses the contract's field names in its tables for that reason.
5. **Non-string values under `metadata:` are unspecified.** The contract types the field as
   `Vec<(String, String)>`. What happens to a non-string value nested under it (a `retries: 3` that
   parses as `Yaml::Int`, not `Yaml::Str`) is not stated: stringify, warn, or reject are all
   consistent with the contract as written. Separately, a flow map on one line (`{a: b}`) is outside
   the parser's subset and is read as a plain string.
6. **Unknown nested keys under `vikram:` are not read.** `SkillMetadata.unknown` is documented as
   "preserved, never dropped", but it is one flat list and `VikramExtension` has no unknown bucket.
   A misspelled `vikram.tag:` is therefore not read into a field; whether an edit rewrites it is not
   stated.
7. **`Yaml::strings` does more than the contract says.** Contract: "scalar -> vec![s], list ->
   items". Source also flattens a `Map` into `key:value` strings and turns `Null` into an empty list
   (`frontmatter.rs:62-79`).
