# Workers

The pool starts work; it never decides what work exists.

## Task classes and ceilings

| Class | Default limit | Used by |
|---|---|---|
| light | 8 | checks and probes |
| script | 4 | shell jobs |
| agent | 2 | agent and delegate runs |
| research | 1 | long research runs |
| workflow | 2 | multi-step workflows |

A ceiling can never be zero: `Limits::new` raises it to one, because a zeroed
class looks exactly like a broken scheduler from the outside.

## Behaviour

- `dispatch` is non-blocking: it takes a permit or refuses with `Busy(class)`.
- A duplicate run id is refused, so a repeated tick cannot start the same run twice.
- Every task runs under a timeout; the timeout is the job's, minimum one second.
- `cancel(id)` aborts the task and reports it as `cancelled`, not failed.
- A finished task gives its permit back.
- Results come back as `Finished { id, class, outcome, cancelled, started_ms, elapsed_ms }`.

The interface is deliberately local: one process, one machine. Nothing assumes a
single machine forever — the run row is the unit of work, so a future distributed
worker only has to claim the same rows.
