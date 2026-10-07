# OpenPCB Responses consumer fixture

`catalog.json` contains only the 15 native `data.inAppTools` definitions from
[OpenPCB's published current catalog](https://github.com/OpenPCB-app/OpenPCB/blob/d69189fa88605e140af01c1246e2da54b04f2201/src/core/backend/tests/fixtures/assistant-parity/catalog.current.json).
Descriptions, names, versions, effects, capabilities, and schemas are unchanged.
Provider records, MCP catalogs, database schema, and migration snapshots are
excluded. The fixture contains no credentials or application state.

Provenance records the pinned commit, exact upstream file SHA-256, upstream
canonical-data SHA-256, and SHA-256 of `JSON.stringify(tools)` in source order.
Tests verify the tool digest before advertising the catalog. Updating this
fixture requires reviewing the upstream diff and replacing its provenance.

The host tests and exported-package Node qualification use a mock Responses
transport and a stub for `designer_get_design_summary`. They prove catalog
preservation, exact call/result identity, private continuation, failure handling,
and durable reopen. They do not execute OpenPCB or prove PCB mutation, receipts,
undo, or application integration; those remain OpenPCB consumer gates.
