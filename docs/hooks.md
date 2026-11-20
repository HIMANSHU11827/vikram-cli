# Hooks

Hooks are the extension surface for modifying the agent turn. A plugin contributes hooks through
its manifest; the host evaluates them at five points inside the real turn loop. Nothing is
replaced: the default context assembly, model call and tool execution still run, and the hook
changes what they receive.

## Points

| Point | Runs | Permission | How its input is filtered |
| --- | --- | --- | --- |
| `turn_start` | Before the turn does anything | `agents` | The user's request text |
| `context_build` | After the transcript and memory are assembled, before the model call | `agents` | The assembled context text |
| `model_request` | Just before the provider call | `agents` | The request text |
| `tool_call` | Before `tool.permission(...)` and before execution | `tools` | Tool name and arguments |
| `tool_result` | After execution, before the result is spooled into the transcript | `tools` | Tool name, arguments and result text |

A `tool_call` hook therefore judges rewritten arguments, and a `tool_result` hook can withhold
content from the model. There is deliberately no `turn_end` point: finalisation is synchronous, so
a blocking hook there would stall the daemon instead of extending it.

## Manifest

A plugin declares points under `components.hooks` and rules under `hook_rules`:

```json
{
  "id": "guard-rm-recursive",
  "version": "1.0.0",
  "components": { "hooks": ["tool_call"] },
  "permissions": ["tools"],
  "priority": 100,
  "hook_rules": [
    {
      "point": "tool_call",
      "tool": "shell.*",
      "arguments_contain": "rm -rf",
      "deny": "recursive force-delete is refused; delete the specific paths instead"
    }
  ]
}
```

Conditions are case-insensitive substring tests except `tool`, which is a glob (`*`, `shell.*`).
A rule with no effect, an effect on the wrong point, or an unknown point is refused at load time
and reported; it is not silently registered.

Effects:

- `deny` — refuse the operation. The first denial in the chain ends it.
- `prepend` / `append` — system text before or after the assembled context.
- `arguments_set` — a JSON object merged into the tool arguments (`tool_call` only).
- `append_result` / `replace_result` / `set_ok` — rewrite a tool result (`tool_result` only).
- `max_output_tokens` — cap the completion (`model_request` only).

## Ordering and isolation

Hooks run by `priority` descending, then plugin id ascending. Denials short-circuit; other effects
chain, and each hook sees the previous rewrite. Every hook call is wrapped so a panic or error is
recorded and the chain continues. `HookRegistry::rejected` records every manifest or rule the host
refused and why.

A plugin that declares a hook point without the matching permission is not registered at all.
Process hooks additionally need the `process` permission (and `network` when their entry point
runs a shell or interpreter).

## Process hooks

A manifest with an `entry` also receives a `ProcessHook`. The entry point is invoked as
`entry <point>`, gets the context as JSON on stdin, and writes a JSON effect object to stdout.
It has five seconds; overrun is killed and recorded as a failure. `.sh`/`.bash` entries run under
`sh`/`bash`, `.py` under `python3`, anything else directly.

## The shipped catalog

`modules/` contains 100 ready modules: 25 context instructions, 35 tool-call guards (34 refusals and
one argument rewrite), 20 result inspectors, 10 model-request caps/reminders and 10 turn-start
refusals. `modules/README.md` lists them all. They are plain plugin directories — no scripts, no
network — so the runtime evaluates them in-process.

```
vikram modules list
vikram modules install --all
vikram modules install guard-rm-recursive
vikram modules hooks
```

`vikram modules install` copies a module into the configured plugin directory; hooks reload on the
next plugin change, and `vikram plugins disable <id>` restores the default behaviour without
editing the runtime.

## Limitations

- Declarative rules are static: they cannot compute values or consult the clock.
- Matching is substring and glob only; there are no regular expressions.
- Hook decisions are journaled as `RecordKind::Hook`, but they are not replayed.
- A process hook is a separate process per call; it is not a persistent service.
