# VIKRAM LINK

One runtime — the machine that holds the sessions, the tools and the model key — serving many
linked devices. Each device gets its own token, its own tier and its own audit trail. The server is
`crates/vikram-link`; the CLI half is `vikram link`.

Nothing about a link token is ambient: a device can only do what its tier allows, every action is
recorded, and a token that is not presented is a 401.

## Tiers

| Tier | Reaches | May submit | Approvals | Tokens, audit, config | Notes |
|---|---|---|---|---|---|
| `admin` | everything | any session | answer, and keep a durable rule if it is the owner | yes | one `admin` token is the owner at a time |
| `main` | everything | any session | answer (never keep) | no | the root assistant |
| `bots` | bot sessions | any bot session | no | no | never the root session, no tool of its own |
| `bot:<name>` | one bot's sessions | that bot only | no | no | |
| `team` | team (hive) sessions | any team session | no | no | may delegate from inside a turn; never the root session |
| `team:<name>` | one team's session | that team only | no | no | |
| `read` | every session | nothing | no | no | read-only mirror: events and listings |

Reads are wider than writes on purpose: `read` sees every session, and still submits nothing. A
`bots` token reads only bot sessions — the root session is not "readable but unwritable" to a bot,
it is out of reach.

### Names, and the old ones

The canonical spellings above are what a token file stores and what a 403 prints. Four earlier
names are still accepted on the way in — a token minted before the rename keeps working — and are
never written back:

| Old | Reads as |
|---|---|
| `full` | `admin` |
| `vikram` | `main` |
| `hive` | `team` |
| `observe` | `read` |

An unknown tier is refused with the whole list, so a typo costs one command, not a debugging
session.

## Temporary and permanent tokens

Exactly one lifetime per token. There is no default, because a default would be an accidental
forever-token.

```console
$ vikram link token add laptop --tier admin --for 30m
minted `laptop` — tier admin, in 30m, at 2026-11-24T11:30:00Z
  store: /home/you/.vikram/link/tokens.json

secret (shown once — copy it now, it cannot be recovered):
  vlink_2f9c…

send it as `Authorization: Bearer <secret>`, or as `?token=<secret>` on /events.
```

- `--for <duration>` — temporary. A whole number and a unit: `90s`, `30m`, `2h`, `7d`. Anything else
  is refused with those forms quoted.
- `--permanent` — no expiry field at all in the store; revoke it by hand.
- Neither, or both, is an error.
- A temporary token is capped by `link.max_temp` (default `30d`) in `$VIKRAM_HOME/config.json`, as a
  duration string (`"30d"`) or a number of seconds. A `--for 365d` is refused with the ceiling
  named, so nobody quietly mints a year-long token.
- An expired token is refused with a 401 that names the expiry; `vikram link token rm --expired`
  prunes them.

`vikram link token list` prints name, tier, created, expiry (`in 29m` or `permanent`), last seen and
state. It never prints a secret — the store keeps only a salted SHA-256, and the list is built from
a summary type that has no hash field to print.

## Endpoints

Bound to `127.0.0.1:7717` by default. `--bind` changes it; `--tls-cert`/`--tls-key` add TLS (both or
neither).

| Method | Path | Tier | What it does |
|---|---|---|---|
| GET | `/health` | none | `{"ok":true,...}` — liveness only, no token needed |
| GET | `/sessions` | any | sessions this tier reaches |
| POST | `/sessions/{id}/submit` | `admin`, `main`, `bots`, `bot:<name>`, `team`, `team:<name>` | `{"text":"…"}` starts a turn; 202 accepted |
| GET | `/events` | any | WebSocket: the same `RuntimeEvent`s the terminal consumes, filtered to this tier |
| GET | `/link/tokens` | `admin` | token summaries (never secrets) |
| POST | `/link/tokens` | `admin` | `{"name","tier","for"\|"permanent","owner"?}` — the secret is in this response only |
| DELETE | `/link/tokens/{name}` | `admin` | revoke |
| GET | `/link/audit?limit=N` | `admin` | the append-only log, oldest first |
| GET | `/link/approvals` | `admin`, `main` | pending approvals |
| POST | `/link/approvals/{id}` | `admin`, `main` | `{"allow":bool,"persist":bool}`; `persist` is owner-only |

