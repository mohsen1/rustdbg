# Machine-readable output (`--json`)

Pass `--json` anywhere in the arguments and every `rdbg` command prints its
result as **one compact JSON line** on stdout instead of the human text
rendering (progress notes like `building …` still go to stderr):

```sh
rdbg --json launch --cargo . --lib --break src/lib.rs:37 -- my_test
rdbg --json vars
rdbg --json continue
```

This is the daemon's own response, exposed stably — the same object the MCP
server consumes. The schema below is a compatibility contract: fields may be
*added* in later versions, but the fields and `status` values listed here will
not be renamed or removed.

## Envelope

Every response is a JSON object with at least:

| field    | type   | meaning                                                       |
|----------|--------|---------------------------------------------------------------|
| `ok`     | bool   | success flag (kept for back-compat; `status == "ok"` ⇔ true)  |
| `status` | string | outcome classification, exactly one of the taxonomy below     |
| `error`  | string | present when `ok` is false: human-readable error message      |

### `status` taxonomy

| status                | meaning                                                                                    |
|-----------------------|--------------------------------------------------------------------------------------------|
| `ok`                  | the command succeeded                                                                      |
| `user_error`          | malformed/unknown arguments, a bad breakpoint spec (`file.rs:line`), an unknown id/frame/subcommand |
| `target_error`        | cargo has no such target: `no test target named`, `no bin target named`, no library unit tests, nothing debuggable |
| `build_error`         | cargo failed to compile the target (the rendered diagnostics are in `error`), or cargo itself could not run |
| `debug_adapter_error` | lldb-dap/codelldb missing, failed to spawn, or the DAP session broke                       |
| `timeout`             | a wait/reply timed out: daemon not responding, no stop/exit event, DAP reply timeout, rust-analyzer still warming up |
| `no_session`          | the command needs a debug session and none is running (`rdbg launch` first)                |
| `no_new_information`  | **reserved** — defined in the taxonomy but never produced yet (future: a command that succeeded but revealed nothing new, e.g. a stop identical to the previous one) |

Exit code under `--json`: `0` when `status` is `ok`, `2` for `user_error`,
`1` otherwise (`launch`/`trace` keep their existing codes: `2` for
argument/build failures, `1` for launch/trace failures).

## Per-command fields

Fields are the daemon's own names. `stop` objects (returned by anything that
runs the program) have this shape:

- still running / paused: `{"exited": false, "reason", "frame", "thread", "source", "watches", "delta"}`
  (`delta` — what changed in the top-frame locals since the previous stop; only
  on fresh stops, not on `state`/`thread` re-summaries)
- program ended: `{"exited": true, "exit_code", "output"}` (`output` — last
  2 KB of program output; only on fresh stops)
- from `continue --until`, the stop also carries `until`:
  `{"outcome": "held" | "exited" | "cap", "cond", "stops", "observed"}`
  (`stops` — resumes consumed; `observed` — the evaluated value when the
  condition held, else null)

| command | success fields (besides `ok`, `status`) |
|---------|------------------------------------------|
| `launch` | `stop` (first stop, or exit) |
| `trace`  | `trace` (rendered table), `hits` (int), `output` (string or null) |
| `run` / `continue`, `step`, `until`, `pause`, `restart` | `stop` |
| `continue --until '<cond>'` | `stop` (with the `until` outcome object above) |
| `thread <id>` | `stop` (summary for the selected thread, no `delta`/`output`) |
| `frame <n>` / `up` / `down` | `source`, `vars` |
| `state`  | `stop` (no `delta`), `vars`, `watches` |
| `vars`   | `vars` (rendered locals) |
| `eval <path>...` | `results`: array of per-path responses, each `{"expr", "ok", "status", "value"}`; top-level `ok`/`status` aggregate (first non-`ok` wins) |
| `set`    | `value` (`path = value` confirmation); with `--then`, also `then` (the resume's `stop` response, and the top-level `ok`/`status` reflect it) |
| `break` (line) | `id` (int), `verified` (bool) |
| `break --fn` | `id` (int) |
| `break --panic` | `id` (`"panic"`) |
| `watch <var>` | `id` (int) |
| `breaks` | `breakpoints` (rendered list) |
| `break-rm` | none (`ok:false` + `error` when the id is unknown) |
| `break-on` / `break-off` | none |
| `watch-expr` | `watches` (rendered list) |
| `bt`     | `bt` (rendered backtrace) |
| `list`   | `source` (rendered source window) |
| `threads`| `threads` (rendered list) |
| `status` | `session` (bool), `stopped` (bool), `lsp_ready` (bool), `cur_thread` (int or null), `threads` (int), `breakpoints` (int) |
| `where`  | `symbols`: array of `{"name", "container", "file", "line"}` |
| `def` / `refs` | `locations`: array of `{"file", "line", "col"}` |
| `hover`  | `hover` (string) |
| `do '<cmd>; ...'` | `steps`: array of `{"cmd", "response"}` (each `response` is that subcommand's object above); top-level `status` is the first non-`ok` step status |
| `stop`   | `stopped_session` (bool) |
| `down`   | none |
| `help` / no args | `usage` |
| `--version` | `version` |

Client-side failures (a build error before the daemon is involved, a bad
breakpoint spec, unknown arguments, a daemon that never responds) are emitted
in the same envelope: `{"ok": false, "status": ..., "error": ...}`.

Notes:

- `eval` reports missing variables inside `value` (e.g. `(cannot evaluate
  "x": ...)`) with `status: "ok"` — the evaluation round-trip succeeded; parse
  the `value` text if you need to score evaluability.
- One line per invocation, always: pipe through `json.loads(stdin.readline())`.

## MCP

MCP tool results carry the same taxonomy end-to-end: each `tools/call` result
includes `"_meta": {"rdbg/status": "<status>"}` alongside `content` and
`isError`, taken from the daemon response (or from the client-side
classification for build/argument failures). Score on `rdbg/status`; `isError`
stays the coarse failure flag it always was.
