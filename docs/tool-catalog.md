# Tool catalog

This file gives each tool's request and reply contract. `tools/list` gives the schemas; this file says what the fields
mean.

| Tool | Does | Modes |
| --- | --- | --- |
| `lean_context` | Proof state at a position in a declaration | `kind`: `proof_position` |
| `lean_trial` | Tries tactics or commands in memory; writes no file | `kind`: `proof_step`, `command` |
| `lean_verify` | Checks declarations, files or modules, with a `sorry` policy and axiom report | target groups |
| `lean_lookup` | Finds declarations, outlines, proof-search hits and references | `kind`: `declaration`, `declarations`, `proof_search`, `references` |
| `lean_status` | Project status and file diagnostics | `kind`: `project`, `file_diagnostics` |
| `lean_build` | Runs Lake in the project | targets or a recipe |
| `lean_imports` | Lists imports as Lake reports them | `files`, `modules` |
| `lean_run`, `lean_get` | Start a named request; read its row and result |  |
| `lean_runtime` | A snapshot of the worker pool |  |

A failure with no domain data sets MCP `isError: true` and keeps the `errors` and `trust` of the reply. A failed proof,
a rejected declaration or a `needs_build` verdict is a completed call with data, not an MCP error.

When a request carries `_meta.progressToken`, every tool sends a start notification, a heartbeat every 15 seconds and a
completion notification on `notifications/progress`, so a client that extends its deadline while progress arrives does
not give up on a long import. The server offers no MCP Tasks; name a call with `request_id` to keep its result past a
dropped connection or a restart ([named requests](#named-requests)).

## Common Workflows

### Proof workflow

1. `lean_context` with `kind: "proof_position"` reads the goals, locals and diagnostics at a position.
2. `lean_lookup` with `kind: "proof_search"` ranks declarations for the goal, and `kind: "declaration"` shows one.
3. `lean_trial` with `kind: "proof_step"` tries one or more tactics in memory. Its batch carries the position's
   `entry_goals` and `locals`, so each trial gives the context for the next.
4. `lean_verify` with an `explicit` target group checks the declaration.

Use `lean_lookup` with `kind: "references"` to find uses of a name, `lean_status` for cheap status before a worker
starts, and `lean_build` when a reply says `needs_build`.

## Response shape

The five semantic tools reply with:

```jsonc
{
  "data": { "...": "the mode's result" },
  "errors": [],
  "trust": {
    "project_root": "/abs/project", "session_id": "short-run-…", "lean_toolchain": "leanprover/lean4:v4.36.0",
    "source_sha256": "…",
    "artifacts": [{ "artifact": "source", "scope": "file", "status": "edit_fresh", "path": "Demo.lean", "content_sha256": "…" }]
  }
}
```

`trust.session_id` names what answered: `request-invalid` when the server refused the request before any work,
`metadata-only` when nothing elaborated, `short-run-<uuid>` when one `lean` process did, and the worker's session id
when a worker did.

`trust.artifacts` is left out when empty. Its rows use these tokens:

- `artifact`: `source`, `olean`, `ilean`, `worker`
- `scope`: `file`, `module`, `project`, `toolchain`
- `status`: `edit_fresh`, `build_fresh`, `stale_build`, `missing_build`, `unknown_import`, `unknown`, `not_applicable`

A row may also carry `path`, `module`, `detail`, `next_action` and a source's `content_sha256`. A path inside the Lake
root is relative to it; any other path is absolute. Repeated rows collapse, and the first order stays.

Every reply whose result came from a file's text, a `needs_build` reply included, has a `source` / `file` / `edit_fresh`
row whose `content_sha256` is the lowercase SHA-256 of the exact bytes the call read and checked. When the call checked
one file, `trust.source_sha256` repeats it: compare it with the text you meant to check before you trust the result. A
call that read several texts, or read one file twice and saw an edit between, has one row per text and no
`trust.source_sha256`. `lean_lookup` with `kind: "declaration"` reads only a file's header to choose imports, so it has
no source row.

A check builds nothing. When an import is missing or stale, the reply is `needs_build` with a `missing_build` or
`stale_build` row for each module Lake names, whose `next_action` names the call that builds it, such as
`run lean_build(targets=["+Chain.B"]), then retry`, or `run lean_build() to build the default targets, then retry` when
no module is known. An import that names no module, or whose source file is missing, gets an `olean` / `module` /
`unknown_import` row whose `next_action` says to fix the import. No reply tells you to run `lake`. An import outside
every Lake library still fails in Lean: `lean_status` `file_diagnostics` shows Lean's `unknown module prefix` error, and
`lean_verify` `file_all` warns that the header could not be parsed.

Runtime failures appear in `errors` with `code`, `details` and a `retryable` flag; follow the flag, since some refusals
need an install, not a retry. While abnormal restarts keep the worker down, `details.retry_after_millis` says how long
to wait before the retry. Warnings appear there too, with `severity: "warning"` and an optional `next_action`.

Flags named `*_truncated`, `partial`, and advisory lists such as `post_closure_diagnostics`, `entry_goals` and `locals`
are left out when false or empty; an absent flag means complete. Verdict and trust facts, `contains_sorry`,
`contains_admit`, `contains_sorry_ax`, `axioms_available`, `facts_trustworthy` and attribute flags, always appear,
`false` included.

When a worker call's CPU time passes `output.slow_call_warning_millis`, a warning names it and suggests smaller
declarations, type annotations or extracted lemmas. It counts CPU, not wall time, so a busy machine never sets it off.
With `telemetry.verbosity = "full"`, `telemetry.runtime` carries `queue_wait_millis`, `call_elapsed_millis` and
`call_cpu_millis`.

## Output Carrier

The reply goes in MCP `content` text by default, which model clients read best. A program that wants `structuredContent`
starts the server with `LEAN_HOST_MCP_RESPONSE_CARRIER=structured`, or with `both` for the same JSON in both fields.

## Request synonyms

The server maps common synonyms, such as `tactic` for `proof_step`, `outline` for `declarations` or a bare string for a
proof position or a `lean_verify` group, to the canonical request before it decodes it, and replies use the canonical
names; `tools/request_aliases.rs` lists them. A `kind` that names another tool's job, such as `search` or `verify`,
fails with an error that names the tool and mode to call.

## Tagged Request Shapes

Tagged arguments carry their tag in a string field named `kind`.

`DeclarationInventoryTarget`, for `lean_lookup(kind="declarations")`:

```json
{ "kind": "file", "path": "LeanRsFixture/ProofAgent.lean" }
{ "kind": "module", "module": "LeanRsFixture.ProofAgent" }
```

`ProofPositionSelector`, for `lean_context`, `lean_trial(kind="proof_step")` and `lean_lookup(kind="proof_search")`:

```json
{ "kind": "default" }
{ "kind": "index", "index": 0 }
{ "kind": "after_text", "text": "skip", "occurrence": 0 }
```

`default`, or no selector, is the entry goal, before any tactic runs; a `proof_step` snippet goes before the first
tactic there. `index: N` is the state after tactic N, counting from 0. `after_text` picks a position the worker
recognizes after a tactic that matches the text; not every substring is one.

## `lean_context`

`kind: "proof_position"` returns the proof context at one position:

```json
{"kind":"proof_position","file":"LeanRsFixture/ProofActions.lean","declaration":"LeanRsFixture.ProofActions.stepTheorem","proof_position":{"kind":"default"}}
```

`data.status` is `context`, `header_parse_failed` when the file's header does not parse, or `unsupported` when the
worker lacks the module query capability. A `context` reply holds the diagnostics, goals, locals, truncation, and any
`needs_build` or ambiguity facts. `include_boundaries: true` adds `proof_boundaries`, each with its `index`, `kind`,
`selector`, `source` range and `excerpt`; ask for it once per declaration to pick a selector.
`include_expected_type: true` adds the goal's `expected_type`. A selector that matches no position is still a normal
reply: `unavailable` holds `proof_state` with the message "declaration has no proof position matching the selector", and
`proof_boundaries`, when asked for, lists the selectors that work. With `telemetry.verbosity = "full"`, `query_facts`
gives the worker's cache status, output bytes and phase times.

## `lean_trial`

### `kind: "proof_step"`

Tries one `snippet`, or a list of `snippets`, at a position, each on its own in request order, against an in-memory copy
of the source:

```json
{"kind":"proof_step","file":"LeanRsFixture/ProofActions.lean","declaration":"LeanRsFixture.ProofActions.stepTheorem","snippets":["simp","exact h"]}
```

The batch is at `data.result`, with at most 16 `candidates`, `candidate_limit`, `candidates_truncated` and a `summary`
that counts each status. A candidate's status is `closed`, `progressed`, `failed`, `timeout`, `budget_exceeded`,
`not_attempted` or `unsupported`. A batch is partial when some rows timed out, ran past the output budget, were skipped
after it ran out, or had truncated output; warnings then suggest a smaller batch or one candidate. `entry_goals` and
`locals` describe the position before any candidate runs.

An error in a candidate's `diagnostics` means it failed. A `closed` candidate keeps warnings there and in
`downstream_diagnostics`, and moves errors that come after closure, such as "no goals", to `post_closure_diagnostics`.
Each diagnostic names its `coordinate_space`: `synthetic_range` is a place in the trial buffer, and only an
`original_range` is a place in the file.

`retry_tainted_non_positive: true` asks for one retry when a worker restart that nobody planned left the call unfinished
and no candidate closed or progressed; the reply then holds the retry's rows. If the retry is also tainted, the reply
keeps the taint warning and the choice returns to you.

### `kind: "command"`

Runs Lean command text, such as `#check` or `#print axioms`, in memory. `imports` names modules from the project's build
or its Lake dependencies, which must be built first; `file` puts the current text of that file before the commands, so
its declarations are in scope.

```json
{"kind":"command","imports":["Init"],"commands":"#check Nat.add\n#print axioms Nat.add_assoc"}
```

Info messages go to `output.value`; errors and warnings stay in the diagnostics. A command that fails to elaborate is a
normal reply with diagnostics.

## `lean_verify`

Checks declarations in memory through Lean's elaborator and kernel; it never runs `lake build`. A target group is
`explicit` (a file and its declarations), `file_all` (every declaration in a file) or `module_all` (every declaration in
a module). A file-backed check runs the current text in one `lean` process against the built imports, so it catches an
edit that a lookup of the compiled declaration would miss. By-name targets cover equation definitions, `where`
structures, `structure` and `class` commands and anonymous instances under their generated `inst…` names; list those
with `lean_lookup(kind="declarations")`.

### Verify One Declaration With Axiom Reporting

```json
{"targets":[{"kind":"explicit","file":"LeanRsFixture/ProofActions.lean","declarations":["LeanRsFixture.ProofActions.closedTheorem"]}],"allow_sorry":false,"report_axioms":true,"detail":"compact"}
```

`data` holds a `summary` (`requested`, `verified`, `failed`, `needs_build`, and `truncated` when expansion was capped)
and one row per declaration:

```json
{"id":"group_1:LeanRsFixture.ProofActions.closedTheorem","file":"LeanRsFixture/ProofActions.lean","declaration":"LeanRsFixture.ProofActions.closedTheorem","verification_status":"verified","facts":{"diagnostics":{"diagnostics":[]},"unresolved_goals":[],"contains_sorry":false,"contains_admit":false,"contains_sorry_ax":false,"axioms":["propext"],"axioms_available":true,"facts_trustworthy":true}}
```

- `verification_status` is `verified`, `has_sorry`, `has_unresolved_goals`, `has_diagnostics`, `not_found`, `ambiguous`,
  `needs_build`, `timeout`, `budget_exceeded`, `worker_recycled`, `header_parse_failed` or `unsupported`.
- Read `facts_trustworthy` before you read the absence of `sorry` or goals. An empty `axioms` list means no axioms only
  when `axioms_available` is true and `axioms_truncated` is absent.
- `detail: "compact"`, the default, keeps names, files, statuses, bounded diagnostics and goals, and the facts, and may
  skip diagnostics past the row's budget; `facts.output_truncated` marks that. `detail: "full"` adds source spans and,
  for a `not_found` row, `facts.candidates` with similar short names.
- `module_all` checks the source Lake names for the module, under its library's `srcDir`. When Lake names none, as for a
  deleted file, it reads the declarations from the module's `.ilean` with a `stale_build` fact, names the file
  `<lean_verify module M>`, and its rows read `not_found`; check such a module with `lean_trial` `command`,
  `#print axioms` and the module in `imports`.
- `retry_tainted_non_positive: true` retries a batch once when a worker restart that nobody planned left it unfinished
  and some row would read `worker_recycled`; `verified` rows never retry.
- For one `explicit` file group, a complete verdict carries `trust.cli_file_verification`, schema
  `lean-host-cli-file-verification-v1`. It names the `server_instance_id`, `project_root`, `source_file`,
  `lean_toolchain`, `lean_executable` and the `runtime_lean_version` the Lean process reported, and holds lowercase
  SHA-256 digests of the source (`source_sha256`), the module setup (`module_setup_sha256`), the checked inputs and
  shims (`artifact_identity_sha256`), the driver request (`request_sha256`) and the returned `data` (`data_sha256`, over
  compact UTF-8 JSON with sorted keys). Check the names you asked for and the source digest before you accept a verdict.

Mapping a Git diff to target groups is the caller's job.

## `lean_lookup`

Lookups that read the project's build hold the project's read turn, so no build runs while they read.

- `kind: "declaration"` shows one declaration by `name`, in the context of a `file` or of explicit `imports`. `fields`
  picks `source`, `statement`, `docstring`, `attributes` and `flags`; `raw_statement` asks for the elaborated term.
- `kind: "declarations"` lists the declarations in a file or module from its current text, adding names from a matching
  `.ilean` when it is newer than the source. A module's source is the one Lake names. When Lake names none, the list
  comes from the module's `.ilean`, without `kind` or `body_span`, and its trust row reads `stale_build` with no
  `next_action`, since no build refreshes the index of a module that is gone. A missing file, or a module with neither
  source nor index, is `not_found`. `limit` defaults to 200, at most 1,000.
- `kind: "proof_search"` ranks declarations for a goal, from a position in a file or from goal text with `imports`.
  `mode` is `next_step`, `exact`, `apply`, `rewrite` or `simp`; `limit` defaults to 10, at most 20. Candidates whose
  name matched a search exactly come first, then semantic search's hits in its rank order, then the other declaration
  search rows; each declaration appears once, and its `match_reason` lists all the evidence. No score is given.
- `kind: "references"` finds uses of a full name. `scope: "file"` elaborates one `file` in a worker, so it reflects the
  current text. `scope: "project"` reads the `.ilean` indexes of the last build without a worker, then asks Lake for the
  source of each module with hits; `files` keeps only hits in those files. The `ilean` / `project` row reads `unknown`,
  since the server does not compare indexes with sources: an edit shows only after `lean_build`. A module Lake names no
  source for adds no hits, and a warning names it.

```json
{"kind":"declaration","name":"Nat.add_zero","imports":["Init"]}
{"kind":"declarations","target":{"kind":"file","path":"LeanRsFixture/ProofAgent.lean"},"limit":200}
{"kind":"proof_search","file":"LeanRsFixture/ProofAgent.lean","declaration":"LeanRsFixture.ProofAgent.miniRatDenominatorStep","mode":"next_step"}
{"kind":"proof_search","goal":"⊢ True","imports":["LeanRsFixture.SourceRanges"],"mode":"exact"}
{"kind":"references","name":"LeanRsFixture.ProofSearchFacts.MiniRat","scope":"project","files":["LeanRsFixture/ProofSearchFacts.lean"]}
```

## `lean_status`

`kind: "project"` reports the project, its toolchain, its build and the server's settings. It runs no Lake, reads no
source and opens no worker. The `olean` and `ilean` rows look at the root package's import directory among the import
roots Lake last named: they read `missing_build` when it is absent and `unknown` otherwise. They read `unknown` with a
note to run a check first when Lake has named no roots since the server started or the Lake configuration changed. The
worker generation reads `not_applicable`. `include` picks `toolchain`, `worker` and `artifacts`; the default is all.

With `worker`, `data.worker.compatibility` holds the broker instance, a contract digest, the native API, lifecycle and
binding contract it expects, and one receipt for each resident worker that has completed a call on its current
generation. It is absent when no worker has one. A receipt records what the worker reported; it never shows that a call
ran, that a proof holds, that a worker exited, or that no earlier IO happened. Check replies carry no receipt.
`data.worker.restart_retry_after_millis` appears while abnormal restarts keep the project's worker down, and says how
long they will.

`kind: "file_diagnostics"` checks a `file` as `lake build` does: it runs `lean --json` from the project root with the
setup `lake setup-file` prints, on the text the server read and hashed, and returns each message at Lean's position.
Lean sees the whole file, header included, and drops messages marked silent. It writes no build output and runs no
project lint. The reply holds `diagnostics` and the header's `imports`. The call fails with no diagnostics when the file
changes while Lake prints its setup, when `lean` crashes or runs out of time, or when its exit code disagrees with its
messages.

## `lean_build`

Runs Lake in the project root, with Lake `targets` or a config `recipe` with optional `args`. The two exclude each
other; with neither, Lake builds its default targets. [Operations](operations.md#builds) gives the config.

```json
{"name":"lean_build","arguments":{"targets":["Chain.C"]}}
{"name":"lean_build","arguments":{"recipe":"format","args":["MyProject/Foo.lean"]}}
```

The build waits for running checks, and later checks wait for it. It stops the project's workers, so the next check
starts a fresh worker on what it wrote. When `[build] guard` names targets, the build first runs `lake build --no-build`
on them, or where the recipe's `guard` puts it, so a build that would rebuild a guarded target stops instead. The first
failing step ends the build.

```json
{"state":"done","ok":false,"exit_code":1,"failed_step":1,"steps":[{"argv":["lake","build","--no-build","@dep/+Dep:olean"],"exit_code":0,"started_at_ms":1764150000000,"finished_at_ms":1764150000420},{"argv":["lake","build","Chain.C"],"exit_code":1,"started_at_ms":1764150000421,"finished_at_ms":1764150003870}],"log":{"path":"/abs/project/.local/lean-host-mcp/store/logs/3f0c….log","bytes":5120,"truncated":false},"tail":"error: ./././Chain/C.lean:3:2: type mismatch …"}
```

- `state` is `done` when every step that should run ran, and `timed_out` when `[build] timeout_secs`, counted from the
  request, stopped the build.
- `ok` is true when every step ran and passed; when it is false, the reply sets `isError`.
- `exit_code` is the first failing step's code, `0` when all passed, and null when the build was stopped; `failed_step`
  is that step's index. A step's `exit_code` is Lake's, or 128 plus the signal that ended it.
- `log` names the whole log, in the store. Output past `[build] log_limit_mib` is dropped and `truncated` is true.
  `tail` holds the log's last 8 KiB, or less when `output.max_field_bytes` is smaller.

A request the config does not allow, such as an unknown recipe or a word that starts with `-`, fails with MCP invalid
params before anything runs.

## `lean_imports`

Reports imports as Lake reads them, under the project's read turn, and builds nothing. Name `files`, `modules` or both.

```json
{"name":"lean_imports","arguments":{"files":["Chain/C.lean"],"modules":["Chain.B","Init"]}}
```

For each of up to 16 `files`, the reply lists the whole import closure from `lake setup-file`, not only the direct
imports: each `module`, the `olean` Lake loads, and, for a module whose `.olean` lies in the root package's own import
directory, the last one `lake env` puts on `LEAN_PATH`, the `source` Lake names under its library's `srcDir`; for a
dependency's module, `source` is null. When an import is missing or stale the call fails as a whole, with `isError` and
code `setup_file_failed`, naming the file and quoting Lake; the code is `unknown_import` when an import names no module,
`invalid_request` for a file the server cannot read, and `module_sources_failed` when Lake names no import directories
or cannot answer the source query.

For each of up to 128 `modules`, in request order, the reply gives the `source` Lake names and the direct `imports` its
header lists, each with `all`, `public` and `meta` flags. Outside the module system every import is public. Lean adds
`Init` to each file that does not start with `prelude`, once plain and once as `meta`. A module Lake cannot answer for
gets its own `error`, and the rest still answer: `unknown_module` when no workspace package holds it, as for `Init`, or
its library has no source for it, and `module_header_failed` when Lake could not read the header. When Lake answers for
none, the call fails with `module_inputs_failed`.

```json
{"module":"Init","source":null,"imports":[],"error":{"code":"unknown_module","message":"Init: error: unknown module `Init`"}}
```

## Named requests

A call with `request_id` in its arguments is a named request: the server records it before anything runs, runs it at
most once, and keeps its result. An id matches `^[A-Za-z0-9._:-]{8,128}$`; ids that start with `srv-` belong to the
server. Every tool takes `request_id` except `lean_run`, `lean_get` and `lean_runtime`.

```json
{"name":"lean_verify","arguments":{"request_id":"verify-7c1d2e3f","targets":[{"kind":"file_all","file":"Chain/C.lean"}]}}
{"name":"lean_run","arguments":{"request_id":"build-7c1d2e3f","tool":"lean_build","arguments":{"targets":["Chain.C"]}}}
{"name":"lean_get","arguments":{"request_id":"build-7c1d2e3f","wait_seconds":60}}
```

- A tool called with `request_id` runs the request and returns its result. If the connection drops, the wait ends and
  the request goes on; the same call again returns the stored result.
- `lean_run {request_id, tool, arguments}` records and starts the request and replies at once with `{request: row}`. The
  inner `arguments` must not hold `request_id`.
- `lean_get {request_id, wait_seconds?}` returns `{request: row, result}`. `result` is null until the request ends, then
  holds what a direct call would have returned; it stays null for a request that a server start ended. `wait_seconds`,
  default 0, waits up to that long, at most `requests.wait_max_secs`.

The row is `{request_id, tool, kind, state, created_at, deadline_at, started_at?, finished_at?, exit_code?, log?}`.
`kind` is `build` for `lean_build` and `check` for the rest; times are milliseconds since the Unix epoch; `log` names a
build's log. `state` is one of:

- `queued`: recorded, and waiting for its turn on the project;
- `running`: it holds its turn and its work has started;
- `done`: the tool returned; the result may be a tool error or a negative verdict;
- `timed_out`: the deadline passed. A check says so only once its worker request is seen to stop; when no one can see
  the stop, the row says `done`;
- `interrupted`: a server stopped before the request returned, and the next start ended it. It never runs again.

The intent of a request is the SHA-256 of the JSON of `{tool, arguments}` without `request_id`, with no spaces and keys
sorted at every level. The same id with the same intent returns the stored row, from any client and after a restart;
another intent fails with `request_id_conflict`. Ids are global, so a reused id returns the old result.

Nothing cancels a named request: it ends when its tool returns, at its deadline, or at the next server start. The row
fixes the deadline when it is written: `runtime.request_timeout_millis` for a check, `[build] timeout_secs` or the
recipe's `timeout_secs` for a build, with the wait for the turn counted. Deadlines use wall time, which counts a
laptop's sleep. At start a server marks each `running` row an earlier server left `interrupted`, and starts each
`queued` row in arrival order unless its deadline has passed or its project is gone.

A call without `request_id` gets an id `srv-<uuid>` and no row; the server cancels it when its connection drops or on
MCP `notifications/cancelled`, and a crash loses it.

Every reply carries `request_id` at the top of its `structuredContent` and of its first text item when that is JSON, and
every JSON-RPC error carries it in `error.data`. A refusal makes no row. It is a tool error, `isError: true`, shaped
`{error: {code, message}, request_id, request?}`, where `request` is the row the id already holds:

| Code | Meaning |
| --- | --- |
| `invalid_request_id` | The id breaks the rule above. |
| `invalid_arguments` | The tool refuses the arguments, or the `lean_run` arguments hold `request_id`. |
| `unknown_tool` | `lean_run` names a tool this server lacks, or `lean_run`, `lean_get` or `lean_runtime`. |
| `request_id_conflict` | The id holds another call; `request` is its row. |
| `request_not_found` | `lean_get` names an id with no row. |
| `request_timed_out`, `request_interrupted` | A direct call with this id ended without a result; `request` is its row. |
| `requests_unavailable` | The server has no store. Start it with `--lake-root`. |
| `request_store_failed` | The table failed. `retryable` is true: ask again after `retry_after_millis`. |

The semantic tools decode most of their arguments only when they run, so a bad argument to one of them makes a row whose
result is the tool's error. A stored JSON-RPC error comes back as a tool error of the same shape, whose code is the
error's `reason`, `invalid_arguments` or `internal_error`.

## `lean_runtime`

Takes no arguments, replies at once, and never runs as a named request. It reads the server's own state, opens no
worker, and changes nothing. A snapshot is advisory: another request can start right after it.

- `build`: the package `version`, the full Git `revision` and the build-time `dirty` state, or null when the build had
  no Git metadata. A revision names the source, not the dependency graph or the binary.
- `effective_settings`: the resolved broker and runtime values, derived heap and residue limits included, and the
  presentation and output overrides; a null override means each tool keeps its own budget. It shows no raw environment.
- `requests: {queued, running}`: named requests that have not ended, and the capability `durable_requests_v1`; both are
  absent on a server without a store.
- `pool`: `instance_id`, which changes when the server restarts; `max_workers` and `max_workers_per_project`;
  `occupied_slots`, which counts a worker that is starting, resident, retiring or whose exit is not confirmed;
  `outstanding_jobs`, which counts admitted requests, startups, preparation and maintenance until their cleanup ends;
  `workers_stopped`, true only once every slot and cleanup is released; `acquiring_projects`; and `projects`, each
  resident worker with `idle`, `outstanding_jobs`, `admitted_caller`, `healthy` and its last `runtime` facts.
