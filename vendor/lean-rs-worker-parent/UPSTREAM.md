# Worker supervisor correction

This directory contains the published `lean-rs-worker-parent` 0.9.4 source from
upstream commit `d2dc03b4c53d6c811d5bef1fe6d481a110c374d1`, including its original
Cargo manifest and licenses. The workspace applies it through a relative Cargo
patch so builds do not depend on a developer's checkout or modified registry cache.

The local corrections observe cancellation while a worker is silent, retain the
original request deadline, and clean up children when startup fails. Regression
tests use the worker protocol without loading Lean. The same correction is committed upstream locally as
`5650444` for a later release; no dependency source or version was published.

Remove this patch after upgrading to an upstream release containing these fixes.
The worker protocol and child binary remain compatible with 0.9.4.
