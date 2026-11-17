# Fix the `deny` and `lint` gates (owner-approved)

The owner approved this on 17 Nov 2026. Both gates were failing before hardening started, for reasons outside any H package (recorded in H0.1 and H0.2).

## deny: RUSTSEC-2026-0543 (chrono unmaintained)

`cargo deny check advisories` failed on `RUSTSEC-2026-0543`: "The chrono crate is unmaintained; all versions are affected." The advisory reports no vulnerability, and no fixed release exists. Besides AgentKit's own uses (provider fetch timestamps in `providers/src/{chat_completions,responses}.rs`, RFC 3339 parsing in `store/src/ts_import.rs`, test and example code), `rmcp`, `oauth2` and `schemars` depend on chrono. Replacing AgentKit's uses with `time` or `jiff` would therefore not clear the advisory.

Change: `rust/deny.toml` gains an `[advisories] ignore` entry for `RUSTSEC-2026-0543`, with its reason and a revisit condition: rmcp drops chrono, or a maintained release appears. This grows an allow-list, with the owner's explicit approval.

## lint: `bge_m3_reference.json` formatting

`bun run lint` (Biome) failed because `rust/crates/embed-local/tests/fixtures/bge_m3_reference.json` (from `89acf19c`, R19) was not in Biome's format. Change: `biome format --write` on that file only.

The values are unchanged: the file was parsed before and after with Python's `json` and compared after key-sorted serialisation, and the result was `values identical: True`. The fixture's only reader, `crates/embed-local/tests/bge_m3_parity.rs`, parses it with `serde_json`, and `cargo test -p agentkit-embed-local --all-features` passes (8 + 1 tests).

## Commands run

| Command | Result |
|---|---|
| `cargo deny check licenses bans advisories` | advisories ok, bans ok, licenses ok |
| `bun run lint` | no errors (2 infos, as before) |
| `cargo test --locked -p agentkit-embed-local --all-features` | pass |
| `node scripts/gates.mjs` (full battery) | recorded below |

## Full gate battery (17 Nov 2026)

`CARGO_INCREMENTAL=0 node scripts/gates.mjs`: `OVERALL=0 (19/19 passed)`. The test step reported 3072 passed across 329 binaries and 0 known failures; the intermittent interrupt test passed in this run. `buntest` reported 11 known failures, all listed. Every AgentKit gate is green.
