# ADR 0034: Output store and masking

Status: accepted (2026-11-15). Implements package 2.1 of the context
efficiency spec (CE-10, INV-6, layer 1 of "Tool output handling"). Builds on
ADR 0024 (repository ports) and ADR 0031 (deferred tools). Layer 4 (batched
masking, package 2.x) is decided here and built later; it reuses this store.

## Context

A tool result over the byte budget was cut by the `[...truncated]` marker in
`core/src/envelope.rs`, and again in the MCP bridge. The cut text is gone:
the model cannot ask for the rest, and a failing command lost the end of its
output, which is where the error is. INV-6 asks the opposite: whatever is
truncated, masked or summarized stays readable in full through a handle until
its retention ends.

## Decision

1. **One port, `OutputStore`, in `agentkit-core`.** `put`, `read`, `search`,
   `purge_session`, plus `info` (the index row, needed to decide who may read
   a handle). Handles are `out_` plus a UUID v7 as 32 lowercase hex digits and
   are parsed before use, because a handle names a file. Faults are
   `OutputError` (core cannot name the engine's `RepositoryFault`; adapters
   map into it).
2. **Storage.** Bytes live in a file under a private output root chosen by
   the host (directories 0700, files 0600, created through `agentkit-sys`,
   never inside a workspace). One directory per session, named by the SHA-256
   of the session id. The index is the `tool_outputs` table (handle, session,
   task, call id, tool, mime, exit code, byte length, line count, SHA-256,
   created, expires), added by a store migration. The file is written and
   synced before its row is inserted. A read re-checks the length every time
   and the SHA-256 on the first read of a handle in the process, and refuses
   a file that no longer matches. A handle is parsed again wherever it is
   joined onto a path, and a handle read from the wire (serde) goes through
   the same strict parse. One stored output holds at most 64 MiB: a larger
   result is stored cut at that size and its footer says it was truncated at
   the storage cap.
3. **Twin and conformance (ADR 0024).** `InMemoryOutputStore` in testkit and
   one suite, `conformance::output_store`, run against it and against
   `SqliteOutputStore`. Line slicing, search and preview are pure functions in
   core that both adapters call, so they cannot disagree on numbering.
4. **When.** `OutputGate` runs on every tool result in the run loop, before
   the envelope is built, so a stored result is stored before the model first
   sees it. Default inline limit 8 KiB, overridable per tool; the runtime
   builder refuses a configured limit under 1 KiB, and the gate holds the
   limit inside the context's result byte budget (less room for the
   envelope's own fields). Any over-limit result is stored whatever its
   `ok` or exit code; only the preview split depends on failure. A run with
   no chat or scope stores under its task id, which `delete_chat` purges with
   the task's chat. MCP results and `tool_call` results pass the same gate
   (the inner call is what runs). The store's own tools, `read_output` and
   `search_output`, are exempt.
5. **Preview.** Line-aligned first 3 KiB, a marker `[… N bytes and M lines
   hidden …]`, line-aligned last 2 KiB, and a footer naming the handle and the
   two tools. A failure (non-zero exit code, or `ok: false` without one) moves
   the split to 1 KiB head and 4 KiB tail. A line ends at `\n`, `\r\n` or a
   lone `\r`. A single line longer than its budget is cut inside and marked
   `[line cut]`. The budget is checked against the JSON-escaped size, so the
   envelope never cuts the footer or the handle of a quote-heavy preview. The preview budgets shrink to fit an inline limit below 8 KiB, so a
   preview is always within the limit it was made for (above a floor of about
   0.5 KiB for marker and footer). Structured results are laid out as
   `key: value` and `=== key ===` sections, so multi-line fields keep real
   lines.
