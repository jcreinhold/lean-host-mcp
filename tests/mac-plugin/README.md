# Darwin plugin worker regression candidate

Run from an idle, matched build lane with a prepared Lean 4.36.0 deployment.
The runner requires the reviewed plugin deployment packet and alternate module
identified in run.py, the absolute path and independently verified digest of the
matched host parent, a clean SDK checkout at the pinned commit, an existing Cargo
target and a fresh scratch path. Run `python3 tests/mac-plugin/run.py --help`
for the input flags. Keep the scratch directory on failure for custody inspection.

The test builds the link-free controller and the linked worker separately. The
test worker installs the fixed deployment authority and calls the actual host
worker main. SharedWriter and native guardian dispatch use the supplied host
binary. The child checks the artifact inventory, mapped image and opened root
order under read protection. Only the fixed diagnostics query is granted.
Replacement and restoration go through the normal writer. Each observation
opens a fresh handle, with no retained request ticket or broker cache.

Building the matched host parent is a prerequisite. The published dependency
pins on this host base predate typed startup custody. An earlier helper or a
private SDK overlay cannot qualify this candidate.
