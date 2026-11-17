import LeanRsFixture.Scalars
import LeanRsFixture.Strings
import LeanRsFixture.Containers
import LeanRsFixture.Effects
import LeanRsFixture.Evidence
import LeanRsFixture.Handles
import LeanRsFixture.Meta
import LeanRsFixture.SourceRanges
import LeanRsFixture.ProofActions
import LeanRsFixture.ProofSearchFacts
import LeanRsFixture.ProofAgent
import LeanRsFixture.ScanForms

/-! Roll-up module for the `LeanRsFixture` Lake library.

    Imported by host integration tests and benchmarks. These modules are
    ordinary consumer Lean code: host shims are bundled with `lean-rs-host`
    and loaded by the worker, so this library does not import them. -/
