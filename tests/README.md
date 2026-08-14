# Workspace-level integration tests

Currently empty by design. All integration tests live under each
crate's own `tests/` directory (per-crate, not workspace-level).

If we ever need cross-crate end-to-end harnesses that don't fit
into a single crate's `tests/`, they go here.
