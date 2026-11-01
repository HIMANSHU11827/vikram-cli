# Bundle Authoring

A bundle is a named group of skills that are installed and activated together. Bundles are declared in a
`bundle.toml` file placed in the bundles directory (see `paths::bundles_dir()`).

## Bundle file format

```toml
name = "starter"
description = "Core skills for new projects"
version = "1.0.0"
members = ["code-review", "git-workflow", "testing"]
optional = ["docker-basics"]
```

| Field | Required | Type | Description |
| - | - | - | - |
| `name` | Yes | string | Unique bundle identifier |
| `description` | Yes | string | Human-readable summary |
| `version` | Yes | string | Semantic version |
| `members` | Yes | array of strings | Skill names that must be installed |
| `optional` | No | array of strings | Skill names that are installed if available |

## Resolution

`BundleIndex::resolve(name) -> Result<Bundle>` loads the bundle file and returns its member list. If a member skill
is not installed, resolution still succeeds — the missing member is reported but does not block the bundle.

`BundleIndex::resolve_in(name, &Catalog)` additionally checks each member against the catalog and reports which
members are installed vs. missing.

## CLI usage

```bash
vikram skills bundles              # list all bundles
vikram skills bundle show starter  # show members, mark missing ones
```

## Installing a bundle

There is no single "install bundle" command. Each member is installed individually:

```bash
vikram skills install <source> --yes   # for each member
```

Or use a bundle file as a manifest and install members in a loop.

## Bundle directory

Bundles live in `<home>/bundles/` (see `paths::bundles_dir()`). Each bundle is a single `.toml` file named after
the bundle: `<home>/bundles/starter.toml`.

## Validation

`BundleIndex::load()` parses all `.toml` files in the bundles directory. Malformed files produce a diagnostic but
do not prevent other bundles from loading.
