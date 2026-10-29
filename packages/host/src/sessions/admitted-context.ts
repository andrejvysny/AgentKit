import { AgentKitHostError } from "../errors.js";
import {
  readSessionDescriptor,
  SESSION_METADATA_KEY,
  type AgentSessionDescriptor,
  type SessionContextOptions,
} from "./descriptor.js";

/** Null pins the absence of overrides; an absent key identifies legacy turns. */
export function admittedContext(
  session: AgentSessionDescriptor,
): SessionContextOptions | null {
  return session.context === undefined
    ? null
    : structuredClone(session.context);
}

/** Future session configuration must not change a turn's admitted budget. */
export function sessionWithAdmittedContext(
  session: AgentSessionDescriptor,
  payload: Record<string, unknown>,
): AgentSessionDescriptor {
  if (!("sessionContext" in payload)) return session;
  const { context: _context, ...base } = session;
  const context = payload["sessionContext"];
  if (context === null) return base;
  const pinned =
    context === undefined
      ? undefined
      : readSessionDescriptor({
          [SESSION_METADATA_KEY]: { ...base, context },
        });
  if (pinned === undefined) {
    throw new AgentKitHostError(
      "session_context_invalid",
      "The turn's admitted context snapshot is unreadable.",
    );
  }
  return pinned;
}
