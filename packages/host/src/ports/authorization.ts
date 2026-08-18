/**
 * Whether the caller may do this at all.
 *
 * Separate from the write policy on purpose: the write policy answers "may this
 * apply without a confirmation?", authorization answers "is this actor allowed
 * near this resource?". A desktop host wires a permissive implementation; a
 * multi-tenant service does not.
 */
export interface AuthorizationSubject {
  userId?: string;
  roles?: string[];
  metadata?: Record<string, unknown>;
}

export interface AuthorizationResource {
  /** Host-defined resource class, e.g. "chat", "run", "document". */
  kind: string;
  id?: string;
  scopeId?: string;
}

export interface AuthorizationRequest {
  subject: AuthorizationSubject;
  /** Host-defined verb, e.g. "chat.submit", "proposal.approve". */
  action: string;
  resource: AuthorizationResource;
}

export interface AuthorizationDecision {
  allowed: boolean;
  /** Why it was denied — surfaced to the caller, so make it sayable out loud. */
  reason?: string;
}

export interface AuthorizationPort {
  authorize(request: AuthorizationRequest): Promise<AuthorizationDecision>;
}
