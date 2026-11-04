# Where a result goes

Every finished run produces one `Notice` and hands it to the destination the job
was created with. `--target` decides:

| Target | What happens |
|---|---|
| `none` | Nothing is sent, and the run records `delivery: none`. This is not a failure. |
| `local` | One line is appended to `$VIKRAM_HOME/daemon/notified.log`. |
| `webhook:<url>` | The notice is posted as JSON. Only a 2xx counts as delivered. |
| `session:<id>` | The line is written into a conversation this runtime holds. |
| `model:<id>` | Runs the job with that model, then delivers to `local`. |

## Delivery into a conversation

The daemon keeps one runtime open for delivery, started before the first tick so
a morning report does not pay for a cold start. A `session:` target is written
through `RuntimeHandle::post_message`, which is how a delegation result arrives:
transcript, journal and event stream, and no model turn — a report is not a
prompt.

If the named session is open in that runtime, the report goes there. If it is
not (an id from another process, or one this runtime never had), the daemon
falls back to its own conversation titled **Background reports**, creating it
once and remembering its id in `daemon_state[delivery.session]`. The fallback is
recorded, not hidden: the run's `delivery` detail says so, and `vikram jobs
inspect` prints it:

```
delivery: delivered
          `ses-0000` is not open in the daemon's runtime, so the report went to
          Background reports (ses-01793786517470107447-000001)
```

Because the id is remembered in the database, the same conversation is reused
across daemon restarts — the runtime rehydrates its sessions from `vikram-store`
on start, so a report from Monday and one from Friday land in the same thread.

The conversation is a real session row, so it appears in the TUI's session list
and can be read there like any other.

## What delivery cannot do

- It cannot write into a session owned by a *different* process. Sessions live in
  memory of the runtime that opened them; the daemon has no way to reach into the
  TUI's. That case takes the documented fallback above.
- It cannot promise exactly-once. A crash between the side effect and the record
  leaves `delivery: pending`, and the run is marked `unknown` on the next start
  rather than replayed.
- A machine with no model configured cannot deliver into a conversation at all:
  the daemon prints `conversation delivery unavailable: …` at startup and records
  that reason on every run that asked for one. Script jobs still run and still
  deliver to `local` or a webhook.
