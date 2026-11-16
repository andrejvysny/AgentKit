# ADR 0036: Turn context

Status: accepted (2026-11-15). Implements CE-01 (selected content inside the
user message) and CE-03 (replayed unchanged) of the context efficiency spec,
roadmap package 1.2. Builds on ADR 0028 (prompt layout) and ADR 0022 (Rust owns
the wire contract).

## Context

Selected skills and templates travelled as request-local `selected_content`,
sent ahead of the system prompt on Chat Completions and as leading input items
on Responses. Selecting or dropping a skill changed the first bytes of the
request (finding F1), and nothing recorded what a turn had been sent, so a
request could not be replayed (INV-2).

## Decision

1. **A content-part type.** `AiTurnContextPart { type: "turn_context", text }`
   joins `AiContentPart` (Rust DTO, schema bundle, TypeBox). The text is
   wrapped in `<turn_context>` so a model can tell it from what the user typed.
2. **Built once, persisted with the user message.** When a turn starts, the
   chat executor builds the turn context and writes it as the last part of the
   turn's user message, together with the session record below, in one
   transaction. A user message that already has the part keeps it unchanged:
   a retried, resumed or restarted turn replays it byte for byte. The user's
   own text stays the first part.
3. **Contents.** In order: `<date>` (opt-in, below), one element per newly
   selected item (`<skill name revision>`, `<template name revision>`,
   `<source name>`), then delta notes (`<note kind=...>`).
   - Skill revisions are written as `sha256:<hex>` when they are a content
     hash. Names are skill ids.
   - A skill or template already in the history is not sent again; a new
     revision is. "In the history" means the history the request is built from:
     each turn-context message records what it wrote (message metadata
     `agentkit.turnContext`: bodies sent, de/reactivation notes), and the turn
     folds those records along the user message's ancestry, leaving out what
     the newest stored summary covers. The chat-level record below is a cache
     and is never read for this decision. A fork, an edit or a compaction
     therefore carries exactly what its own path still holds; a message
     written before this rule has no record and counts as carrying nothing
     (its instruction is sent once more). After the request's own rewrite (a
     summary or trim written by this very request), the executor checks the
     final messages again and re-sends a selected body they no longer hold as
     one more turn-context part of the turn's user message. A fork keeps the
     session's current selection, so its first turn sends a body its path does
     not hold.
   - Deselecting a skill or template keeps its pin, marked inactive, and
     appends a `skill_deactivated` / `template_deactivated` note. The earlier
     body stays in the history. Selecting the same revision again appends a
     `skill_reactivated` / `template_reactivated` note instead of sending the
     body again; a new revision sends the body.
   - An item is recorded as carried only if its body was written. An item that
     got a placeholder is sent again by the next turn that selects it.
   - Source snapshots come from `sourceSnapshots` (`[{name, text, origin?}]`)
     in the turn's task payload. At most 32 are written; the rest leave one
     `[N more sources omitted]` line.
   - Items whose origin is `workspace` or `external` (sources: `origin`;
     skills and templates: `contentSnapshot.origins`, id to origin) carry
     `trust="untrusted"`, the same idea as the untrusted-workspace notice on
     project instructions.
   - Every wrapper tag (`turn_context`, `skill`, `template`, `source`, `note`,
     `date`), opening or closing, in any letter case and with whitespace, is
     defanged inside a body (`<\/skill>`), so content cannot close its own
     wrapper or open a block. Typed user text has any `<turn_context` tag
     defanged the same way when the wire message is built.
   - `turn_context` is a host-only part. Every submit (`send`, `follow_up`,
     `steer`, `submit_chat`, and the REST submit schema) refuses a user
     message that carries one (`invalid_input`).
4. **Date.** `ChatExecutorOptions::turn_context_date` (default off) adds
   today's UTC date when it differs from the last date a turn context sent.
   It is off by default because it is the one value that changes daily.
