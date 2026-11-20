# Plugins

Plugin manifests, lifecycle, dependencies and integrity checks. Loaded at boot from the
default directory.

## Features

- Manifest-based plugin definitions.
- Dependency resolution.
- Integrity checks.
- Permission declarations.

## Files

- `crates/vikram-runtime/src/plugins/` (`manifest.rs`, `registry.rs`, `permission.rs`, `install.rs`)
- `crates/vikram-runtime/src/hooks.rs` — hook engine; see [hooks.md](hooks.md)

## Tests

Plugin tests in `vikram-runtime`.