# Architecture

One crate, library + binary. Internal layering:

```
                ┌─────────────────────────────────────────┐
                │  main.rs (clap CLI + rmcp stdio entry)  │
                └────────────────────┬────────────────────┘
                                     │
                ┌────────────────────▼─────────────────────┐
                │  server.rs (LeanHostService, rmcp glue)  │
                └────────────────────┬─────────────────────┘
                                     │
         ┌────────────────┬────────┴───────┬────────────────┐
         ▼                ▼                ▼                ▼
   tools::lean      tools::scan      tools::index    (more later)
   (six handlers)   (one handler)    (three handlers)
         │                                  │
         │                                  ▼
         │                            index.rs
         │                            (DeclarationIndex — SQLite)
         │                                  │
         ▼                                  ▼
       session.rs ◀────────────── rebuild  pipeline
       (SessionHost, dedicated thread)
                  │
                  ▼
        lean-rs / lean-rs-host  (lifetime-bound, single-threaded)
```

## Why a dedicated session thread

`lean-rs` exposes `LeanRuntime`, `LeanHost`, `LeanCapabilities`, `LeanSession`, and `SessionPool` as `!Send` types
carrying a `'lean` lifetime anchored to a process-global runtime. They cannot cross a `tokio` task boundary; they cannot
live inside an `Arc<Mutex<…>>` accessed from multiple async tasks.

`SessionHost::spawn` creates one OS thread that initialises `LeanRuntime`, opens the `LeanHost`, loads capabilities, and
serves requests off a `tokio::mpsc::Receiver` in a blocking loop. Tool handlers submit a `Request` enum value and
`await` a `oneshot` reply. From the caller's side this looks like an async actor; the lifetime tangle stays hidden.

This is also the abstraction boundary that lets us swap the in-process implementation for `lean-rs-worker` (subprocess
isolation) later without touching tool code — the `SessionHost` API is closure / channel shaped on purpose.

## The envelope contract

Every tool wraps its result in `Response<T>` from `envelope.rs`. That struct is the only thing the entire tool layer
agrees on. The three volatile decisions it hides are:

1. **What "freshness" means.** Today: lake root, imports, session id, toolchain label. Tomorrow we may add file-version
   vectors or build ids. Tools don't pick the shape.
2. **What a warning looks like.** A plain string today, but maybe a structured `{ code, message }` once we have a stable
   warning catalogue.
3. **Whether `next_actions` are present.** A hint surface for the LLM client. Tools sprinkle them; the envelope decides
   if/how to serialize.

## The `DeclarationIndex` boundary

Three tools (`find_symbol`, `find_lemma`, `outline`) all answer "what declarations match X" against the open Lake
project. They share exactly one piece of state: a SQLite database under the user's cache directory, keyed by
Lake-manifest hash. `src/index.rs` owns that boundary in full — schema, fingerprinting, bulk rebuild, and the seven read
methods the tools consume. Nothing past the module sees `rusqlite` or `sha2`. A fourth caller that wants a new query
adds a method here; no caller writes SQL.

Rebuild is on-demand, gated by SHA-256 of `lake-manifest.json`. The session walks the live environment via
`LeanSession::list_declarations_strings`
+ `declaration_kind_bulk` / `declaration_type_bulk`, the index commits
in one transaction, and the fingerprint is stamped last so a crashed
rebuild leaves the index detectably stale rather than partially populated.

## Why no `lean-rs-worker` yet

Subprocess isolation is real value, but pulling in `lean-rs-worker` means authoring a separate worker child binary,
wiring its framed-protocol transport, and threading capability loading through the supervisor. None of that earns its
keep until we actually see a tool that wedges the in-process Lean state. The `SessionHost` shape will accept a
worker-backed implementation later without rippling to tools.
