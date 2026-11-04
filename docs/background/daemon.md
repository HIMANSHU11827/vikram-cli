# The daemon

The daemon is a foreground-or-background process that ticks the schedule.
Nothing about it needs the TUI open.

## Start it

```
vikram daemon start              # background: spawns, logs to $VIKRAM_HOME/daemon/daemon.log
vikram daemon start --foreground # stays attached; Ctrl+C stops it
vikram daemon status
vikram daemon health             # one line, for a prompt or a script
```

`status` prints whether a daemon is running, the home it owns, how many jobs
exist, the heartbeat state and which service manager this machine has.
`health` prints `ok · daemon pid N`, `stale · pid N is gone`, or `idle · no daemon`.

## Only one daemon per home

A lock file at `$VIKRAM_HOME/daemon/daemon.lock` holds the pid, the process start
time and the host name. A second `start` refuses while that process is alive.

The start time is what makes this reliable: a pid on its own is worthless,
because the OS reuses pids. Liveness is checked against `/proc/<pid>/stat` (field
22) with a two-second tolerance, so clock ticks cannot make a live daemon look
dead. A lock whose process is gone is taken over instead of blocking you
forever.

## Signals

- `SIGINT` / `SIGTERM` → finish the current tick, log `daemon stopping`, drop the lock, exit 0.
- `vikram daemon stop` sends `SIGTERM`, waits up to five seconds for the lock to
  disappear, and clears a stale lock if the process never answered.
- `vikram daemon restart` is `stop` then `spawn`.

## Crash handling

On the first tick after a crash, every run row still marked `running` is closed
as `unknown` with the note `interrupted by a restart`. It is never silently
retried: an interrupted run may already have had its side effect, so calling it
failed would be a guess. A retry is a new attempt of the same slot, governed by
the job's retry policy.

## Install it as a service

```
vikram daemon install      # writes the unit, prints the one command that activates it
vikram daemon enable
vikram daemon disable
vikram daemon uninstall
```

| OS | What is written |
|---|---|
| Linux (systemd) | `~/.config/systemd/user/vikram-daemon.service` |
| macOS (launchd) | `~/Library/LaunchAgents/dev.vikram.daemon.plist` |
| Windows | `$VIKRAM_HOME\daemon\vikram-daemon.xml` (Task Scheduler, `IgnoreNew`) |

Nothing escalates privileges: install writes a file and tells you the command
that finishes the job (`systemctl --user enable --now …`, `launchctl load -w …`,
`schtasks /Create …`). `--foreground` always works with no service installed.

## Logs

- `$VIKRAM_HOME/daemon/daemon.log` – background process output
- `$VIKRAM_HOME/daemon/notified.log` – delivered notices
- `$VIKRAM_HOME/daemon/service.log` – launchd stdout/stderr