6. **Failures.** If the store cannot take the bytes the gate fails open: the
   redacted result is returned whole with a warning ("kept whole, bounded by
   envelope") and the envelope's byte budget bounds it, exactly as before. The `[...truncated]` marker therefore stays
   as the bound for a host with no output store; with a store it is not
   reached. The MCP bridge stops pre-cutting its text when the run loop tells
   it (via `ToolContext.metadata["outputStore"]` and the tool's limit in
   `["outputInlineLimit"]`) the gate will store text over that limit; a result
   the gate will not store keeps the bridge's own cap
   (`OutputGate::will_store` is the same decision).
7. **Secrets.** One redactor, in `agentkit-core` (`is_sensitive_key`,
   `is_bag_key`, `redact_value`), serves the gate and the store's consumer
   event redaction. A key is a credential when its name, lowercased with
   punctuation dropped, contains `secret`, `token`, `password`, `apikey`,
   `privatekey`, `accesskey`, `authorization`, `cookie` or `credential`
   (`AWS_SECRET_ACCESS_KEY`, `x-api-key`, `private_key`); names ending in
   `ref(s)`, `tokens`, `count`, `limit` or `usage` are not values and
   survive. `env` and `headers` bags are masked member by member. The value is
   redacted before the size check, so a small result goes inline redacted
   too, and the fail-open path returns the redacted value. An optional host
   redactor (`OutputGate::with_redactor`) runs on every string. Text
   attachments (`model_content`) go through the same redact, store and
   preview path. MCP results are already redacted by the MCP session. The
   preview is cut from the redacted text, so the store and the model see the
   same bytes.
8. **Tools.** `read_output(handle, start_line, max_lines)` and
   `search_output(handle, pattern, max_matches)` are direct framework tools
   with effect `read`, contributed by the runtime (`output` namespace). A call
   is allowed only for a handle whose session is the caller's own or a
   descendant subagent session of it (from the `subagents` table). A child
   cannot read its parent's or a sibling's outputs; package 4.1 adds explicit
   per-handle grants through `context_refs`. A handle of any other session, an
   unknown handle and a malformed one all answer `output_not_found`, so the
   answer does not reveal which handles exist. A read is capped to 12 KiB and says where to continue.
9. **Retention.** Outputs are removed with their chat: `delete_chat` deletes
   the rows in its transaction, renames the session directories aside just
   before the commit, removes them after it, and renames them back when the
   commit fails, so a failed deletion leaves the outputs readable. A chat
   with output rows on a store handle that has no output root is not deleted
   (`StoreError::OutputsUnavailable`). Outputs also expire 30 days after the
   session's last activity: a write or read moves the expiry of every output
   of that session forward. An expired handle reads as not found. The runtime
   builder sweeps expired outputs when it builds and then every hour; the
   sweep also removes aside directories and files no row names (older than an
   hour, so an in-flight write is spared).
10. **Masking (layer 4, not built here).** A masked result body becomes
    `[result masked to save context · N bytes · handle out_…]`. A result that
    was inline is written through this same port at mask time, so the handle
    exists. Masks are a derived record applied on assembly; canonical history
    is untouched. Nothing in this package changes history or the prompt
    layout.
11. **Shaped results (layer 2, package 2.2).** A tool that knows a smaller
    text for the model returns `model_data = shaped_value(raw, shaped, N,
    what, note)` (core `output.rs`), only when the context says a store is in
    play and only when its own text is over the inline limit; a result at or
    below the limit is returned unchanged and nothing is stored. The marker
    carries a per-process proof written by `shaped_value`: the gate takes it
    only with that proof and a string `text`, and strips the marker keys from
    every other result (also on the early return of a session-less call), so
    a foreign tool cannot hide its output behind a forged marker. The gate
    accepts a shaped text only when it hides something, fits the inline limit
    and, footer included, is at most 80% of the raw text; otherwise the raw
    path (preview or whole) applies. An accepted shaped text goes to
    `model_data` as `<shaped>\n[<N> <what> hidden · <note> · handle out_…]`;
    the tool's `data` (exit code, signal, `fullLog`) and its `truncated` flag
    stay. The raw text goes to the store, cut at the 64 MiB storage cap (the
    footer says so when it cuts). For `run_command` and `run_tests` the raw
    text is the full log (`details.fullLog`, at most 64 MiB) and the rules,
    including the `run_tests` counts, run on it. For a tool whose own cap cut
    its output (read_file, git_inspect, code_search, list_dir, edit_file) the
    footer says `tool output capped at <limit>` and the hidden counts cover
    the returned part; read_file counts from the file's real length when the
    tool's notice gives it. A shaped read_file marks the version read in the
    session ledger as partial, since the model saw an outline. The coding
    pack's rules live in `toolpacks-coding/src/shape/`; rules see the text
    without ANSI codes (the store keeps it as it was), a line naming a failure
    is never dropped, a rule fault returns the raw text with a warning, and a
    rule that drops lines checks no failure line was lost.

## Consequences

- A command that prints 1 MiB costs the context about 5 KiB, and the model
  can page or search the rest without running the command again.
- Disk use grows with output volume until the 30-day or chat-deletion purge;
  one output is capped at 64 MiB.
- Schema version 20 adds `tool_outputs`. The version number is provisional
  and is ordered against other migrations at merge.
- Descendants are read from the `subagents` table; a session deleted while
  its children live cuts the link, and the parent then reads only its own
  outputs.
