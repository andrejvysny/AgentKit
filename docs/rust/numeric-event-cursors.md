# Numeric event cursors

`TaskEventEnvelope.seq` and `AiRunEvent.seq` are finite JavaScript numbers. The schemas do not require integers, safe integers, contiguous increments, or a nonnegative value. A durable log orders its events by strictly increasing sequence values. The TypeScript stores separately require the first appended sequence to exceed their initial high-water mark of `-1`.

The Rust HTTP transport uses an exclusive `Option<f64>` cursor containing the last delivered sequence. `None` means the beginning of the log. A recognized `Last-Event-ID` resolves to that event's exact numeric sequence; an unknown ID replays the whole log. Pagination never adds or subtracts `1`, so fractional values and large finite numbers retain their ordering. The backend must return committed events in strictly increasing order. Task settlement still requires an authoritative terminal/interrupted status and a final exhausted read before the settlement marker.

The Rust client and native run-state reducer keep a finite numeric high-water mark. A sequence at or below that mark is a replay and does not change text, the reconnect ID, or retry progress. Any greater finite sequence is new, including negative values, fractions, gaps, and values beyond JavaScript's safe-integer range. Native core-generated events remain integer counters. Empty string event IDs are valid; reconnecting with an empty ID replays from the beginning, and the numeric high-water mark suppresses duplicates. Missing or nonstring IDs remain malformed.

## Reproduced TypeScript HTTP defects

The reference HTTP implementation in `packages/transport-http/src/sse.ts` initializes a next-sequence cursor to `0`, advances it with `event.seq + 1`, and queries the exclusive store cursor with `cursor - 1`. Those operations assume integer spacing that the event schemas do not require.

`packages/transport-http/tests/rust-sequence-parity.test.ts` freezes the observed baseline:

- The committed log `[-0.5, 0.25, 0.5, 1.5]` produces `[0.25, 1.5]`: the negative event and the close fractional event disappear.
- At `2 ** 53`, adding `1` does not change the number. With a one-event read batch, resuming after that event repeatedly emits the same event and cannot settle. The regression reads four repeated frames and then aborts, with an additional deadline to bound the reproduction.
- The TypeScript client already accepts finite negative, fractional, gapped, and wide sequences. Its deduplication compares the numeric high-water mark without requiring an integer step.

The TypeScript source remains unchanged as the frozen reference. Rust corrects the HTTP cursor defect instead of reproducing event loss or a stalled stream. The native integration regression writes the same numeric values through the real SQLite event store, exercises fresh/known/unknown replay, and verifies replay after process restart. The Rust socket protocol test additionally uses a one-event batch to cover the rounding boundary directly.

## Malformed frames

Rust retains strict validation of event fields and rejects nonnumeric or nonfinite sequences. The TypeScript client's permissive malformed-frame handling is a separate baseline difference: it can yield an unusable sequence without advancing its high-water mark. This cursor correction changes valid numeric event handling; it does not broaden Rust's malformed-frame acceptance.
