# Tool Catalog

Five semantic tools cover proof context, trials, lookup, verification, and status. `lean_context`, `lean_trial`,
`lean_lookup`, and `lean_status` select a mode with `kind`; `lean_verify` takes target groups directly. Operational
coordination uses `lean_compatibility`, `lean_request`, and `lean_runtime`.

- Semantic tools: [context](#lean_context), [trial](#lean_trial), [verification](#lean_verify), [lookup](#lean_lookup),
  [status](#lean_status).
- Shared contracts: [response shape](#response-shape), [output carrier](#output-carrier),
  [request synonyms](#request-synonyms), [proof workflow](#proof-workflow).
- Coordination: [native compatibility](#native-compatibility), [retained requests](#prepared-retained-lifecycle-v3),
  [worker activity and admission leases](#worker-activity-and-admission-leases).

```text
lean_context -> lean_lookup -> lean_trial -> lean_verify
                         \-> lean_status
```

The semantic tools read files and elaborate in memory without applying source edits automatically. Lean commands,
tactics, and import initializers can perform IO with the server user's privileges; the worker is not an OS sandbox. Only
use trusted local clients and projects. `lean_runtime` may pause admission or explicitly retire idle workers.

Invocation/runtime failures with no domain data set MCP `isError: true` and retain the semantic `errors` and `trust`
envelope. Negative proof/check verdicts remain successful tool executions with domain data, not MCP errors.

## Native compatibility

`lean_compatibility` is a synchronous operational handshake:

```json
{"name":"lean_compatibility","arguments":{"project":"/absolute/project"}}
```

The broker admits and opens a real host session under its ordinary worker capacity. The handshake executes no target
proof. It returns `status: "ok"` and one `native_compatibility` object containing the broker instance, a contract
digest, the expected native API/lifecycle/binding contract, and actual actor/generation-bound loaded receipts. The same
object carries `shared_writer: {protocol: 1, executable: ...}` from the server's current executable; its required
protocol is part of the expected contract while the executable is an instance-bound observation. Every writer client
negotiates that executable's JSON protocol before launching a write. Worker package versions are observations;
compatibility is not tied to an executable hash or source checkout revision. Compiled Lean version/header facts are
separate from `runtime_lean_version`, which comes through the loaded version shim.

The broker binds its required host ABI subset for its lifetime and checks that subset on every later host session,
including after internal replacement. Extra compatible loaded operations are allowed. A missing or incompatible runtime
is refused before the caller submits its proof. Compatibility never certifies execution, a proof verdict, worker exit,
absence of prior IO, or readiness of arbitrary first-party artifacts.

Save the contract and instance with the prepared request. Semantic responses carry the actual call's receipt at
`trust.native_compatibility`, even with quiet telemetry. Validate it against the saved contract before accepting a
result; a later broker instance does not rewrite the old request's identity. Retained recovery uses the original ticket
rather than submitting again. `lean_runtime(action=status)` remains a non-mutating advisory snapshot and never opens a
worker.

`lean_compatibility({"project":"/canonical/project","mode":"pure_v1"})` returns compiled expectations and helper
configuration without native admission or loaded receipts. Default `mode: "loaded"` retains the manual loaded handshake.
A client requiring pure discovery must not remove the mode or fall back to loaded discovery after an error.

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

Clients declaring the `io.modelcontextprotocol/tasks` extension may receive a standard task handle from `tools/call`.
Use `tasks/get` with its `taskId` to retrieve status and the eventual original tool result; use `tasks/cancel` to
request cancellation. Cheap project status remains synchronous. Existing clients keep synchronous tool responses. The
HTTP server shares task records across client sessions, so reconnecting clients can retrieve their existing handles. A
task continues while its client is disconnected; reconnect with the existing handle rather than submitting it again.

For one explicit named file group on the ordinary setup-aware `lean_verify` route, a complete verdict includes
`trust.cli_file_verification` with schema `lean-host-cli-file-verification-v1`. It identifies the
`server_instance_id`, canonical `project_root` and `source_file`, selected `lean_toolchain`,
`lean_executable`, and `runtime_lean_version` reported by the executing Lean CLI. The runtime version must
match the selection before that process opens the staged setup.

The receipt includes lowercase SHA-256 strings `source_sha256`, `module_setup_sha256`, `artifact_identity_sha256`,
`request_sha256`, and `data_sha256`. These bind the exact source bytes, staged ModuleSetup bytes, checked input
identities and installed shim digests under read custody, the staged driver request bytes, and the returned `data`
respectively. The `data` digest uses compact UTF-8 JSON with sorted object keys and unchanged array order. A caller
must check the exact requested names and compare the source digest against its saved input before accepting a verdict.
The receipt establishes CLI provenance. Loaded-worker ABI evidence continues to use `trust.native_compatibility`.

### Prepared retained lifecycle (v3)

Clients without the Tasks extension use `lean_request`. Servers advertise `retained_request_lifecycle_v3` in
`lean_runtime`. Prepare new work first:

```json
{"name":"lean_request","arguments":{"action":"prepare","tool":"lean_verify","arguments":{"targets":[]}}}
```

`prepare` validates the tool and arguments, reserves one retained-record slot and returns a server-issued `taskId` and
immutable `request` binding. It does not invoke the tool, open a worker, or evaluate Lake configuration. The ticket is
valid for five minutes. Its `execution` remains `pending` with reason `prepared`, because it can still be submitted.
Preparing does not reserve a worker lane. `prepare` rejects client-supplied IDs, epochs and digests.

Save the returned ticket and binding before submitting the exact input:

```json
{"name":"lean_request","arguments":{"action":"submit","request_id":"ticket-from-prepare","request_epoch":"epoch-from-ticket","tool":"lean_verify","arguments":{"targets":[]}}}
{"name":"lean_request","arguments":{"action":"get","request_id":"ticket-from-prepare","request_epoch":"epoch-from-ticket"}}
{"name":"lean_request","arguments":{"action":"cancel","request_id":"ticket-from-prepare","request_epoch":"epoch-from-ticket"}}
```

`submit` requires an existing prepared ticket. It never creates work for a missing ID. An exact retry of an accepted,
running or retained terminal ticket returns its original record without executing again. Changed inputs are rejected.
Malformed submit retries report `unknown` for the referenced ticket; a rejected retry cannot certify that previously
accepted work never started. Pure invalid preparation or submission without a ticket can report typed `not_started`. Use
the ticket's original epoch and origin instance on every recovery; status observations are advisory. Expiry,
acknowledgment and unrelated requests do not rotate the live broker's epoch or invalidate another prepared ticket.

The immutable `request` binding contains `server_instance_id`, `request_epoch`, `tool`, `submission_sha256`, and
`project`. The digest is lowercase SHA-256 of the compact JSON array `[tool, arguments]`, with object keys sorted by the
server's JSON serializer. `project` is the exact submitted string or null, not a source-revision claim. The origin
instance and epoch remain unchanged after recovery by a restarted broker. The prepare receipt is authoritative even if
an earlier status observation described a previous broker instance. Native Tasks records have no such binding.

Persist the logical request before preparing, the returned ticket before submitting, and submit intent before sending. A
lost prepare response can be followed by another prepare because the unobserved ticket was never submitted. After
submission might have occurred, recover only the same ticket and input. A missing or forgotten ticket is `unknown` and
never authorizes a replacement submission. Earlier v2 records keep their immutable identities for retrieval,
cancellation, acknowledgment and exact retained retries; fresh client-chosen IDs are no longer admitted.

`get` returns task metadata plus the original `CallToolResult` on completion or a JSON-RPC error on failure. A completed
task can contain Lean errors or a negative verification verdict; it does not establish successful verification. The
bridge remains synchronous for task-aware clients and never submits `lean_runtime` or another `lean_request`.

The registry holds at most 128 prepared, active and finished records. Preparing can reclaim acknowledged records and
expired records, but pressure never removes an unacknowledged terminal result. If all slots are protected, it returns
`request_registry_full` with typed `execution.disposition: not_started`; no tool was submitted. Read or acknowledge
existing results, then prepare new work. Unsubmitted tickets expire after five minutes and become irrevocably
`not_started`; cancellation can close them earlier. Active dispatched requests never expire. Terminal bridge results
have 24-hour retention; negotiated MCP Tasks keep their ten-minute process-local retention. Reads and admissions prune
expired records. The record bound is 4 MiB, so an oversized result explicitly loses its verdict
(`result_available: false`) while retaining execution evidence.

Every bridge record includes:

- `execution.disposition`: `pending`, `not_started`, `stopped`, or `unknown`.
- `execution.reason`: a stable diagnostic code, separate from the Lean verdict.
- `cancellation_requested`: intent, not evidence of exit.
- `result_available` and `acknowledged`: whether the terminal payload is available or consumed.

`not_started` means the entire retained invocation cannot dispatch. Failed preparation of one named target can instead
carry scoped `execution.target_check` evidence while the overall execution remains `unknown`. Configuration evaluation
and its subprocesses may already have started. A multi-operation tool may have completed earlier steps.

Source-backed module queries, proof attempts and declaration verification also retain this scoped receipt with
`stage: "project_admission"` when typed admission expiry or cancellation returns before the actor call. It identifies
only the check named by `file_label`; option discovery, startup and earlier checks may already have run. The overall
execution remains `unknown` without independent operation evidence, or `stopped` only when all started operations have
independently confirmed termination or return. This is retained-request evidence; direct tool envelopes and historical
stored results are unchanged. No receipt authorizes replay or supplies a proof verdict. Errors after admission do not
gain this receipt.

A confirmed worker timeout/cancellation can return `stopped` with an optional `execution.worker_stop_receipt` while the
semantic response reports runtime unavailability and contains no proof verdict. The receipt binds the interrupted
operation to its actor session and worker generation. It covers the direct worker only; it does not certify Lake
descendants, roll back external effects or authorize replay. Unresolved operation evidence remains `unknown` through a
successful fallback. Later queue refusal cannot turn earlier completed preparation or worker work into `not_started`.

Cancellation records intent. A queued operation is skipped; a running operation retains cleanup ownership until it
returns. If completion races cancellation, the original result is preserved. Failed/cancelled task status and the
presence of a proof verdict do not independently release an execution lifetime.

After durably saving the complete response and atomically updating its local source/build association, a client may
acknowledge it:

```json
{"name":"lean_request","arguments":{"action":"acknowledge","request_id":"saved-ticket","request_epoch":"saved-epoch","submission_sha256":"digest-from-request-binding"}}
```

Acknowledgment requires the exact binding and a terminal record. It releases the payload, preserving execution evidence
while the record remains retained. Repeated acknowledgments are idempotent. Acknowledged records can be reclaimed for
new tickets; subsequent access to a forgotten ID returns `unknown` and never relaunches work. A terminal `unknown` may
be acknowledged only after its unresolved lifetime is preserved locally; acknowledgment does not certify exit.

If a lost acknowledgment is retried after its record was reclaimed, the server returns `status: absent`,
`acknowledged: true`, `result_available: false`, the echoed `request_id`/`request_epoch`/`submission_sha256`, and
`identity_verified: false`. It includes no `execution`, request binding or proof result. This confirms only that no
retained record remains; a client that already archived the bound result can clear its pending acknowledgment. It does
not verify the echoed identity or establish an execution outcome. While a record remains retained, a wrong epoch or
digest is still rejected.

Durability is explicit: `request_registry.durable` requires a healthy `--request-journal DIR` or
`LEAN_HOST_MCP_REQUEST_JOURNAL`. With no journal, retention is process-local. Storage failure disables new admission
until current records can be synchronized again; reads and cancellation remain usable. The journal has one OS-locked
broker owner and bounded atomic, synced request snapshots. It stores private response evidence, not a replay queue.

Restart creates a new incarnation for new tickets. Recovered prepared and accepted-before-dispatch records become
`not_started`; dispatch-intent records without a terminal outcome become `unknown`. Terminal results retain their
original binding. No old work is replayed. A healthy new broker is not an old-worker exit witness. Journal recovery
neither adopts nor kills old workers. Invalid or unreadable snapshots are preserved separately with a warning; failure
to preserve them blocks startup. Missing evidence remains unknown. Quarantined files are retained for inspection and do
not count toward registry capacity; repeated failures can grow that separate set of files.

Busy project mailboxes and cold-project capacity admit waiting callers in FIFO order. Their admission wait and worker
execution use separate bounded budgets. Closing an HTTP connection alone does not request cancellation. A synchronous
client should send MCP `notifications/cancelled` with the original request ID when it intends to stop work. Reconnect
and retrieve the same retained ticket after a lost response.

## Worker Activity And Admission Leases

`lean_runtime` is always synchronous, including for task-aware clients. It has five actions:

Every response includes `build` with package `version`, full Git `revision`, and build-time `dirty` state. Missing Git
metadata (for example, an exported source archive) is reported as null, never guessed from the runtime checkout. A Git
revision identifies source, not the resolved dependency graph or binary hash. Build/install with `--locked` for
reproducible dependency selection, and retain the executable hash when an exact runtime artifact must be pinned.
`effective_settings` reports resolved broker/runtime values, including derived heap/residue limits, plus server
presentation and output overrides. Null output overrides mean each tool keeps its own default budget. These are the
values the running process actually uses, not the current contents of a configuration file. No raw environment or
credentials are exposed. Transport addresses are not part of the transport-agnostic tool service.

`request_registry` reports active requests, retained terminal records, capacity, and terminal retention time. A zero
active count does not prove clients have consumed retained results or authorize discarding them in a restart. Settings,
build identity, registry counts, and capability discovery never open a Lean worker.

| Action | Fields | Behavior |
| --- | --- | --- |
| `status` (default) | none | Observe resident workers, startup callers, occupied slots, and admission state. |
| `acquire` | optional `lease_seconds`, `retire_idle` | Atomically pause admission if no worker request or startup is admitted. |
| `drain` | optional `lease_seconds`, `retire_idle` | Pause new admission immediately and let already-admitted work finish. |
| `renew` | `lease_id`, optional `lease_seconds` | Extend the caller's unexpired lease. |
| `release` | `lease_id` | Release that lease and resume ordinary admission. |

Lease lifetimes default to 30 seconds and must be between 1 and 300 seconds. Keep the returned `lease_id` private;
status never reveals it. Another connection can renew or release the lease with its token. An expired token cannot
affect a later lease. Leases expire after client failure and do not survive a server restart; `pool.instance_id`
identifies this broker lifetime. Renew well before expiry while depending on the pause, and stop relying on it if
renewal fails. Losing a response can lose the token; in that case wait for expiry rather than assuming ownership.

Acquire returns `acquired`, `busy` (a request or startup already admitted), or `leased` (another lease holds admission).
It never cancels existing work or waits for capacity. While held, new worker-backed calls return the retryable semantic
reason `worker_pool_leased`; cheap project status and runtime coordination remain available. The ordinary worker queue
and parallelism are unchanged outside a lease. There is no priority or fairness guarantee between competing acquirers.

Servers advertising `draining_lease_v1` also accept `drain`. It returns an owned token with `draining` while earlier
work finishes, or `acquired` when ready. Later worker-backed requests wait inside the server, with their existing
timeout and cancellation, rather than overtaking the drain. They resume after release or expiry without client
resubmission. Already-admitted startup continues; a waiting later caller owns no worker or startup slot.

Renew the same token to advance the drain. With `retire_idle: true`, a successful `ok` renewal confirms workers have
stopped; `draining` means they have not. A generic status call cannot advance a drain or grant permission to start a
build. Do not hold a build-file lock while waiting for work that may need that lock to finish. After taking the build
lock, recheck the live token before starting external work. Competing drain owners still have no fairness guarantee.

Expiry restores server admission, but it does **not** prove an external build stopped. If its owner dies while its child
keeps writing, TTL alone cannot exclude server work. Keep actual build-file/process protections and stop external work
when lease ownership is lost. This feature does not claim crash-safe exclusion from arbitrary external writers.

An ordinary status snapshot is advisory: another request can start immediately after it. Only a successful, unexpired
owned lease closes that race. The admission check and lease acquisition share the registry lock. Admitted callers count
as active before enqueue, and queued/running messages remain active through cancellation cleanup. An opening caller
retains startup ownership even if its HTTP connection disappears. Background recycling and prewarming use the same
admission boundary and count as active work. The pool-level `outstanding_jobs` count also retains work owned by a
controller that has been removed from the resident registry.

By default acquire keeps workers and their warm caches across ordinary idle-reaper ticks. Independent artifact-writer
demand can still retire an affected idle generation after admitted work finishes; the lease continues to fence MCP
admission and is neither advanced nor released by that handoff. For a caller that actually needs their memory,
explicitly set `retire_idle: true` on acquire. `retirement_requested` counts shutdown requests; it does not acknowledge
child exit. Wait for `pool.workers_stopped: true`, under the same live lease, before relying on released worker
capacity. An occupied slot may represent a starting, resident, retiring, or uncertain worker; uncertain shutdown
continues to count and must not be treated as freed memory. Poll no faster than once per second while waiting, and renew
the lease if needed.

The lease covers only this server's worker admission. It does not reserve RAM, stop other programs, coordinate another
server process, bind results to a source revision, or replace a caller's build/output locks. The caller chooses when to
claim and whether retirement is worth losing warm caches.

For example, obtain a memory handoff with `{"action":"acquire","retire_idle":true,"lease_seconds":30}`, retain its
token, observe confirmed exit, renew during the external operation, and release in cleanup. If acquire is busy, continue
independent work or wait for the existing owner; do not kill the active worker.

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
missing/stale build state; `needs_build` degradations report missing `.olean` artifacts. Duplicate trust rows are
collapsed while preserving first occurrence order. Runtime counters, cache timings, and import lists remain telemetry,
not trust.

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
  `target`, string groups (`"Proofs/A.lean"`, `"Proofs.A"`), and the group kinds `file` → `file_all`, `module` →
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

Explicit imports:

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
supervised one-shot Lean process against the prepared imports. A compiled declaration lookup can still succeed after
an edit breaks the proof. Use file-backed verification to check that edit.

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
        "LeanRsFixture.ProofActions.sorryTheorem"
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
module has no source file, `module_all` may use the `.ilean` declaration inventory, with typed artifact freshness facts
in `trust`. Every source-backed trust fact includes `content_sha256`, the SHA-256 of the exact bytes used by the call.

`detail` defaults to `"compact"`, which keeps declaration names, files, statuses, bounded diagnostics and goal text,
sorry/admit facts, axiom facts, and trustworthiness flags. It omits target and ambiguous-candidate span blocks.
For ordinary file-backed verification, diagnostics that exceed the compact row budget may be skipped while later
diagnostics are retained. `facts.output_truncated` identifies partial display evidence. Use `"detail": "full"` when you
need source spans in each row.

`retry_tainted_non_positive` (default `false`) opts into one server-side retry per target-group batch: when the worker
was recycled mid-call and the batch comes back non-positive (any row that would be relabeled `worker_recycled`), the
server re-issues that batch once. `verified` rows are never retried — verification is monotone. If the retry is also
tainted and non-positive, the rows are relabeled to `worker_recycled` with the usual warning, exactly as with the flag
unset. At most one retry per batch; it is surfaced through `telemetry.runtime.retry_count` with full telemetry.

A `not_found` row for an explicit target may carry declarations with matching short names across namespaces and case in
`facts.candidates` (with `"detail": "full"`) and a warning. Mapping a Git diff to explicit target groups is the caller's
job; the server returns no diff-coverage object.

Check `facts_trustworthy` before interpreting absence of sorry or unresolved goals. An empty `axioms` list establishes
that no dependencies were found only when `axioms_available` is true and `axioms_truncated` is false.
`axioms_available` is false when reporting was not requested or the axiom set could not be computed.

## `lean_lookup`

Artifact-backed declaration inventory and reference queries report `unavailable` when shared read custody cannot be
established. They do not reinterpret a busy or unconfirmed writer as an empty index or a missing build. Retry after the
current writer finishes. Custody describes a consistent artifact read; source/index freshness and exact content binding
remain separate trust facts.

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
returns `not_found`; an unavailable module index returns `missing_build`. Shared artifact custody failure returns
`unavailable`, as described at the start of this section. `limit` defaults to 200 and is capped at 1000; truncation
keeps a deterministic prefix and sets `truncated: true`.

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

### `kind: "file_diagnostics"`

Elaborates the current source snapshot and returns the same bounded diagnostics block shape used by proof context. This
surfaces Lean diagnostics and linter messages produced while elaborating the file; it does not run `lake build`,
`lake exe lint`, or other external project-specific lint commands.

```json
{
  "kind": "file_diagnostics",
  "file": "LeanRsFixture/ProofActions.lean"
}
```

The result includes `diagnostics` and the header `imports` used for the worker session, with a source `edit_fresh` trust
fact for the file snapshot.

A worker crash or timeout returns `data: null` and an `errors` issue with `code: "runtime_unavailable"`, with MCP
`isError: true`. Interrupted results are not cached. Planned recycling before a successful check does not invalidate its
result.

### Opt-in managed module lint

`lean_module_lint_v1` accepts an ordered list of qualified module names for the approved primary
project. It runs the project's canonical prebuilt `.lake/build/bin/runLinter` with
`--no-build --module-only --deny=all` and returns its bounded output, exit code, and verdict.
This version uses the same standard Lake layout as the managed artifact trace reader. It
does not resolve custom Lake build or binary directories. If the canonical executable is
missing or resolves through an alias, START refuses with `prebuilt_module_linter_unavailable`
before launching it. The operation does not invoke Lake to discover or build the executable.
