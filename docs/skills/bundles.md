# Bundles

A **bundle** is a named group of skills that are meant to be installed and enabled together. Bundles are
declared in a `bundles.toml` file under the state directory and resolved against the current catalog.

## Where bundles live

- State dir: `<VIKRAM_HOME>/skills/.vikram` (or `<root>/.vikram` for a project root).
- File: `bundles.toml` inside that dir.
- `BundleIndex::load()` reads the default state dir; `BundleIndex::load_in(dir)` reads a custom one.

## Bundle shape

```toml
[[bundle]]
name = "starter"
description = "A minimal starter set"
members = ["skill-a", "skill-b"]
```

Each member is a skill name. `BundleIndex::resolve(name)` returns the bundle plus a per-member
`installed: bool` flag computed against the catalog. `resolve_in(name, &Catalog)` does the same with
an explicit catalog.

## CLI

- `vikram skills bundles` — list all bundles with member counts.
- `vikram skills bundle show <name>` — list members with `installed`/`not installed` and a
  `resolved: N required member(s)` summary. Exit 0 even when members are missing; the CLI reports
  `nope not installed` per member.

## Resolution semantics

- A bundle is **not** auto-installed. It is a declaration; `install`/`enable` are still per-skill.
- `resolve` reports `installed: false` for any member not present in the catalog. The bundle itself
  can still be shown.
- There is no implicit dependency resolution beyond membership.

## Non-negotiables

- Bundles never execute skill content. They only group names.
- A bundle cannot grant permissions or raise trust. Trust is per-skill, per-root.
