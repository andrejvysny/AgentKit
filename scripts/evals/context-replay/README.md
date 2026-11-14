# Context replay and simulated billing

This offline evaluation scores scripted sessions captured by AgentKit's real host assembly. The captured corpus contains five Chat and five Code sessions. Provider frames, tool results, and outcomes are synthetic; completion does not establish task success. Captured request values are not raw HTTP wire bytes.

Run from the repository root:

```sh
bun scripts/evals/context-replay/score.ts docs/evidence/context-efficiency/0.4/sessions.json /path/to/new-report.json
```

The command runs the repository's pinned Rust `context_capture --replay` verifier before scoring. It checks that the input remains unchanged and refuses to overwrite the report. Cargo dependencies must be available locally. The verifier checks freshly assembled requests, response frames, and durable output events against the fixture.

The report uses illustrative, versioned rates in abstract units, not provider prices. Its cache and token estimates use serialized request bytes divided by four; they do not predict provider billing. Output excludes reasoning. Unknown reasoning, ambiguous project and memory attribution, and unknown follow-the-handle counts remain `null`. Bills with unknown reasoning have no finite upper bound. Output validation currently supports one text or tool result per frame and rejects unsupported multipart or mixed frames.

These baseline-only scripts do not establish CE-17. That requires a comparable candidate replay and unchanged task outcomes.
