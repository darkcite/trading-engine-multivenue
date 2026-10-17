# Workspace-level integration tests

Currently empty by design. All integration tests live under each
crate's own `tests/` directory (per-crate, not workspace-level).

If we ever need cross-crate end-to-end harnesses that don't fit
into a single crate's `tests/`, they go here.

- `conformance/` holds PVM-6's fixtures (ours, tracked): `chart.json` (impl-00's), the `.fix`/`.pine` catalogue, `manifest.tsv`, `pins.tsv`, probes and sheets (impl-06 §2.3).
