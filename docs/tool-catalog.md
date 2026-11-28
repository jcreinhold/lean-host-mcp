# Tool Catalog

Five semantic tools cover proof context, trials, lookup, verification, and status. `lean_context`, `lean_trial`,
`lean_lookup`, and `lean_status` select a mode with `kind`; `lean_verify` takes target groups directly. Operational
coordination uses `lean_runtime`; `lean_run` and `lean_get` start and read named requests. `lean_build` runs Lake, and
`lean_imports` reads what Lake reports a file imports.

- Semantic tools: [context](#lean_context), [trial](#lean_trial), [verification](#lean_verify), [lookup](#lean_lookup),
  [status](#lean_status).
- Shared contracts: [response shape](#response-shape), [output carrier](#output-carrier),
  [request synonyms](#request-synonyms), [proof workflow](#proof-workflow).
- Coordination: [native compatibility](#native-compatibility), [named requests](#named-requests),
  [worker activity](#worker-activity).
- Lake: [builds](#lean_build), [import closures](#lean_imports).

```text
lean_context -> lean_lookup -> lean_trial -> lean_verify
                         \-> lean_status
```

The semantic tools read files and elaborate in memory without applying source edits automatically. Lean commands,
tactics, and import initializers can perform IO with the server user's privileges; the worker is not an OS sandbox. Only
use trusted local clients and projects.

Invocation/runtime failures with no domain data set MCP `isError: true` and retain the semantic `errors` and `trust`
envelope. Negative proof/check verdicts remain successful tool executions with domain data, not MCP errors.

## Native compatibility

The broker binds its required host ABI subset for its lifetime and checks that subset on every host session, including
after internal replacement. Extra compatible loaded operations are allowed. A missing or incompatible runtime is refused
before the caller's proof runs. After each completed call, the worker's actor checks the host facts the child reported
and keeps a receipt that binds them to its session and worker generation. `lean_status` reports these receipts:

```json
{"name":"lean_status","arguments":{"kind":"project","include":["worker"]}}
```

`data.worker.compatibility` holds the broker instance, a contract digest, the expected native API, lifecycle and binding
contract, and one receipt for each of the project's resident workers that has completed a call on its current
generation. It is absent when no worker has one; `lean_status` never opens a worker to get one. Worker package versions
are observations; compatibility is not tied to an executable hash or source checkout revision. Compiled Lean version and
header facts are separate from `runtime_lean_version`, which comes through the loaded version shim. Compatibility never
certifies execution, a proof verdict, worker exit, absence of prior IO, or readiness of arbitrary first-party artifacts.

Check replies do not carry receipts. To recover a lost reply, read the same `request_id` with `lean_get` rather than
sending the call under a new id. `lean_runtime` is an advisory snapshot and never opens a worker.

## Tool Roles

- `lean_status`: cheap project, toolchain, build-artifact, and diagnostics status. Use it before spending a worker
  permit or when you need file diagnostics.
- `lean_context`: local proof state at a declaration position. Use it to see goals, locals, expected type, and
  diagnostics before choosing a tactic.
- `lean_trial`: in-memory probes. Use `proof_step` to try tactics and `command` for `#check`, `#eval`, or
  `#print axioms`.
- `lean_lookup`: semantic discovery. Use it for declarations, declaration inventory, proof search, and reference search.
- `lean_verify`: verification gates. Use it when a declaration, file, or module must be checked with `sorry` policy and
  optional axiom reporting.

When an MCP request carries `_meta.progressToken`, every tool emits a start notification, a heartbeat every 15 seconds,
and a completion notification on `notifications/progress`. Clients such as Claude Code extend their per-call deadline
only while progress arrives, so a cold import or a large file's elaboration is not abandoned at the client while the
worker keeps computing. Clients that omit the token keep the same single-response behavior.

The server offers no MCP Tasks. To keep a result past a dropped connection or a server restart, name the call with
`request_id`; see [named requests](#named-requests).

For one explicit named file group on the ordinary setup-aware `lean_verify` route, a complete verdict includes
`trust.cli_file_verification` with schema `lean-host-cli-file-verification-v1`. It identifies the `server_instance_id`,
canonical `project_root` and `source_file`, selected `lean_toolchain`, `lean_executable`, and `runtime_lean_version`
reported by the executing Lean CLI. The runtime version must match the selection before that process opens the staged
setup.

The receipt includes lowercase SHA-256 strings `source_sha256`, `module_setup_sha256`, `artifact_identity_sha256`,
`request_sha256`, and `data_sha256`. These bind the exact source bytes, staged ModuleSetup bytes, checked input
identities and installed shim digests, the staged driver request bytes, and the returned `data` respectively. The `data`
digest uses compact UTF-8 JSON with sorted object keys and unchanged array order. A caller must check the exact
requested names and compare the source digest against its saved input before accepting a verdict. The receipt
establishes CLI provenance. `lean_status` reports loaded-worker ABI evidence; see
[native compatibility](#native-compatibility).

### Named requests

A call that carries `request_id` in its arguments is a named request. The server writes a row for it in the request
table before anything runs, runs it at most once, and keeps its result. An id matches `^[A-Za-z0-9._:-]{8,128}$`; ids
that start with `srv-` belong to the server. `tools/list` shows the optional `request_id` property on every tool except
`lean_run`, `lean_get` and `lean_runtime`, which never run as requests.

```json
{"name":"lean_verify","arguments":{"request_id":"verify-7c1d2e3f","targets":[{"kind":"file_all","file":"Chain/C.lean"}]}}
{"name":"lean_run","arguments":{"request_id":"build-7c1d2e3f","tool":"lean_build","arguments":{"targets":["Chain.C"]}}}
{"name":"lean_get","arguments":{"request_id":"build-7c1d2e3f","wait_seconds":60}}
```

- A tool called with `request_id` runs the request, waits for its end and returns the tool's result. If the connection
  drops, the wait ends and the request goes on; the same call again returns the stored result.
- `lean_run {request_id, tool, arguments}` records the request, starts it and replies at once with `{request: row}`. The
  inner `arguments` must not hold `request_id`.
- `lean_get {request_id, wait_seconds?}` returns `{request: row, result}`. `result` is null until the request ends, then
  the stored tool result, which a direct call would have returned. A request that a server start ended has a null
  result. `wait_seconds` (default 0) waits up to that long for the end; `[requests] wait_max_secs` caps it.

The row is `{request_id, tool, kind, state, created_at, deadline_at, started_at?, finished_at?, exit_code?, log?}`.
`kind` is `build` for `lean_build` and `check` for every other tool. Times count milliseconds since the Unix epoch, and
`log` names a build's log in the store. `state` is one of:

- `queued`: recorded, and waiting for its turn on the project;
- `running`: it holds its turn and its work has started;
- `done`: the tool returned. Its result may be a tool error or a negative verdict;
- `timed_out`: the deadline passed. A check says so only once its worker request is seen to stop; when no one can see
  the stop, the row says `done`;
- `interrupted`: a server stopped before the request returned, and the next start ended it. It never runs again.

The intent of a request is the SHA-256 of the JSON of `{tool, arguments}` without `request_id`, with no spaces and keys
sorted at every level. The same id with the same intent returns the stored row, from any client and after a restart.
Another intent fails with `request_id_conflict`, which names the stored tool and state. Ids are global, so a reused id
returns the old result.

Nothing cancels a named request: it ends when its tool returns, at its deadline, or at the next server start. The row
fixes the deadline when it is written: `runtime.request_timeout_millis` for a check, `[build] timeout_secs` or the
recipe's `timeout_secs` for a build. The wait for the project's turn counts. A build takes the write turn before its row
says `running`; a check takes the read turn first. Checks on one project share the read turn, so their rows run at once,
with at most `broker.max_lean_runs` Lean or Lake processes of theirs at a time (see
[Operations](operations.md#rebuilt-artifacts)). A check sent during a build waits for it. Deadlines use wall-clock time,
which counts a laptop's sleep.

When a server starts, it ends the rows an earlier server left. Before it serves, it waits until every process the
earlier server started has exited (see [Operations](operations.md#one-server-per-store)), then marks each `running` row
`interrupted`. It starts `queued` rows in the order they came, unless the deadline has passed (`timed_out`) or the
project is gone (`interrupted`).

A call without `request_id` gets a server-made id `srv-<uuid>` and no row. When its connection drops, the server cancels
it, as it does on MCP `notifications/cancelled`. A crash loses it.

Every reply carries `request_id` at the top of its `structuredContent` and of its first text item when that is JSON, and
every JSON-RPC error carries it in `error.data`. A refusal is a tool error (`isError: true`) with this body:

```json
{"error":{"code":"request_id_conflict","message":"request build-7c1d2e3f already holds another call: lean_build (running)"},"request_id":"build-7c1d2e3f","request":{"request_id":"build-7c1d2e3f","tool":"lean_build","kind":"build","state":"running","created_at":1764150000000,"deadline_at":1764164400000,"started_at":1764150000120}}
```

| Code | Meaning |
| --- | --- |
| `invalid_request_id` | The id breaks the rule above. |
| `invalid_arguments` | The tool refuses the arguments, or the `lean_run` arguments hold `request_id`. |
| `unknown_tool` | `lean_run` names a tool this server lacks, or `lean_run`, `lean_get` or `lean_runtime`. A direct call to a tool the server lacks fails with JSON-RPC invalid params. |
| `request_id_conflict` | The id holds another call; `request` is its row. |
| `request_not_found` | `lean_get` names an id with no row. |
| `request_timed_out`, `request_interrupted` | A direct call with this id ended without a result; `request` is its row. |
| `requests_unavailable` | The server has no store. Start it with `--lake-root`. |
| `request_store_failed` | The table failed. `retryable` is true: ask again after `retry_after_millis`. |

No refusal makes a row. The five semantic tools decode most of their arguments only when they run, so a bad argument to
one of them makes a row whose result is the tool's error. A stored result of a tool that failed with a JSON-RPC error is
a tool error of the same shape, whose code is the error's `reason`, `invalid_arguments` or `internal_error`.

[Operations](operations.md#request-table) describes the table and its settings. `lean_runtime` reports
`requests: {queued, running}` and the capability `durable_requests_v1`.

## Worker activity

`lean_runtime` takes no arguments, replies at once and never runs as a named request. It reads the server's own state,
never opens a Lean worker, and pauses or retires nothing.

Every response includes `build` with package `version`, full Git `revision`, and build-time `dirty` state. Missing Git
metadata (for example, an exported source archive) is reported as null, never guessed from the runtime checkout. A Git
revision identifies source, not the resolved dependency graph or binary hash. Build/install with `--locked` for
reproducible dependency selection, and retain the executable hash when an exact runtime artifact must be pinned.
`effective_settings` reports resolved broker/runtime values, including derived heap/residue limits, plus server
presentation and output overrides. Null output overrides mean each tool keeps its own default budget. These are the
values the running process actually uses, not the current contents of a configuration file. No raw environment or
credentials are exposed. Transport addresses are not part of the transport-agnostic tool service.

On a server with a store, `requests: {queued, running}` counts the named requests that have not ended; a server without
one leaves `requests` out and does not list `durable_requests_v1`.

`pool` describes the workers:

| Field | Meaning |
| --- | --- |
| `instance_id` | This broker lifetime; it changes when the server restarts. |
| `max_workers`, `max_workers_per_project` | The configured slot limits. |
| `occupied_slots` | Slots still owned: a worker starting, resident, retiring, or whose exit is not confirmed. |
| `outstanding_jobs` | Admitted requests, startups, preparation and maintenance, each counted until its cleanup ends, including work of a worker no longer in `projects`. |
| `workers_stopped` | True only once every slot and every preparation cleanup is released. |
| `acquiring_projects` | Roots whose callers are preparing, starting, or waiting for a worker. |
| `projects` | Each resident worker: `idle`, `outstanding_jobs`, `admitted_caller`, `healthy`, and its last published `runtime` facts. |

A snapshot is advisory: another request can start right after it. A slot counts until its worker's exit is confirmed, so
an uncertain shutdown is not freed memory.

## `lean_build`

`lean_build` runs Lake in the project root. Send Lake `targets`, or a `recipe` from the project's config with optional
`args`. The two exclude each other; with neither, Lake builds its default targets.

```json
{"name":"lean_build","arguments":{"targets":["Chain.C"]}}
{"name":"lean_build","arguments":{"recipe":"format","args":["MyProject/Foo.lean"]}}
```

The build waits for the project's write turn: checks already running finish first, and checks sent later wait until the
build ends. Then it stops the project's workers, so the next check starts a fresh worker on what the build wrote, even
when the build changed nothing. Each step runs as `lake <args>` in its own process group, and the build ends only after
every group has exited. Lake decides what to build and in which order. When `[build] guard` names targets, the server
adds the step `lake build --no-build <guard>` before the build (or where the recipe's `guard` puts it), so a build that
would compile those targets stops instead. The first failing step ends the build. The config, recipes and the words the
server refuses are in [operations](operations.md#builds).

```json
{
  "state": "done",
  "ok": false,
  "exit_code": 1,
  "failed_step": 1,
  "steps": [
    {"argv": ["lake", "build", "--no-build", "@dep/+Dep:olean"], "exit_code": 0,
     "started_at_ms": 1764150000000, "finished_at_ms": 1764150000420},
    {"argv": ["lake", "build", "Chain.C"], "exit_code": 1,
     "started_at_ms": 1764150000421, "finished_at_ms": 1764150003870}
  ],
  "log": {"path": "/abs/project/.local/lean-host-mcp/store/logs/3f0c….log", "bytes": 5120, "truncated": false},
  "tail": "error: ./././Chain/C.lean:3:2: type mismatch …"
}
```

- `state` is `done` when every step that should run ran, and `timed_out` when `[build] timeout_secs` stopped the build.
  The wait for the turn counts toward that deadline.
- `ok` is true when every step ran and passed. The reply sets MCP `isError` when `ok` is false.
- `exit_code` is the first failing step's code, `0` when all passed, and null when the build was stopped. `failed_step`
  is that step's index in `steps`.
- Each step's `exit_code` is Lake's code, or 128 plus the signal that ended it.
- `log` names the whole log of every step, in the server's store. Output past `[build] log_limit_mib` is dropped and
  `truncated` is true. `tail` holds the log's last 8 KiB, or less when `[output] max_field_bytes` is smaller.

A request the config does not allow, such as an unknown recipe or a word Lake must not see, fails with MCP invalid
params before anything runs.

## `lean_imports`

`lean_imports` lists each file's whole transitive import closure, not only the file's direct imports. It reads the
closure from `lake --no-build --no-cache setup-file <file>` (the `importArts` Lake reports), and the sources of the
modules the root built from one `lake --no-build --no-cache query --json +MODULE:lean ...`, under the project's read
turn, so it waits for a running build. It builds nothing.

```json
{"name":"lean_imports","arguments":{"files":["Chain/C.lean"]}}
```

```json
{
  "files": [
    {
      "file": "Chain/C.lean",
      "module": "Chain.C",
      "imports": [
        {"module": "Chain.B", "olean": "/abs/project/.lake/build/lib/lean/Chain/B.olean",
         "source": "/abs/project/Chain/B.lean"},
        {"module": "Chain.A", "olean": "/abs/project/.lake/build/lib/lean/Chain/A.olean",
         "source": "/abs/project/Chain/A.lean"},
        {"module": "Dep", "olean": "/abs/project/dep/.lake/build/lib/lean/Dep.olean", "source": null}
      ]
    }
  ]
}
```

- `files` takes 1 to 16 Lean files, absolute or relative to the Lake root. Each reply entry keeps `file` as the request
  named it.
- `olean` is the absolute path Lake loads. `source` is the absolute source path Lake names, under the module's library
  `srcDir`, when the `.olean` lies under the root's own `.lake/build`; for a dependency's module it is null.
- When an import of a file is missing or stale, the call fails as a whole: the result sets MCP `isError` and holds
  `{"error":{"code":"setup_file_failed","message":…}}`, whose message names the file and quotes Lake. A file the server
  cannot read fails the same way with code `invalid_request`, and a source query Lake cannot answer with code
  `module_sources_failed`. The reply never holds a partial list.

## Response Shape

The five semantic tools return this response baseline:

```jsonc
{
  "data": { "...": "mode-specific result" },
  "errors": [],
  "trust": {
    "project_root": "/abs/project",
    "session_id": "metadata-only-or-worker-session",
    "lean_toolchain": "leanprover/lean4:v4.35.0",
    "artifacts": [
      {
        "artifact": "ilean",
        "scope": "project",
        "status": "build_fresh",
        "detail": "project reference index is current for contributing modules"
      }
    ]
  }
}
```

`trust.artifacts` is omitted when empty. Rows use these stable tokens:

- `artifact`: `source`, `olean`, `ilean`, `worker`
- `scope`: `file`, `module`, `project`, `toolchain`
- `status`: `edit_fresh`, `build_fresh`, `stale_build`, `missing_build`, `unknown`, `not_applicable`

Rows may also carry `path`, `module`, `detail`, `next_action`, and a source `content_sha256`. Paths are rendered
project-relative when the file is inside the resolved Lake root, and absolute only for files outside that root.
Source-overlay tools and source-backed declaration inventory report the source file snapshot as `source` / `file` /
`edit_fresh`; project reference lookup and index-backed declaration inventory report `.ilean` build freshness or
missing/stale build state; `needs_build` degradations report missing `.olean` artifacts, or a `stale_build` row for each
module Lake lists as out of date when it sets up a file. Duplicate trust rows are collapsed while preserving first
occurrence order. Runtime counters, cache timings, and import lists remain telemetry, not trust.

Every reply whose result came from a source file's text, including a `needs_build` reply, has a `source` / `file` /
`edit_fresh` row for that file. Its `content_sha256` is the lowercase SHA-256 of the exact bytes the call read and
checked. When the call checked one file, `trust.source_sha256` repeats that digest: compare it with the text you meant
to check before you trust the result. A `lean_verify` call that names several files, or that read one file twice and saw
an edit between the reads, has one row per text and no `trust.source_sha256`. This holds for `lean_context`,
`lean_trial`, `lean_verify`, `lean_status` with `file_diagnostics`, and `lean_lookup` with `declarations`, `references`
or `proof_search` when they read a file. `lean_lookup` with `declaration` reads only a file's header to choose imports,
so its result comes from built artifacts and carries no source row.

Lean-domain outcomes remain data. A failed tactic, a rejected declaration, an ambiguous name, or a `needs_build` verdict
is not an MCP transport error. Infrastructure failures appear in `errors` with structured details and a `retryable`
flag; follow that flag, since restart-loop exhaustion and compatibility refusals require recovery. Warnings and next
actions also appear as warning issues in `errors`.

### Call timing and the slow-call warning

Every call's telemetry `runtime` block carries three timing facts: `queue_wait_millis` (admission delay before the
worker started the call), `call_elapsed_millis` (worker-call wallclock, queue wait excluded, including any recycle and
retries the call triggered), and `call_cpu_millis` (the worker child process's own CPU delta around the call — sampled
at 10 ms granularity on Linux, nanosecond precision on macOS — omitted when the platform cannot sample it or a mid-call
recycle invalidated the baseline). Under the default `telemetry.verbosity = quiet` the telemetry block is dropped, but
the actionable signal is not: when `call_cpu_millis` exceeds `output.slow_call_warning_millis` (default 10000), the
response carries a warning issue naming the CPU figure and recommending refactoring (split the declaration, add explicit
type annotations, extract intermediate lemmas). Keying on CPU rather than wallclock keeps machine contention from ever
triggering it; when wallclock is more than twice the CPU figure the warning says so instead.

Omission convention: completeness flags named `*_truncated` (plus `RenderedText.truncated`, `partial`, and advisory
containers such as `post_closure_diagnostics`, `entry_goals`, and `locals`) are omitted from the JSON when they carry
their empty/default value (`false` or empty); an absent flag means complete/untruncated. The examples below show
`true`/populated cases where relevant and may omit the default-valued keys. Verdict and trust facts — `contains_sorry`,
`contains_admit`, `contains_sorry_ax`, `axioms_available`, `facts_trustworthy`, and declaration attribute flags — always
serialize, `false` included, so a client never silently defaults a check that matters.

## Output Carrier

By default, the serialized semantic response is placed in MCP `content` text so model clients can read it reliably.
Programmatic clients can request structured output at server startup:

```sh
LEAN_HOST_MCP_RESPONSE_CARRIER=structured lean-host-mcp --lake-root /path/to/project --bind 127.0.0.1:8765
```

Use `LEAN_HOST_MCP_RESPONSE_CARRIER=both` to mirror the same JSON into both `content` and `structuredContent`.

## Request Synonyms

The decoder maps these aliases to the canonical request shapes before typed decoding. Responses use canonical names.

- Modes: `lean_lookup` `signature`/`print`/`decl` → `declaration`; `inventory`/`outline` → `declarations`; `lean_trial`
  `tactic`/`step` → `proof_step`, `snippet`/`eval`/`check` → `command`; `lean_context` `goal`/`state` →
  `proof_position`; `lean_status` `health`/`capabilities` → `project`, `diagnostics` → `file_diagnostics`.
- Fields: `lean_lookup(kind="declarations")` accepts a bare `file`/`path` or `module` in place of `target`;
  `lean_lookup(kind="declaration")` accepts `declaration` for `name` and `value`/`type`/`docs` as field names;
  `lean_trial(kind="command")` accepts `command`, `code`, or a line array for `commands`;
  `lean_trial(kind="proof_step")` accepts `tactic` for `snippet` and `candidates` for `snippets`; a `proof_position`
  given as a string is the entry goal (`"start"`) or an `after_text` match, and as an integer an `index`.
- `lean_verify` accepts one group at top level (`file` with `declarations`, `file` alone, or `module` alone), a singular
  `target`, string groups (`"MyProject/A.lean"`, `"MyProject.A"`), and the group kinds `file` → `file_all`, `module` →
  `module_all`, `declarations` → `explicit`.
- A `kind` that names another tool's job (`search`, `file`, `verify`, …) is still rejected, with the error naming the
  tool and mode to call instead.

## Common Workflows

### Proof workflow

1. Call `lean_context` with `kind: "proof_position"` to read the current proof goals, locals, and diagnostics for a
   declaration position. Add `include_boundaries: true` when you need the boundary list to pick a selector, and
   `include_expected_type: true` when you need the goal's expected type; both default to `false`.
2. Call `lean_lookup` with `kind: "proof_search"` to retrieve ranked declarations for the goal.
3. Call `lean_lookup` with `kind: "declaration"` to inspect a promising declaration's statement, docstring, attributes,
   and flags.
4. Call `lean_trial` with `kind: "proof_step"` to try one or more tactics in memory without editing the file. The trial
   batch at `data.result` carries `entry_goals` and `locals` for the resolved position, so each trial supplies the
   context for the next step.
5. Call `lean_verify` with an explicit target group to verify the target declaration.

Use `lean_lookup` with `kind: "references"` when the task is semantic reference discovery rather than proof search. Use
`lean_status` for cheap project and host status before spending a worker permit.

### Tagged Request Shapes

Several arguments are tagged enums. The tag is always a string field named `kind`.

`DeclarationInventoryTarget` for `lean_lookup(kind="declarations")`:

```json
{ "kind": "file", "path": "LeanRsFixture/ProofAgent.lean" }
```

or:

```json
{ "kind": "module", "module": "LeanRsFixture.ProofAgent" }
```

`ProofPositionSelector` for `lean_context(kind="proof_position")`, `lean_trial(kind="proof_step")`, and
`lean_lookup(kind="proof_search")`:

```json
{ "kind": "default" }
```

```json
{ "kind": "index", "index": 0 }
```

```json
{ "kind": "after_text", "text": "skip", "occurrence": 0 }
```

## `lean_context`

### `kind: "proof_position"`

Returns proof context for one declaration proof position. The request fields are the existing declaration anchor plus an
optional proof-position selector:

```json
{
  "kind": "proof_position",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "proof_position": { "kind": "default" }
}
```

The response omits echoed declaration and namespace names. Two fields are opt-in, both defaulting to `false`:

- `include_boundaries: true` returns the `proof_boundaries` navigation list (and `proof_boundaries_truncated`). Request
  it once per declaration to pick a position selector.
- `include_expected_type: true` returns the goal's `expected_type`.

When `proof_position` is omitted, the default is the pristine entry goal: the state before any tactic runs. This is the
same position where a default `lean_trial(kind="proof_step")` snippet is spliced.

An `index` selector chooses a recorded tactic boundary (`index: 0` is after the first tactic). `after_text` chooses a
worker-recognized boundary matching a source fragment. Not every substring is a boundary; inspect `goals_before` and
`goals_after` to determine the state available at the match. [Selector shapes](#tagged-request-shapes) are shared with
trials and proof search.

If an `after_text` selector does not resolve, the result stays a normal Lean-domain response and — when
`include_boundaries` is set — includes valid `proof_boundaries`:

```json
{
  "kind": "proof_position",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "proof_position": { "kind": "after_text", "text": "not a boundary" },
  "include_boundaries": true
}
```

```json
{
  "status": "context",
  "unavailable": [
    {
      "id": "proof_state",
      "message": "declaration has no proof position matching the selector"
    }
  ],
  "proof_boundaries": [
    {
      "index": 0,
      "kind": "entry",
      "selector": { "kind": "default" },
      "source": { "start_line": 2, "start_column": 3, "end_line": 2, "end_column": 10 },
      "excerpt": { "value": "intro h" }
    },
    {
      "index": 1,
      "kind": "after_tactic",
      "selector": { "kind": "index", "index": 1 },
      "source": { "start_line": 3, "start_column": 3, "end_line": 3, "end_column": 10 },
      "excerpt": { "value": "exact h" }
    }
  ]
}
```

Retry with the returned selector:

```json
{
  "kind": "proof_position",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "proof_position": { "kind": "index", "index": 1 }
}
```

The `data` payload contains status, diagnostics, goals, locals, requested optional fields, truncation, and any
`needs_build` or ambiguity facts.

## `lean_trial`

### `kind: "proof_step"`

Tries one or more proof snippets at a declaration proof position against an in-memory source snapshot. It never writes
files.

```json
{
  "kind": "proof_step",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "snippet": "trivial"
}
```

Use `snippets` to try a bounded list independently:

```json
{
  "kind": "proof_step",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "snippets": ["simp", "exact h"]
}
```

The worker attempts candidates independently in request order and returns at most 16 rows. The batch is at
`data.result`; `candidate_limit` records the cap, `candidates_truncated` marks omitted candidates, and `summary` gives
the counts. An abridged batch:

```json
{
  "candidates": [
    { "id": "candidate_1", "status": "closed" },
    { "id": "candidate_2", "status": "failed" }
  ],
  "candidate_limit": 16,
  "summary": {
    "requested_candidates": 2,
    "returned_candidates": 2,
    "candidate_limit": 16,
    "closed": 1,
    "progressed": 0,
    "failed": 1,
    "timeout": 0,
    "budget_exceeded": 0,
    "not_attempted": 0,
    "unsupported": 0,
    "output_truncated": 0
  }
}
```

Candidate status is one of `closed`, `progressed`, `failed`, `timeout`, `budget_exceeded`, `not_attempted`, or
`unsupported`. A batch is partial when some rows timed out, exceeded the total output budget, were skipped after that
budget was exhausted, or had truncated output. Warning issues in `errors` suggest a smaller batch, a single-candidate
retry, or a larger `output.max_total_bytes` budget.

`data.result.entry_goals` and `locals` describe the resolved position before candidates run, matching the context tool's
`goals_before` and `locals`. They are rendered once for the batch and omitted when empty or unavailable.

`retry_tainted_non_positive` (default `false`) opts into one server-side retry: when the worker was recycled mid-call
and the batch is non-positive (no candidate `closed` or `progressed`), the server re-issues the attempt once and returns
the retry's rows instead of the suspect ones. If the retry is also tainted and non-positive, the response keeps the
usual execution-taint warning and the retry decision returns to the client. At most one retry per call; it is surfaced
through `telemetry.runtime.retry_count` with full telemetry. With the flag unset the behavior is unchanged: the server
reports the taint and the client decides.

Candidate diagnostics label their coordinate space. Use `synthetic_range` for a trial-buffer location; treat it as an
editable file range only when `original_range` is also present.

An error-severity entry in `diagnostics` signals candidate failure. A `closed` candidate keeps warnings/info there and
in `downstream_diagnostics`; errors produced after closure, such as downstream tactics reporting "no goals", move to
advisory `post_closure_diagnostics`. That field appears only on closed candidates and is omitted when empty.

```json
{
  "id": "candidate_1",
  "status": "failed",
  "diagnostics": {
    "diagnostics": [
      {
        "severity": "error",
        "message": "unknown identifier 'definitely_missing_identifier'",
        "coordinate_space": "synthetic_buffer",
        "position": { "line": 82, "column": 9, "end_line": 82, "end_column": 39 },
        "synthetic_range": { "line": 82, "column": 9, "end_line": 82, "end_column": 39 }
      }
    ]
  }
}
```

### `kind: "command"`

Runs bounded Lean command text as an in-memory trial. Use it for import-context snippets such as `#check` and
`#print axioms`; it is not a replacement for project-wide shell workflows.

Explicit imports name modules from the build of the project or of its Lake dependencies, read in place as `lean` reads
them; build them first with `lean_build`:

```json
{
  "kind": "command",
  "imports": ["Init"],
  "commands": "#check Nat.add\n#print axioms Nat.add_assoc"
}
```

File-derived context prepends the current source snapshot before the command text, so declarations in that file are
visible to later commands:

```json
{
  "kind": "command",
  "file": "LeanRsFixture/ProofActions.lean",
  "commands": "#check LeanRsFixture.ProofActions.closedTheorem"
}
```

Info-level command messages are collected into `output.value`; errors and warnings remain in the bounded diagnostics
block. Invalid command snippets are normal results with diagnostics, not MCP transport failures.

## `lean_verify`

Verifies declarations in memory. Targets can be explicit declaration lists, every declaration in a file, or every
declaration in a module. The server reads Lean source and calls Lean's elaborator/kernel through the worker; it does not
run `lake build`.

For an explicitly selected prepared project, file-backed verification stages the current source and checks it in a
supervised one-shot Lean process against the prepared imports. A compiled declaration lookup can still succeed after an
edit breaks the proof. Use file-backed verification to check that edit.

By-name targets support multi-clause equation definitions and theorems, `where`-structure definitions,
`structure`/`class` commands, and anonymous instances under generated `inst…` names. Discover generated names with
`lean_lookup(kind="declarations")`. Inspect `not_found` suggestions or inventory the file to resolve a missing name.

### Verify One Declaration With Axiom Reporting

```json
{
  "targets": [
    {
      "kind": "explicit",
      "file": "LeanRsFixture/ProofActions.lean",
      "declarations": ["LeanRsFixture.ProofActions.closedTheorem"]
    }
  ],
  "allow_sorry": false,
  "report_axioms": true,
  "detail": "compact"
}
```

Mixed target groups:

```json
{
  "targets": [
    {
      "kind": "explicit",
      "file": "LeanRsFixture/ProofActions.lean",
      "declarations": [
        "LeanRsFixture.ProofActions.closedTheorem",
        "LeanRsFixture.ProofActions.stepTheorem"
      ]
    },
    { "kind": "file_all", "file": "LeanRsFixture/ProofAgent.lean" },
    { "kind": "module_all", "module": "LeanRsFixture.ProofActions" }
  ],
  "allow_sorry": false,
  "report_axioms": false,
  "detail": "compact"
}
```

The `data` payload contains a summary and result rows. For one verified declaration with `report_axioms: false`:

```json
{
  "summary": { "requested": 1, "verified": 1, "failed": 0, "needs_build": 0 },
  "results": [
    {
      "id": "group_1:LeanRsFixture.ProofActions.closedTheorem",
      "file": "LeanRsFixture/ProofActions.lean",
      "declaration": "LeanRsFixture.ProofActions.closedTheorem",
      "verification_status": "verified",
      "facts": {
        "diagnostics": { "diagnostics": [] },
        "unresolved_goals": [],
        "contains_sorry": false,
        "contains_admit": false,
        "contains_sorry_ax": false,
        "axioms": [],
        "axioms_available": false,
        "facts_trustworthy": true
      }
    }
  ]
}
```

`verification_status` uses the same vocabulary as the declaration-verification projection: `verified`, `has_sorry`,
`has_unresolved_goals`, `has_diagnostics`, `not_found`, `ambiguous`, `needs_build`, `timeout`, `budget_exceeded`,
`worker_recycled`, `header_parse_failed`, or `unsupported`. `summary.requested` counts expanded targets before host-side
caps. `summary.truncated` is true when declaration inventory or target expansion was capped. `file_all` and
source-backed `module_all` use the current source snapshot and report the same project-relative file paths in rows. If a
module of the project has no source file, `module_all` reads its declarations from the module's `.ilean` index in the
project's build, with typed artifact freshness facts in `trust`, and a worker imports the module's build to check them.
That check finds only declarations elaborated from source, so their rows read `not_found`; `lean_trial` with
`kind: "command"`, the module in `imports` and `#print axioms` checks them instead. `module_all` reads no index from a
Lake dependency's build. Every source-backed trust fact includes `content_sha256`, the SHA-256 of the exact bytes used
by the call.

`detail` defaults to `"compact"`, which keeps declaration names, files, statuses, bounded diagnostics and goal text,
sorry/admit facts, axiom facts, and trustworthiness flags. It omits target and ambiguous-candidate span blocks. For
ordinary file-backed verification, diagnostics that exceed the compact row budget may be skipped while later diagnostics
are retained. `facts.output_truncated` identifies partial display evidence. Use `"detail": "full"` when you need source
spans in each row.

`retry_tainted_non_positive` (default `false`) opts into one server-side retry per target-group batch: when the worker
was recycled mid-call and the batch comes back non-positive (any row that would be relabeled `worker_recycled`), the
server re-issues that batch once. `verified` rows are never retried — verification is monotone. If the retry is also
tainted and non-positive, the rows are relabeled to `worker_recycled` with the usual warning, exactly as with the flag
unset. At most one retry per batch; it is surfaced through `telemetry.runtime.retry_count` with full telemetry.

A `not_found` row for an explicit target may carry declarations with matching short names across namespaces and case in
`facts.candidates` (with `"detail": "full"`) and a warning. Mapping a Git diff to explicit target groups is the caller's
job; the server returns no diff-coverage object.

Check `facts_trustworthy` before interpreting absence of sorry or unresolved goals. An empty `axioms` list establishes
that no dependencies were found only when `axioms_available` is true and `axioms_truncated` is false. `axioms_available`
is false when reporting was not requested or the axiom set could not be computed.

## `lean_lookup`

Artifact-backed declaration inventory and reference queries read the project's build under the request's read turn, so
no build of this server runs while they read. Source and index freshness remain separate trust facts.

### `kind: "declaration"`

Inspects one declaration by name. Use `file` when local imports or namespace context are needed; use `imports` for
explicit import context.

```json
{
  "kind": "declaration",
  "name": "Nat.add_zero",
  "imports": ["Init"]
}
```

Optional `fields` can select `source`, `statement`, `docstring`, `attributes`, and `flags`; `raw_statement` asks for the
raw elaborated term.

### `kind: "declarations"`

Lists declarations in one source file or module. File targets use the current source snapshot and the worker's
declaration outline, supplementing missing names from a fresh matching `.ilean` when available:

```json
{
  "kind": "declarations",
  "target": { "kind": "file", "path": "LeanRsFixture/ProofAgent.lean" },
  "limit": 200
}
```

Module targets first resolve `<module>.lean` under the project root and use the same source-fresh path when the file
exists:

```json
{
  "kind": "declarations",
  "target": { "kind": "module", "module": "LeanRsFixture.ProofAgent" },
  "limit": 200
}
```

If module source is unavailable, the mode falls back to a matching `.ilean` and reports its build-freshness facts in
`trust`. Index rows have declaration and name/selection ranges but omit `kind` and `body_span`. A missing file target
returns `not_found`; an unavailable module index returns `missing_build`. `limit` defaults to 200 and is capped at 1000;
truncation keeps a deterministic prefix and sets `truncated: true`.

### `kind: "proof_search"`

Returns ranked declarations relevant to a proof goal. The target can come from a file/declaration position:

```json
{
  "kind": "proof_search",
  "file": "LeanRsFixture/ProofAgent.lean",
  "declaration": "LeanRsFixture.ProofAgent.miniRatDenominatorStep",
  "mode": "next_step",
  "limit": 10
}
```

or from explicit goal/type text:

```json
{
  "kind": "proof_search",
  "goal": "⊢ True",
  "imports": ["LeanRsFixture.SourceRanges"],
  "mode": "exact"
}
```

Modes are `next_step`, `exact`, `apply`, `rewrite`, and `simp`. `limit` is clamped to the tool cap.

### `kind: "references"`

Finds semantic references to a fully-qualified Lean name.

File scope elaborates one anchor file through the worker, so it reflects the current source snapshot and carries a
worker session id plus a `source` / `file` / `edit_fresh` trust fact:

```json
{
  "kind": "references",
  "name": "LeanRsFixture.ProofActions.closedTheorem",
  "scope": "file",
  "file": "LeanRsFixture/ProofActions.lean",
  "limit": 20
}
```

Project scope reads the on-disk `.ilean` reference index without opening a worker (see
[reader architecture](architecture.md#reference-indices)). `files` restricts where references occur; omit it to scan all
project indices. Results reflect the last build, with stale contributing sources reported in `trust` and warnings:

```json
{
  "kind": "references",
  "name": "LeanRsFixture.ProofSearchFacts.MiniRat",
  "scope": "project",
  "files": ["LeanRsFixture/ProofSearchFacts.lean"],
  "limit": 100
}
```

## `lean_status`

### `kind: "project"`

Returns cheap project, toolchain, output, and broker configuration. This mode uses Lake metadata only and does not open
a worker.

```json
{ "kind": "project" }
```

Use `project` to override the default Lake root:

```json
{ "kind": "project", "project": "/abs/path/to/lake/project" }
```

Use `include` to request cheap status sections. The default is all sections:

```json
{
  "kind": "project",
  "project": "/abs/path/to/lake/project",
  "include": ["toolchain", "worker", "artifacts"]
}
```

`lean_status` reads Lake metadata and cheap filesystem facts only: it does not run `lake`, does not read source files,
and does not open a worker. When the project build tree is absent it reports `olean` and `ilean` `missing_build` facts;
when the build tree exists but no semantic query has checked source mtimes it reports artifact freshness as `unknown`.
Worker runtime generation is likewise `not_applicable` because this status mode deliberately avoids opening a worker.
The `worker` section's `compatibility` holds the receipts that resident workers kept from their last completed calls;
see [native compatibility](#native-compatibility).

### `kind: "file_diagnostics"`

Checks the file as `lake build` does: it runs `lean --json` from the project root with the module setup that
`lake --no-build setup-file` prints, and returns each message Lean prints, at Lean's position, in the bounded
diagnostics block used by proof context. Lean sees the whole file, header included, and skips messages marked silent, as
in a build. It reads the text the server read and hashed, on stdin under the file's own name, so an edit during the
check does not change what it checked. It writes no build output and does not run `lake exe lint` or other project lint
commands.

```json
{
  "kind": "file_diagnostics",
  "file": "LeanRsFixture/ProofActions.lean"
}
```

The result includes `diagnostics` and the header `imports`, with a source `edit_fresh` trust fact for the file snapshot.

The call fails and returns no diagnostics if the file changes while Lake prints its setup, if `lean` crashes or runs out
of time, or if its exit code disagrees with its messages.
