# Commands

```
vikram daemon status|health|start [--foreground]|stop|restart|install|uninstall|enable|disable|reload
vikram cron list|show|history|add|edit|pause|resume|run|remove
vikram heartbeat status|run|enable|disable|pause|resume
vikram jobs list|inspect|cancel
```

## A worked example

```
$ vikram cron add greeting "every 2m" "echo hello from the daemon" --job-mode script --target local
added greeting-1793770141286 (every 2m)
next: 2026-11-04 09:31

$ vikram cron run greeting
greeting-1793770141286 is due now; the daemon will pick it up on its next tick

$ vikram daemon start --foreground
[vikram] daemon running in the foreground (pid 40707) — Ctrl+C to stop

$ vikram jobs list
2026-11-04 09:29  greeting-1793770141286  ok  hello from the daemon

$ vikram cron history greeting
2026-11-04 09:29  attempt 1  ok  hello from the daemon

$ cat $VIKRAM_HOME/daemon/notified.log
2026-11-04 09:29  greeting  ok  hello from the daemon
```

## Stopping work that is already running

```
$ vikram jobs cancel slow
asked run_slowcheck-1793771894408_1 to stop
```

`jobs cancel` takes a run id or a job id/name. A name means "the run of that job that is
running right now" — `jobs list` does not print run ids, so naming the job is the way an
operator reaches it. A cancelled run is recorded as `cancelled`, never as `failed`: it earns
no retry and raises no alert, and a one-time job still switches itself off.

## Scheduling by talking

The same operations are registered as agent tools, so the main agent in the TUI can do them
from a sentence instead of a flag list:

```
schedule.list      schedule.show      schedule.add       schedule.pause
schedule.resume    schedule.run       schedule.remove    schedule.history
daemon.status      daemon.heartbeat   jobs.list          jobs.inspect
jobs.cancel
```

Each one calls the same function the shell command does, so a job created by asking and a job
created by typing cannot diverge. Normal permissions apply: `schedule.remove` is destructive
and needs approval like any other destructive tool.

## Options for `cron add` / `cron edit`

`--job-mode`, `--target`, `--timezone`, `--description`, `--agent`, `--bot`,
`--retries`, `--timeout`, `--priority`, `--disabled`.

`cron edit` accepts the same flags and changes only what you pass.

## Where state lives

- `$VIKRAM_HOME/vikram.db` – jobs, runs, heartbeat scalars (tables `daemon_jobs`, `daemon_runs`, `daemon_state`)
- `$VIKRAM_HOME/daemon/daemon.lock` – the running daemon
- `$VIKRAM_HOME/daemon/daemon.log` – background output
- `$VIKRAM_HOME/daemon/notified.log` – local delivery

## In the TUI

`/schedule` (alias `/cron`) opens the background panel: one row per scheduled job with its
schedule, next run and last status, plus a row for the daemon and one for the heartbeat.
Enter on a job pauses or resumes it; Enter on the heartbeat row turns the beat on or off.
Both write to the same rows the daemon reads, so the change lands on its next tick without
restarting anything.
