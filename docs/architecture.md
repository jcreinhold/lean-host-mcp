# Architecture

`lean-host-mcp` is a thin MCP server over `lean-rs-worker`. The public API is intentionally declaration-centric: agents
name a declaration and, when needed, a proof position inside that declaration. They do not pass cursor coordinates,
source spans, byte offsets, replacement ranges, or inserted spans.

The model-facing tools are:

```text
proof_state -> search_for_proof -> inspect_declaration -> try_proof_step -> verify_declaration
```

`find_references` is the bounded semantic support tool. Text grep, Mathlib placement policy, raw hover/type queries,
and low-level term/meta primitives are not part of the public MCP surface.

## Crate Layout

```
main.rs         clap CLI, rmcp stdio entry
server.rs       LeanHostService tool registration
tools/          declaration.rs, proof_search.rs, proof_action.rs, position.rs
project.rs      LeanProject worker actor for one Lake project
projections.rs  stable MCP projections from lean-rs worker types
lake_meta.rs    minimal Lake-project metadata
cache.rs        small LRU for bounded reference queries
envelope.rs     Response<T> = { result, freshness, warnings, next_actions }
bin/worker.rs   entry point for lean_rs_worker_child::run_worker_child_stdio()
```

`server.rs` is glue only. Tool implementations resolve a project through `ProjectBroker`, read source files when
needed, and send one typed read-only semantic job to the project worker actor.

## Project And Worker Boundary

`ProjectBroker` owns an LRU pool of `Arc<LeanProject>`, keyed by canonical Lake root. It handles idle eviction,
manifest invalidation, multi-project routing, and coalescing concurrent opens for the same canonical root.
`LeanProject` owns the supervised `lean-rs-worker` child and a dedicated actor thread. The actor has private state
(`Ready`, `Restarting`, `Draining`, `Stopped`), a bounded mailbox, a worker generation counter, the last restart reason,
the last import profile, and module-query cache handles. The parent binary never links `libleanshared`; the
per-toolchain worker child does.

Each project actor runs one semantic job at a time in FIFO mailbox order. The broker also owns a process-wide semantic
permit gate, defaulting to one permit, so cross-project heavy calls are serialized unless the deployment explicitly
raises `LEAN_HOST_MCP_SEMANTIC_PERMITS`. A full project mailbox is a structured retryable infrastructure error rather
than unbounded memory growth.

The actor samples worker RSS before import-profile switches. If the worker is above the configured soft ceiling, it
cycles the child before opening the next session. Fatal child exits are caught inside the actor: the worker is rebuilt,
the generation is bumped, and read-only semantic jobs are retried once. Responses carry runtime facts (`worker_generation`,
`worker_restarted`, `retry_count`, `queue_wait_millis`, `restart_reason`) so clients can distinguish Lean-domain results
from infrastructure recovery.

Each semantic tool opens a short-lived worker session with imports derived from the source header or explicit request.
Lean-domain failures such as parse errors, elaboration diagnostics, missing imports, unsupported shim exports, failed
proof snippets, and sorry policy failures are structured tool results. MCP errors are reserved for infrastructure
failures such as a full mailbox, restart-loop exhaustion, or a project that cannot start.

## Declaration-Centric Proof API

Proof tools share the same anchor shape:

- `file`: the source file containing the declaration.
- `declaration`: the selected declaration name.
- `proof_position`: optional selector inside the declaration.

`proof_position` is intent-shaped:

- `{"kind":"default"}` selects the first open tactic state in declaration order.
- `{"kind":"index","index":N}` selects the Nth open tactic state.
- `{"kind":"after_text","text":"...","occurrence":N}` selects a tactic/source fragment inside the declaration body.

The Lean shim resolves these selectors to private source evidence. Raw offsets, line offsets, syntax spans,
indentation, overlay insertion, and diagnostic-locality classification stay below the worker boundary.

## Tool Semantics

`proof_state` returns the current goals, locals, expected type, diagnostics, cache status, and timing facts for one
declaration proof position.

`search_for_proof` builds a small target profile from `proof_state` or explicit goal/type text, calls bounded lean-rs
declaration search, and ranks metadata-only candidates. It does not render candidate types and does not build a host
declaration index.

`inspect_declaration` inspects exactly one declaration by name. Optional `file` input is used only to derive local
imports so project declarations can resolve. Rendered fields are capped before crossing the worker boundary and carry
their own truncation flags.

`try_proof_step` tries tactic fragments at the selected proof position in an in-memory overlay. It reports per-candidate
status, local diagnostics, downstream diagnostics, resulting goals, and the resolved declaration/proof-position summary.
It never writes source files.

`verify_declaration` elaborates the in-memory source snapshot and checks one declaration under the requested sorry/axiom
policy. Policy failures are normal results.

`find_references` runs semantic reference lookup for a fully-qualified name in file or bounded project scope. Project
scope reports scanned/skipped files, header failures, missing imports, unsupported files, and truncation.

## Removed Public Surface

The server deliberately does not expose:

- low-level term/meta tools: `elaborate`, `kernel_check`, `infer_type`, `whnf`, `is_def_eq`;
- LSP-shaped declaration tools: `hover_by_name`, `type_of_name`, raw `search_declarations`;
- raw module-query access: `lean_query`;
- text search and placement policy: `source_search`, `project_scan`, `mathlib_placement`;
- split reference aliases: `references_in_file`, `references_in_project`.

If one of these capabilities is needed to implement a deeper tool, keep it private and compose it inside the host or
worker. A need to expose it directly is treated as evidence that a better proof-work abstraction is missing.

## Budgets And Caching

Worker APIs enforce per-field and total output budgets before IPC. The host adds hard caps for candidate lists,
reference fanout, and response shape. The worker owns module snapshot reuse and reports cache status, output bytes, and
phase timings for declaration-context queries. The host keeps only a small content-hash LRU for the older single-file
reference path.

Frame size is controlled by query shape, not transport tuning: no public tool requests a whole info tree, bulk rendered
types, or unbounded source/project scans.
