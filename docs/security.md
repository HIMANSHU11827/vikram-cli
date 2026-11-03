# Security

## Threat model

Prompt injection (direct/indirect), tool injection, malicious files/plugins, unsafe shell
commands, secret leakage, permission escalation, cross-agent access, workspace isolation,
network access, browser security, database access, untrusted model outputs, supply chain.

## Mitigations

- Deny-first policy + non-widenable mandatory floor.
- Posture modes (incl. unattended, allow-list).
- Workspace root confinement + extra write roots.
- Untrusted-content verb floor (`grant.`/`policy.`/`self.`).
- Delegation depth limit.
- Secret redaction in evolution.
- Prompt-injection scanning for learned content.
- OS-level sandbox for spawned commands (bubblewrap/sandbox-exec).

## Gaps

- No taint tracking on observations.
- Floor only vetoes Write (not Execute/Network) outside root.
- No at-rest encryption for SQLite.
- No secret scanning in runtime.
- No dependency vulnerability scanning in CI.
- MCP serve is a stub; MCP auth_required not enforced.
- No output validation before context entry.

## References

- `docs/research/security/agent-security-and-permissions.md`
- `crates/vikram-policy/src/lib.rs`
- `crates/vikram-runtime/src/sandbox.rs`