/**
 * Task kinds are host-chosen strings. The store treats `TaskRecord.kind` as
 * opaque; the only thing that reads it is {@link ExecutorRegistry}, which maps
 * it to the executor that runs the work.
 *
 * TWO PREFIXES ARE RESERVED FOR THE FRAMEWORK: `chat.*` and `agentkit.*`.
 * A host that names its own kind `chat.summarize` today collides with whatever
 * this package adds under that prefix tomorrow, and the collision surfaces as a
 * duplicate-registration throw at boot (best case) or as the framework's
 * executor silently running the host's work (worst). Reserving the namespace up
 * front is cheap; renaming a persisted `kind` after tasks are already in a
 * production queue is not — every queued row carries the old string, and there
 * is no migration that can rewrite work already in flight. Everything else
 * (`notes.reindex`, `acme.billing.sync`) belongs to the host.
 */

/** The kind the chat-turn executor handles — one turn of one conversation. */
export const CHAT_TURN_TASK_KIND = "chat.turn";