Authentication: `Authorization: Bearer <secret>`, or `?token=<secret>` on `/events` (a browser
WebSocket cannot set a header). The comparison is constant-time over the stored hash.

Status codes mean what they say:

- **401** — no token, an unknown secret, a revoked token, or an expired one. The body names the
  reason and, for expiry, the instant.
- **403** — a real token with the wrong tier. The body names the tier (`"tier":"bot:alpha"`) and why
  (`"session … belongs to the root operator"`).

The event stream is a JSON projection of `RuntimeEvent`: `{"type":"stream_delta","session":…,
"turn":…,"text":…,"ts_ms":…}`. Session-bearing events are filtered per tier before they are written
to the socket, so a bot token never hears the root session.

## Audit

One line per remote action, allowed or denied, in `$VIKRAM_HOME/link/audit.jsonl` (append-only,
mode 0600):

```json
{"ts_ms":1795462866754,"token":"laptop","tier":"admin","action":"submit","target":"ses-…","result":"ok"}
{"ts_ms":1795462866737,"token":"phone","tier":"read","action":"submit","target":"ses-…","result":"denied: tier `read` may not submit a turn: a read token receives events and submits nothing"}
```

The runtime journal is per session, and half of these actions belong to no session — minting a
token, opening the event stream — so the link keeps its own journal next to the token store. The
secret is never an argument to an audit record, which is the only guard that matters.

## Reaching it from another machine: a tunnel, not a port

The safest link is no listening port at all from the network's point of view. On the machine that
runs VIKRAM:

```console
$ vikram link token add laptop --tier admin --for 30m
$ vikram link serve                     # 127.0.0.1:7717
vikram link: serving http://127.0.0.1:7717
```

On the laptop:

```console
$ ssh -N -L 7717:127.0.0.1:7717 you@the-vm
$ curl -s -H "Authorization: Bearer $VIKRAM_LINK" http://127.0.0.1:7717/sessions
```

The token still matters — the tunnel carries it, it does not replace it — and it is what the audit
log records, so a tunnel is not a way to be anonymous.

A non-loopback bind (`0.0.0.0:7717`, a LAN address) is refused unless `VIKRAM_LINK_TOKEN` is set.
That value is the bootstrap owner token: it is accepted as `admin`, it is not in `tokens.json`, and
it is how the first real token gets minted over the network. Treat it as a password, and set it for
one session rather than in a shell profile.

TLS: `vikram link serve --tls-cert chain.pem --tls-key key.pem`. Behind a tunnel it is usually
redundant; across a network that is not a tunnel, it is not optional.

## A worked example

```console
# on the VM
$ vikram link token add laptop --tier admin --for 7d
$ vikram link token add buildbot --tier bot:alpha --for 30d
$ vikram link token add dashboard --tier read --permanent
$ vikram link serve --bind 127.0.0.1:7717

# on the laptop
$ ssh -N -L 7717:127.0.0.1:7717 you@the-vm
$ curl -s -H "Authorization: Bearer $ADMIN" http://127.0.0.1:7717/sessions
$ curl -s -X POST -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' \
       -d '{"text":"summarise today"}' \
       http://127.0.0.1:7717/sessions/<id>/submit
{"accepted":true,"session":"<id>"}

# the dashboard token can watch, and cannot type
$ curl -s -X POST -H "Authorization: Bearer $READ" -d '{"text":"hi"}' \
       http://127.0.0.1:7717/sessions/<id>/submit
{"error":"tier `read` may not submit a turn: a read token receives events and submits nothing","tier":"read"}

# what happened
$ curl -s -H "Authorization: Bearer $ADMIN" http://127.0.0.1:7717/link/audit?limit=20
```

## Limits

- The link serves the runtime it was started with; there is no second source of truth and no
  remote configuration write. `admin` may mint tokens, not change the model or the posture.
- One owner at a time. `--owner` on a new `admin` token moves ownership; revoking the owner leaves
  none until another is minted.
- The client is a later task. This server is exercised by `crates/vikram-link/tests/server.rs` over
  real sockets, not by a shipped device UI.
