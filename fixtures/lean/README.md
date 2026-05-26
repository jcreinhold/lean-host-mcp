# `LeanRsFixture`

Self-contained Lake package the `lean-host-mcp` end-to-end tests load. Doubles as a minimum-viable template: copy the
layout, rename the package and library, and the host server can serve it.

## What's here

```
lakefile.lean         Lake DSL declaring the `lean_rs_fixture` package
                      and the `LeanRsFixture` lean_lib (shared facet on)
lean-toolchain        pins the Lean toolchain used to build and serve
LeanRsFixture.lean    umbrella import; `lean-host-mcp` opens with
                      this as `default_imports`
LeanRsFixture/        one submodule per ABI category covered by the
                      tests (Scalars, Strings, Containers, Effects,
                      Evidence, Capability, SourceRanges)
```

The submodules each declare a few `@[export]`-style symbols against one Lean ABI shape (scalars, strings, containers,
effects, evidence, capability state, source-range corner cases). The host's e2e suite opens this project, asks for
declarations and diagnostics, and verifies the wire-shape contract.

## Build

```sh
cd fixtures/lean
lake build
```

Artifacts land under `.lake/build/`:

- `.lake/build/lib/liblean__rs__fixture_LeanRsFixture.{dylib,so}`: the shared library the host worker `dlopen`s.
- `.lake/build/lib/lean/LeanRsFixture/*.olean` and `.lake/build/lib/lean/LeanRsFixture.olean`: per-submodule oleans the
  worker walks for declarations and source ranges.

Lake mangles each underscore in the package name to a double underscore in emitted symbol and filename strings, so the
module initializer is `initialize_lean__rs__fixture_LeanRsFixture`.

## Using as a template

Rename `package «lean_rs_fixture»` and `lean_lib «LeanRsFixture»` in `lakefile.lean`; rename the matching directory and
the umbrella import. Keep `defaultFacets := #[LeanLib.sharedFacet]` on the `lean_lib`; that's what `lean-host-mcp`
`dlopen`s. If your project depends on mathlib or other prebuilt libraries, make sure their `:shared` facets are on the
link line; symbol-resolution failures at `dlopen` time surface as a `BadProject` error from the server with the missing
symbol named verbatim.

Pointed at by the e2e tests via `LEAN_HOST_MCP_TEST_FIXTURE` (defaults `lean_rs_fixture` / `LeanRsFixture` for the
package and library names; override with `LEAN_HOST_MCP_TEST_PACKAGE` and `LEAN_HOST_MCP_TEST_LIBRARY`).