5. **Delta notes.** `append_turn_note(store, chat, TurnNote)` queues a note of
   kind `project_rules_changed`, `core_memory_changed`, `tool_namespace_added`,
   `tool_namespace_removed` or `skill_deactivated`. The next turn that starts
   renders and clears the queue. Packages 1.4 and 3.3 call it. The queue is
   read, rendered and cleared inside the transaction that commits the part, so
   a note queued meanwhile is never lost. Notes are coalesced per (kind,
   subject); at most 16 are written, the rest leave `[N notes omitted]`.
6. **Budget.** 4,000 estimated tokens (characters / 4) per turn. The date and
   notes (counted in the budget) are always written; items fill the rest in
   order. An item that does
   not fit is never dropped silently. With a handle the placeholder reads
   `[omitted: about N tokens, over the B-token turn budget; full text at handle
   H; read it with read_output(handle, start_line, max_lines)]`. The handle
   comes from `OversizedItemSink` (`ChatExecutorOptions::turn_oversize`) when
   the host sets one; otherwise, when the executor has an output store
   (`ChatExecutorOptions::output`), the full text is stored under the chat's
   scope before the transaction opens and `read_output` serves the handle
   (ADR 0034). A retry that loses the race stores the text again (an orphan
   until retention). With neither, or when the store refuses (run warning
   `turn_context_spill_failed`), the item renders `[Selected <kind> <name> (N
   bytes) could not be supplied in full: ...]` followed by at most its first
   2,000 characters. Such an item is not carried, so a later turn that still
   selects it tries again.
7. **Session record.** Chat metadata `agentkit.session.turnContext` holds the
   skills (id to revision) and template the history carries, the last date sent
   and the queued notes.
8. **Transports.** No transport sends selected content ahead of the system
   segments. The executor leaves `ChatRequest::selected_content` empty. A
   caller that still fills it gets the content folded into the last user
   message (`agentkit_core::fold_selected_content`) on Chat Completions and
   Responses; on Responses it is no longer part of `instructions`. A stored
   `turn_context` part crosses as a plain text part (`content_for_wire`):
   history assembly converts it, and the provider content mappers accept it.
9. **Wire shape of the stored message.** A string body becomes
   `[{type:"text", text}, {type:"turn_context", text}]`. A message that gets no
   turn context is left as it was (the common case stays a string).
10. **Project rules snapshot (CE-06, package 1.4).** Chat metadata
    `agentkit.session.projectRules` holds the frozen S3 text, its digest, the
    trust it was rendered for, the last-seen files (text, hash, size, mtime)
    and the files refused at the last read. Later turns re-read the files and
    queue a `project_rules_changed` note per changed file (unified diff, the
    full text when shorter, `(whitespace or line-ending change only)` when no
    line differs). Notes totalling more than 8 KiB since the last freeze, a
    change of workspace trust, an unreadable record (run warning
    `project_rules_record_unreadable`, revision keeps counting) and an explicit
    reload or compaction re-freeze: S3 adopts the current files, queued notes
    are dropped and the prefix record reports `context.cache_break` (reason
    `refreeze`). A file refused on a later turn (symlink, special file) keeps
    its frozen text and warns once per change of state. Every write is one
    transaction that checks the lease fence and compare-and-sets on the record
    it was computed from; a lost race adopts the winner's record.
    Limit: change detection is size plus mtime, trusted only when the file was
    modified more than 2 s before it was read. An edit that preserves both
    size and mtime is not seen until a reload.

## Consequences

- Selecting a skill no longer changes any byte before the new turn. The skill
  stays in history from the turn it was selected and costs cached input.
- The recorded part is history: compaction and masking reclaim it. What a
  request carries is derived from its own messages, so a compaction, a fork or
  an edit re-sends a selected skill whose message is gone (tests:
  `chat/tests/turn_context_persist.rs`).
- The `T4.4` recommender line is a `toolset_recommendation` note written into
  the turn context when the turn starts, so continuation inputs replay it
  unchanged (INV-2). Nothing is appended to the request per attempt.
- Accepted risk: text of a connector whose access was revoked stays in the
  history until masking removes it; package 2.4 (masking) reclaims it. The
  turn context does not rewrite stored messages.
- Anthropic: the request builder is in OneMind; package 1.7 must read the
  user message parts (`turn_context` as text) and stop reading
  `selected_content`.
