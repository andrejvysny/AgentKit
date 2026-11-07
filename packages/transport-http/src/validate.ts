/**
 * Structural validation of every request body in the contract.
 *
 * DELIBERATELY HAND-WRITTEN, not schema-driven. The TypeBox schemas that define
 * these bodies live in `@agentkit/contracts`, and validating against them needs
 * a validator — Ajv (a `@agentkit/core` dependency) or TypeBox's own
 * `Value.Check` (a `@agentkit/contracts` dependency). Reaching through a
 * dependency for a transitive package is how a lockfile change becomes a
 * runtime crash in someone else's deployment, and this package is meant to be
 * addable with no new dependencies at all. A dozen bodies of flat, mostly
 * optional fields still do not earn one — the checks below are the schemas'
 * rules restated, and the cost of restating them is one function each.
 *
 * The rules are the schemas', restated: required fields must be present and of
 * the declared type, optional fields must match when present, and unknown
 * members ride through (every schema here is a plain `Type.Object`, which does
 * not forbid them — a client sending a field from a later contract version must
 * not be rejected by an older server).
 *
 * THREE EXCEPTIONS. Two are the contract's rather than this file's: a message
 * body's content parts are a CLOSED union, so an unknown part `type` (or an
 * unknown image `source.kind`) is rejected (see {@link checkMessageContent}),
 * and so is an MCP server's transport (see {@link checkTransport}). The third
 * is an MCP server's `resilience` bag (see {@link checkResilience}), which the
 * contract types open but only one package can interpret — an unknown key there
 * is a knob nothing will ever read, and silently storing it tells the operator
 * a timeout is in force that is not.
 *
 * A fourth exception is also the contract's: the AGENTKIT-90 teammate bodies
 * are `additionalProperties: false` objects (see {@link checkAgentDefinition}),
 * whose closedness IS the smuggling gate the DTO docs promise — a member
 * naming a credential VALUE or a foreign scope must be refused at the
 * boundary, not ride through a cast to the service that trusts this layer.
 */
import {
  IMAGE_URL_PATTERN,
  MEDIA_TYPE_PATTERN,
  SubmitMessageRequestSchema,
} from "@agentkit/contracts";
import type {
  AgentDefinitionDto,
  ApplyProposalRequest,
  CreateAgentRequest,
  CreateChatRequest,
  CreateMcpServerRequest,
  CreateProviderRequest,
  ForkChatRequest,
  GrantAllowanceRequest,
  ProposalDecisionRequest,
  RegenerateMessageRequest,
  SetAgentModelRequest,
  SubmitMessageRequest,
  SubmitTeammateTaskRequest,
  UpdateAgentRequest,
  UpdateChatRequest,
  UpdateMcpServerRequest,
  UpdateProviderRequest,
  UpdateSettingsRequest,
} from "@agentkit/contracts";

export type ValidationResult<T> =
  | { ok: true; value: T }
  | { ok: false; detail: string };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** `undefined` passes (the caller decides whether the field was required). */
function checkOptionalString(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (value === undefined || typeof value === "string") return null;
  return `\`${field}\` must be a string.`;
}

function checkRequiredString(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (typeof value === "string") return null;
  return value === undefined
    ? `\`${field}\` is required.`
    : `\`${field}\` must be a string.`;
}

function checkOptionalMetadata(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (value === undefined || isRecord(value)) return null;
  return `\`${field}\` must be an object.`;
}

function first(...issues: (string | null)[]): string | null {
  for (const issue of issues) if (issue !== null) return issue;
  return null;
}

/**
 * `AiMessageContentSchema` restated, by the same hand-written rule this module
 * uses everywhere else (see the header: the schemas live in
 * `@agentkit/contracts`, the validator that would check them against it does
 * not, and this package takes no dependency to get one).
 *
 * Restated FAITHFULLY, which for content parts means restating the part union's
 * closedness too: an unknown `type` is rejected, not waved through. That is the
 * one place the "unknown members ride through" rule above does not apply, and it
 * is the contract's own decision — `AiContentPartSchema` is deliberately closed
 * (`packages/contracts/src/content.ts`), because a server that accepted a part
 * it cannot render would persist content no provider will ever be shown.
 *
 * The two source patterns are the CONTRACT'S OWN, imported rather than
 * restated: they are security rules (a `;` smuggled into a `data` source's
 * media type ends the field early in the `data:` URL an adapter builds; a
 * `file://` image `url` asks the provider client to dereference something in
 * the host's network position), and a security rule with two homes is a
 * security rule that drifts. Everything else in this module is a restatement;
 * these two are not.
 */
const MEDIA_TYPE_RE = new RegExp(MEDIA_TYPE_PATTERN);
const IMAGE_URL_RE = new RegExp(IMAGE_URL_PATTERN);

const IMAGE_DETAILS = new Set(["auto", "low", "high"]);

/** `null` when the part is valid; otherwise why it is not. */
function checkContentPart(part: unknown, index: number): string | null {
  const at = `\`content[${index}]\``;
  if (!isRecord(part)) return `${at} must be an object.`;
  const type = part["type"];
  if (type === "text") {
    return typeof part["text"] === "string"
      ? null
      : `${at}.text must be a string.`;
  }
  if (type === "audio")
    return typeof part["data"] === "string" &&
      (part["format"] === "wav" || part["format"] === "mp3")
      ? null
      : `${at} requires audio data and wav/mp3 format.`;
  if (type !== "image") {
    return `${at}.type must be "text", "image" or "audio".`;
  }
  const source = part["source"];
  if (!isRecord(source)) return `${at}.source must be an object.`;
  const detail = part["detail"];
  if (detail !== undefined && !IMAGE_DETAILS.has(detail as string)) {
    return `${at}.detail must be "auto", "low" or "high".`;
  }
  switch (source["kind"]) {
    case "url":
      if (typeof source["url"] !== "string") {
        return `${at}.source.url must be a string.`;
      }
      return IMAGE_URL_RE.test(source["url"])
        ? null
        : `${at}.source.url must be an http(s) URL.`;
    case "data":
      if (typeof source["base64"] !== "string") {
        return `${at}.source.base64 must be a string.`;
      }
      if (typeof source["mediaType"] !== "string") {
        return `${at}.source.mediaType must be a string.`;
      }
      return MEDIA_TYPE_RE.test(source["mediaType"])
        ? null
        : `${at}.source.mediaType is not a bare type/subtype media type.`;
    case "ref":
      return typeof source["ref"] === "string"
        ? null
        : `${at}.source.ref must be a string.`;
    default:
      return `${at}.source.kind must be "url", "data" or "ref".`;
  }
}

/**
 * A message body: a string, or a non-empty array of content parts.
 *
 * Empty-array rejection is the schema's (`minItems: 1`), and it is not
 * pedantry — `content: []` is not "a message with no body", it is a caller bug;
 * the empty body is the empty STRING, and providers reject the array form
 * outright. Catching it here turns a failed run into a 400.
 */
function checkMessageContent(body: Record<string, unknown>): string | null {
  const value = body["content"];
  if (typeof value === "string") return null;
  if (!Array.isArray(value)) {
    return value === undefined
      ? "`content` is required."
      : "`content` must be a string or an array of content parts.";
  }
  if (value.length === 0) {
    return "`content` must not be an empty array; an empty body is the empty string.";
  }
  for (const [index, part] of value.entries()) {
    const issue = checkContentPart(part, index);
    if (issue !== null) return issue;
  }
  return null;
}

export function validateCreateChatRequest(
  body: Record<string, unknown>,
): ValidationResult<CreateChatRequest> {
  const issue = first(
    checkOptionalString(body, "title"),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as CreateChatRequest };
}

/**
 * The reasoning levels the contract admits, read off the contract's own request
 * schema rather than restated here: a level added to the union becomes
 * admissible through these two validators without this file being touched
 * again, and one removed stops being accepted the same day. Explicit null is
 * refused, as it is on the Rust host — "no opinion" is an omitted field, and a
 * caller that sent `null` said something this server cannot interpret.
 *
 * The empty-set guard is what makes this safe to derive: a contract edit that
 * reshapes the field into something this code no longer understands fails at
 * module load, rather than leaving the validator with no levels to check
 * against and every value admitted.
 */
const reasoningEffortField = (
  SubmitMessageRequestSchema.properties as Record<string, unknown>
)["reasoningEffort"] as { anyOf?: { const?: unknown }[] } | undefined;
const RUN_REASONING_EFFORTS: readonly string[] = (
  reasoningEffortField?.anyOf ?? []
)
  .map((member) => member.const)
  .filter((level): level is string => typeof level === "string");
if (RUN_REASONING_EFFORTS.length === 0) {
  throw new Error(
    "contracts drift: `submitMessage.reasoningEffort` is no longer a closed union of string levels",
  );
}

export function validateSubmitMessageRequest(
  body: Record<string, unknown>,
): ValidationResult<SubmitMessageRequest> {
  const issue = first(
    checkMessageContent(body),
    checkContentSelection(body["contentSelection"]),
    checkOptionalEnum(body, "reasoningEffort", RUN_REASONING_EFFORTS),
    checkOptionalString(body, "model"),
    checkOptionalString(body, "providerId"),
    body["parentMessageId"] === null
      ? null
      : checkOptionalString(body, "parentMessageId"),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as SubmitMessageRequest };
}

export function validateForkChatRequest(
  body: Record<string, unknown>,
): ValidationResult<ForkChatRequest> {
  const issue = checkRequiredString(body, "fromMessageId");
  if (issue !== null) return { ok: false, detail: issue };
  const value = body as ForkChatRequest;
  if (value.fromMessageId.trim() === "") {
    return { ok: false, detail: "`fromMessageId` must not be empty." };
  }
  return { ok: true, value };
}

export function validateProposalDecisionRequest(
  body: Record<string, unknown>,
): ValidationResult<ProposalDecisionRequest> {
  const issue = checkOptionalString(body, "reason");
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as ProposalDecisionRequest };
}

export function validateApplyProposalRequest(
  body: Record<string, unknown>,
): ValidationResult<ApplyProposalRequest> {
  const issue = checkRequiredString(body, "operationId");
  if (issue !== null) return { ok: false, detail: issue };
  const value = body as ApplyProposalRequest;
  if (value.operationId.trim() === "") {
    return { ok: false, detail: "`operationId` must not be empty." };
  }
  return { ok: true, value };
}

function checkOptionalBoolean(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (value === undefined || typeof value === "boolean") return null;
  return `\`${field}\` must be a boolean.`;
}

interface NumberBounds {
  /** Reject a fractional value as well as a non-number. */
  integer?: boolean;
  min?: number;
  max?: number;
}

/**
 * An optional number, optionally bounded.
 *
 * `typeof x === "number"` alone lets `NaN`, `Infinity` and `-1` through, and
 * every number on this surface is a COUNT that something downstream loops on or
 * allocates against. A stored `maxToolIterations: 1e9` is not a config mistake
 * the host notices — it is a run that never ends, written by a request nobody
 * rejected. Finiteness is therefore checked unconditionally; the range is the
 * caller's to name.
 */
function checkOptionalNumber(
  body: Record<string, unknown>,
  field: string,
  bounds: NumberBounds = {},
): string | null {
  const value = body[field];
  if (value === undefined) return null;
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return `\`${field}\` must be a finite number.`;
  }
  if (bounds.integer === true && !Number.isInteger(value)) {
    return `\`${field}\` must be an integer.`;
  }
  if (bounds.min !== undefined && value < bounds.min) {
    return `\`${field}\` must be at least ${bounds.min}.`;
  }
  if (bounds.max !== undefined && value > bounds.max) {
    return `\`${field}\` must be at most ${bounds.max}.`;
  }
  return null;
}

/** A `Record<string, string>` — the shape of every header/env/alias bag here. */
function checkOptionalStringMap(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (value === undefined) return null;
  if (!isRecord(value)) return `\`${field}\` must be an object.`;
  for (const [key, entry] of Object.entries(value)) {
    if (typeof entry !== "string") {
      return `\`${field}.${key}\` must be a string.`;
    }
  }
  return null;
}

/** A member of a closed string union, when present. */
function checkOptionalEnum(
  body: Record<string, unknown>,
  field: string,
  allowed: readonly string[],
): string | null {
  const value = body[field];
  if (value === undefined) return null;
  if (typeof value !== "string" || !allowed.includes(value)) {
    return `\`${field}\` must be one of: ${allowed.join(", ")}.`;
  }
  return null;
}

/** Same, required. */
function checkRequiredEnum(
  body: Record<string, unknown>,
  field: string,
  allowed: readonly string[],
): string | null {
  if (body[field] === undefined) return `\`${field}\` is required.`;
  return checkOptionalEnum(body, field, allowed);
}

/** A required string that is not blank — an id nobody can look anything up by. */
function checkRequiredNonEmptyString(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const issue = checkRequiredString(body, field);
  if (issue !== null) return issue;
  return (body[field] as string).trim() === ""
    ? `\`${field}\` must not be empty.`
    : null;
}

const RISK_LEVELS = ["low", "medium", "high", "destructive"] as const;
const CONTEXT_SIZE_PREFERENCES = ["small", "medium", "large"] as const;
const WRITE_POLICY_MODES = [
  "auto_readonly_confirm_writes",
  "confirm_all_writes",
  "auto_all",
] as const;
const TOOL_CALLING_MODES = ["auto", "on", "off"] as const;

/**
 * Highest `maxToolIterations` this surface will store.
 *
 * The number bounds one turn's tool loop, and every iteration is a provider
 * round trip plus a tool execution under the run's lease. 64 is far past any
 * real agent loop and far short of "this run will still be going tomorrow";
 * a host that genuinely wants more sets it on its own settings port, which is
 * not a stranger's HTTP request.
 */
const MAX_TOOL_ITERATIONS_CEILING = 64;

/**
 * A provider id a caller may choose.
 *
 * Constrained because the id is not just a primary key: `providerSecretRef`
 * turns it into `provider/<id>/api-key`, the NAME a `SecretStore` files the
 * credential under. A path-namespaced store (a directory, a KV prefix, a
 * Vault mount) handed `../../root` or an id with a NUL in it writes outside the
 * namespace it was given — so the grammar is the conservative one that cannot
 * mean anything in a path, and it is enforced here rather than in the store,
 * which sees a ref and cannot know an untrusted id went into it.
 */
const PROVIDER_ID_RE = /^[A-Za-z0-9._-]{1,64}$/;

export function validateUpdateChatRequest(
  body: Record<string, unknown>,
): ValidationResult<UpdateChatRequest> {
  const issue = first(
    checkOptionalString(body, "title"),
    checkOptionalMetadata(body, "metadata"),
    checkOptionalBoolean(body, "archived"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as UpdateChatRequest };
}

export function validateRegenerateMessageRequest(
  body: Record<string, unknown>,
): ValidationResult<RegenerateMessageRequest> {
  const issue = first(
    checkContentSelection(body["contentSelection"]),
    checkOptionalEnum(body, "reasoningEffort", RUN_REASONING_EFFORTS),
    checkOptionalString(body, "model"),
    checkOptionalString(body, "providerId"),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as RegenerateMessageRequest };
}

/**
 * `null` when the caller named no id (the route mints one) or named a legal
 * one. Checked against the TRIMMED value, because that is what the route uses.
 *
 * An id that is only dots is refused on top of the grammar: `..` carries no
 * illegal character, but `provider/../api-key` still resolves one directory
 * above the namespace the secret store was handed.
 */
function checkProviderId(value: unknown): string | null {
  if (value === undefined) return null;
  if (typeof value !== "string") return null; // reported by checkOptionalString
  const trimmed = value.trim();
  if (trimmed === "") return null; // absent in effect; the route mints one
  return providerIdIssue(trimmed);
}

/**
 * The grammar as a reusable verdict, so the route that MINTS a secret ref from
 * a path parameter (update) applies the same rule as the create body — a
 * pre-existing row with an id outside the grammar must not keep the namespace
 * escape alive on every later PATCH.
 */
export function providerIdIssue(id: string): string | null {
  if (!PROVIDER_ID_RE.test(id) || /^\.+$/.test(id)) {
    return `\`id\` must match ${PROVIDER_ID_RE.source} and must not be only dots.`;
  }
  return null;
}

export function validateCreateProviderRequest(
  body: Record<string, unknown>,
): ValidationResult<CreateProviderRequest> {
  const issue = first(
    checkOptionalString(body, "id"),
    checkProviderId(body["id"]),
    checkRequiredNonEmptyString(body, "label"),
    checkRequiredNonEmptyString(body, "kind"),
    checkRequiredNonEmptyString(body, "baseUrl"),
    checkRequiredNonEmptyString(body, "defaultModel"),
    checkOptionalBoolean(body, "enabled"),
    checkOptionalString(body, "apiKey"),
    checkOptionalStringMap(body, "extraHeaders"),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as CreateProviderRequest };
}

export function validateUpdateProviderRequest(
  body: Record<string, unknown>,
): ValidationResult<UpdateProviderRequest> {
  const issue = first(
    checkOptionalString(body, "label"),
    checkOptionalString(body, "kind"),
    checkOptionalString(body, "baseUrl"),
    checkOptionalString(body, "defaultModel"),
    checkOptionalBoolean(body, "enabled"),
    checkOptionalString(body, "apiKey"),
    checkOptionalStringMap(body, "extraHeaders"),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as UpdateProviderRequest };
}

/**
 * A settings patch. Every field is optional; the three unions are CLOSED and
 * checked, because they are the fields a wrong value silently changes behaviour
 * with — a `writePolicyMode` of `"auto-all"` (a hyphen) would be stored, read
 * back as a mode nothing matches, and quietly fall through to confirming every
 * write forever.
 */
export function validateUpdateSettingsRequest(
  body: Record<string, unknown>,
): ValidationResult<UpdateSettingsRequest> {
  const issue = first(
    checkOptionalString(body, "defaultProviderId"),
    checkOptionalString(body, "defaultModel"),
    checkOptionalEnum(body, "contextSizePreference", CONTEXT_SIZE_PREFERENCES),
    checkOptionalEnum(body, "writePolicyMode", WRITE_POLICY_MODES),
    checkOptionalBoolean(body, "allowRawToolData"),
    checkOptionalNumber(body, "maxToolIterations", {
      integer: true,
      min: 1,
      max: MAX_TOOL_ITERATIONS_CEILING,
    }),
    checkOptionalEnum(body, "toolCalling", TOOL_CALLING_MODES),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as UpdateSettingsRequest };
}

export function validateGrantAllowanceRequest(
  body: Record<string, unknown>,
): ValidationResult<GrantAllowanceRequest> {
  // No `chatId`: the chat is a path parameter on all three allowance routes,
  // and a body that also carried one would be a second answer to a question the
  // URL has already settled — and the only one the authorizer never saw.
  const issue = first(
    checkRequiredNonEmptyString(body, "toolName"),
    checkRequiredNonEmptyString(body, "proposalKind"),
    checkRequiredEnum(body, "maxRisk", RISK_LEVELS),
  );
  if (issue !== null) return { ok: false, detail: issue };
  // Rebuilt field by field, not `body as ...`: the route spreads this value into
  // the port call, and a stray body member — a `chatId` — riding through the
  // cast would override the path parameter the authorizer already ruled on.
  return {
    ok: true,
    value: {
      toolName: body["toolName"] as string,
      proposalKind: body["proposalKind"] as string,
      maxRisk: body["maxRisk"] as GrantAllowanceRequest["maxRisk"],
    },
  };
}

/**
 * `McpTransportDtoSchema` restated — and, like the content-part union, restated
 * as the CLOSED union it is: an unknown `kind` is rejected rather than waved
 * through.
 *
 * That is the one place this module's "unknown members ride through" rule does
 * not apply, and for the same reason it does not apply to content parts: a
 * transport nothing can connect is a record that fails at the worst possible
 * moment, on the first run that stages the server's tools, with nothing left in
 * the request to blame it on.
 */
function checkTransport(value: unknown, required: boolean): string | null {
  if (value === undefined) {
    return required ? "`transport` is required." : null;
  }
  if (!isRecord(value)) return "`transport` must be an object.";
  const nested = value as Record<string, unknown>;
  switch (nested["kind"]) {
    case "stdio": {
      const issue = first(
        checkRequiredNonEmptyString(nested, "command"),
        checkOptionalStringMap(nested, "env"),
      );
      if (issue !== null) return `\`transport\`: ${issue}`;
      const args = nested["args"];
      if (args === undefined) return null;
      if (!Array.isArray(args) || args.some((a) => typeof a !== "string")) {
        return "`transport.args` must be an array of strings.";
      }
      return null;
    }
    case "http": {
      const issue = first(
        checkRequiredNonEmptyString(nested, "url"),
        checkOptionalStringMap(nested, "headers"),
      );
      return issue === null ? null : `\`transport\`: ${issue}`;
    }
    default:
      return '`transport.kind` must be "stdio" or "http".';
  }
}

/**
 * The tool NAMESPACE grammar, restated from `McpServerDtoSchema.alias`'s own
 * description (`^[a-z][a-z0-9-]*$`) — the same rule `normalizeServerAlias` in
 * `@agentkit/mcp-client` enforces.
 *
 * Restated here, and not merely left to that package, because of WHEN it would
 * otherwise be enforced: the alias is checked at `McpClientManager`
 * construction, so one row stored through this route with a capital letter in
 * it throws `mcp_invalid_alias` while the HOST is being wired, and every chat —
 * not just the one that touches this server — stops working until somebody
 * edits the database by hand. A 400 on the request that created the row is the
 * same rule enforced where it can still be corrected.
 */
const MCP_ALIAS_RE = /^[a-z][a-z0-9-]*$/;

/**
 * `McpResilienceOptions` (`@agentkit/mcp-client`) restated as bounds.
 *
 * Every field is a duration a session waits on, a count of attempts it makes,
 * or the factor those attempts grow by. Unbounded, `requestTimeoutMs: 1e12`
 * parks a run's lease for a lifetime and `maxConnectAttempts: 1e6` turns one
 * turn into an unkillable spawn loop — with nothing in the request left to
 * blame it on, because the row is only read at connect time.
 *
 * Ten minutes is the ceiling for a duration (longer than any host hook budget)
 * and 20 for an attempt count; the backoff factor is the one non-integer, since
 * 1.5 is a legitimate ramp.
 */
const RESILIENCE_MS_FIELDS = [
  "requestTimeoutMs",
  "connectTimeoutMs",
  "connectBackoffBaseMs",
  "connectBackoffMaxMs",
  "circuitOpenMs",
] as const;
const RESILIENCE_ATTEMPT_FIELDS = [
  "maxConnectAttempts",
  "reconnectMaxAttempts",
] as const;
const RESILIENCE_MAX_MS = 600_000;
const RESILIENCE_MAX_ATTEMPTS = 20;

/**
 * `null` when the bag is absent or every knob in it is one this client has and
 * a value it can act on. UNKNOWN KEYS ARE REJECTED — the one place a bag on
 * this surface is closed, and for the same reason the transport union is: a
 * misspelled `requestTimeouMs` is silently ignored forever, and the operator
 * who set it believes the timeout they configured is in force.
 */
function checkResilience(value: unknown): string | null {
  if (value === undefined) return null;
  if (!isRecord(value)) return "`resilience` must be an object.";
  const known = new Set<string>([
    ...RESILIENCE_MS_FIELDS,
    ...RESILIENCE_ATTEMPT_FIELDS,
    "reconnectBackoffFactor",
    "retryTimeouts",
  ]);
  for (const key of Object.keys(value)) {
    if (!known.has(key)) {
      return `\`resilience.${key}\` is not a known resilience option.`;
    }
  }
  const issue = first(
    ...RESILIENCE_MS_FIELDS.map((field) =>
      checkOptionalNumber(value, field, {
        integer: true,
        min: 0,
        max: RESILIENCE_MAX_MS,
      }),
    ),
    ...RESILIENCE_ATTEMPT_FIELDS.map((field) =>
      checkOptionalNumber(value, field, {
        integer: true,
        min: 1,
        max: RESILIENCE_MAX_ATTEMPTS,
      }),
    ),
    checkOptionalNumber(value, "reconnectBackoffFactor", {
      min: 1,
      max: RESILIENCE_MAX_ATTEMPTS,
    }),
    checkOptionalBoolean(value, "retryTimeouts"),
  );
  return issue === null ? null : `\`resilience\`: ${issue}`;
}

/** `null` when the alias is absent or matches {@link MCP_ALIAS_RE}. */
function checkMcpAlias(body: Record<string, unknown>): string | null {
  const value = body["alias"];
  if (value === undefined) return null;
  const issue = checkRequiredNonEmptyString(body, "alias");
  if (issue !== null) return issue;
  return MCP_ALIAS_RE.test((value as string).trim())
    ? null
    : `\`alias\` must match ${MCP_ALIAS_RE.source}.`;
}

export function validateCreateMcpServerRequest(
  body: Record<string, unknown>,
): ValidationResult<CreateMcpServerRequest> {
  const issue = first(
    checkRequiredNonEmptyString(body, "alias"),
    checkMcpAlias(body),
    checkTransport(body["transport"], true),
    checkOptionalStringMap(body, "secretRefs"),
    checkOptionalBoolean(body, "enabled"),
    checkOptionalStringMap(body, "toolAliases"),
    checkResilience(body["resilience"]),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as CreateMcpServerRequest };
}

export function validateUpdateMcpServerRequest(
  body: Record<string, unknown>,
): ValidationResult<UpdateMcpServerRequest> {
  const alias = body["alias"];
  const issue = first(
    alias === undefined ? null : checkRequiredNonEmptyString(body, "alias"),
    checkMcpAlias(body),
    checkTransport(body["transport"], false),
    checkOptionalStringMap(body, "secretRefs"),
    checkOptionalBoolean(body, "enabled"),
    checkOptionalStringMap(body, "toolAliases"),
    checkResilience(body["resilience"]),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as UpdateMcpServerRequest };
}

function checkContentSelection(value: unknown): string | null {
  if (value === undefined) return null;
  if (!isRecord(value)) return "contentSelection must be an object.";
  const template = value["template"];
  if (
    template !== undefined &&
    (!isRecord(template) ||
      typeof template["id"] !== "string" ||
      typeof template["revision"] !== "string" ||
      !isRecord(template["arguments"]))
  )
    return "Invalid template selection.";
  const skills = value["skills"];
  if (
    skills !== undefined &&
    (!Array.isArray(skills) ||
      skills.length > 20 ||
      skills.some(
        (item) =>
          !isRecord(item) ||
          typeof item["id"] !== "string" ||
          typeof item["revision"] !== "string",
      ))
  )
    return "Invalid skill selection.";
  return null;
}

// ---------------------------------------------------------------------------
// AGENTKIT-90 — teammate bodies
//
// Every object below is `additionalProperties: false` in the contract, and
// the closedness is load-bearing: the definition and the task-profile are
// the smuggling gates their DTO docs promise, so an unknown member here is
// refused, not passed through. The rest of this module's "unknown members
// ride through" rule stays exactly where it was.
// ---------------------------------------------------------------------------

/** The largest integer the wire's `maximum` fields admit (2^53 − 1). */
const MAX_SAFE = 9_007_199_254_740_991;

/** Unknown members of a closed object, named. */
function checkClosedMembers(
  value: Record<string, unknown>,
  allowed: readonly string[],
  label: string,
): string | null {
  const extras = Object.keys(value).filter((key) => !allowed.includes(key));
  if (extras.length === 0) return null;
  return `\`${label}\` carries unknown ${extras.length === 1 ? "member" : "members"}: ${extras.join(", ")}.`;
}

/** A string array, required or optional, by the same rule the MCP bodies use. */
function checkStringArray(
  value: unknown,
  label: string,
  required: boolean,
): string | null {
  if (value === undefined) {
    return required ? `\`${label}\` is required.` : null;
  }
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string")) {
    return `\`${label}\` must be an array of strings.`;
  }
  return null;
}

/** A required finite integer, non-negative — optimistic-concurrency tokens. */
function checkRequiredRevision(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) {
    return `\`${field}\` must be a non-negative integer.`;
  }
  return null;
}

const BUDGET_FIELDS = [
  "maxToolIterations",
  "maxToolCallsPerIteration",
  "maxOutputTokens",
  "attemptBudgetMs",
] as const;

/** `AgentBudgetsDto` — four optional bounded integers, and nothing else. */
function checkBudgets(value: unknown, label: string): string | null {
  if (value === undefined) return null;
  if (!isRecord(value)) return `\`${label}\` must be an object.`;
  const closed = checkClosedMembers(value, BUDGET_FIELDS, label);
  if (closed !== null) return closed;
  for (const field of BUDGET_FIELDS) {
    const issue = checkOptionalNumber(value, field, {
      integer: true,
      min: 1,
      max: MAX_SAFE,
    });
    if (issue !== null) return `\`${label}\`: ${issue}`;
  }
  return null;
}

/** `AgentSkillSelectionDto[]` — closed `{ id, revision }` rows. */
function checkSkillSelection(value: unknown, label: string): string | null {
  if (!Array.isArray(value)) return `\`${label}\` must be an array.`;
  for (const [index, item] of value.entries()) {
    if (!isRecord(item)) {
      return `\`${label}[${index}]\` must be an object.`;
    }
    const issue = first(
      checkClosedMembers(item, ["id", "revision"], `${label}[${index}]`),
      checkRequiredNonEmptyString(item, "id"),
      checkRequiredNonEmptyString(item, "revision"),
    );
    if (issue !== null) return issue;
  }
  return null;
}

const DEFINITION_FIELDS = [
  "name",
  "avatar",
  "purpose",
  "instructions",
  "resources",
  "skills",
  "model",
  "reasoningEffort",
  "permissionMode",
  "allowedTools",
  "budgets",
  "secretRefs",
] as const;

/**
 * `AgentDefinitionDtoSchema` restated, closed. The closedness is the point:
 * there is no field anywhere that could carry a credential VALUE — only
 * `secretRefs`, which carries REFERENCES — and an envelope that tries to
 * smuggle one under another name is refused here, before the service ever
 * sees the request.
 */
function checkAgentDefinition(body: Record<string, unknown>): string | null {
  const definition = body["definition"];
  if (!isRecord(definition)) return "`definition` must be an object.";
  const issue = first(
    checkClosedMembers(definition, DEFINITION_FIELDS, "definition"),
    checkRequiredNonEmptyString(definition, "name"),
    checkOptionalString(definition, "avatar"),
    checkRequiredNonEmptyString(definition, "purpose"),
    checkRequiredNonEmptyString(definition, "instructions"),
    checkStringArray(definition["resources"], "definition.resources", true),
    checkSkillSelection(definition["skills"], "definition.skills"),
    checkOptionalString(definition, "model"),
    checkOptionalString(definition, "reasoningEffort"),
    checkRequiredNonEmptyString(definition, "permissionMode"),
    checkStringArray(
      definition["allowedTools"],
      "definition.allowedTools",
      true,
    ),
    definition["budgets"] === undefined
      ? "`definition.budgets` is required (an empty object is the unset answer)."
      : checkBudgets(definition["budgets"], "definition.budgets"),
    checkStringArray(definition["secretRefs"], "definition.secretRefs", true),
  );
  return issue;
}

const TASK_PROFILE_FIELDS = [
  "providerId",
  "model",
  "tools",
  "requiresVision",
  "requiresContinuation",
  "permissionMode",
  "budgets",
  "truncation",
] as const;

/**
 * `TaskProfileOverridesDtoSchema` restated, closed. Narrowing-only is the
 * SERVICE's rule (tools inside the agent's ceiling, budgets under its
 * ceilings); what the wire guarantees is that the envelope names nothing
 * outside this vocabulary — no secret value, no credential, no grant.
 */
function checkTaskProfile(value: unknown): string | null {
  if (value === undefined) return null;
  if (!isRecord(value)) return "`taskProfile` must be an object.";
  const issue = first(
    checkClosedMembers(value, TASK_PROFILE_FIELDS, "taskProfile"),
    checkOptionalString(value, "providerId"),
    checkOptionalString(value, "model"),
    checkStringArray(value["tools"], "taskProfile.tools", false),
    checkOptionalBoolean(value, "requiresVision"),
    checkOptionalBoolean(value, "requiresContinuation"),
    checkOptionalString(value, "permissionMode"),
    checkBudgets(value["budgets"], "taskProfile.budgets"),
    checkTruncation(value["truncation"]),
  );
  return issue;
}

/** `truncation` is one literal decision or absent — the on-record drop gate. */
function checkTruncation(value: unknown): string | null {
  if (value === undefined) return null;
  if (!isRecord(value)) return "`taskProfile.truncation` must be an object.";
  const closed = checkClosedMembers(value, ["mode"], "taskProfile.truncation");
  if (closed !== null) return closed;
  if (value["mode"] !== "explicit") {
    return '`taskProfile.truncation.mode` must be "explicit".';
  }
  return null;
}

const CONTEXT_UNIT_KINDS = ["text", "toolPair"] as const;
const SELECTED_SOURCE_KINDS = ["skill", "resource"] as const;

/** `SelectedSourceDto[]` — closed rows of closed units. */
function checkProjectSources(value: unknown): string | null {
  if (value === undefined) return null;
  if (!Array.isArray(value)) return "`projectSources` must be an array.";
  for (const [index, source] of value.entries()) {
    if (!isRecord(source)) {
      return `\`projectSources[${index}]\` must be an object.`;
    }
    const issue = first(
      checkClosedMembers(
        source,
        ["id", "kind", "units"],
        `projectSources[${index}]`,
      ),
      checkRequiredNonEmptyString(source, "id"),
      checkRequiredEnum(source, "kind", SELECTED_SOURCE_KINDS),
    );
    if (issue !== null) return issue;
    if (!Array.isArray(source["units"])) {
      return `\`projectSources[${index}].units\` must be an array.`;
    }
    for (const [unitIndex, unit] of source["units"].entries()) {
      if (!isRecord(unit)) {
        return `\`projectSources[${index}].units[${unitIndex}]\` must be an object.`;
      }
      const unitIssue = first(
        checkClosedMembers(
          unit,
          ["kind", "text", "citations", "required"],
          `projectSources[${index}].units[${unitIndex}]`,
        ),
        checkRequiredEnum(unit, "kind", CONTEXT_UNIT_KINDS),
        checkRequiredString(unit, "text"),
        checkStringArray(
          unit["citations"],
          `projectSources[${index}].units[${unitIndex}].citations`,
          true,
        ),
      );
      if (unitIssue !== null) return unitIssue;
      if (typeof unit["required"] !== "boolean") {
        return `\`projectSources[${index}].units[${unitIndex}].required\` must be a boolean.`;
      }
    }
  }
  return null;
}

/** `ApprovalGrantRefDto[]` — closed `{ sessionId, grantId, digest }` rows. */
function checkApprovals(value: unknown): string | null {
  if (value === undefined) return null;
  if (!Array.isArray(value)) return "`approvals` must be an array.";
  for (const [index, grant] of value.entries()) {
    if (!isRecord(grant)) {
      return `\`approvals[${index}]\` must be an object.`;
    }
    const issue = first(
      checkClosedMembers(
        grant,
        ["sessionId", "grantId", "digest"],
        `approvals[${index}]`,
      ),
      checkRequiredNonEmptyString(grant, "sessionId"),
      checkRequiredNonEmptyString(grant, "grantId"),
      checkRequiredNonEmptyString(grant, "digest"),
    );
    if (issue !== null) return issue;
  }
  return null;
}

export function validateCreateAgentRequest(
  body: Record<string, unknown>,
): ValidationResult<CreateAgentRequest> {
  // No `scope`, no `owner`, no `actor`: the agent's scope and owner are the
  // server's answer from the authenticated actor, and the closed body means a
  // request cannot even try to name them.
  const issue = first(
    checkClosedMembers(body, ["definition"], "body"),
    checkAgentDefinition(body),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return {
    ok: true,
    value: { definition: body["definition"] as AgentDefinitionDto },
  };
}

export function validateUpdateAgentRequest(
  body: Record<string, unknown>,
): ValidationResult<UpdateAgentRequest> {
  const issue = first(
    checkClosedMembers(body, ["expectedRevision", "definition"], "body"),
    checkRequiredRevision(body, "expectedRevision"),
    checkAgentDefinition(body),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return {
    ok: true,
    value: {
      expectedRevision: body["expectedRevision"] as number,
      definition: body["definition"] as AgentDefinitionDto,
    },
  };
}

export function validateSetAgentModelRequest(
  body: Record<string, unknown>,
): ValidationResult<SetAgentModelRequest> {
  const model = body["model"];
  const reasoningEffort = body["reasoningEffort"];
  const issue = first(
    checkClosedMembers(
      body,
      ["expectedRevision", "model", "reasoningEffort"],
      "body",
    ),
    checkRequiredRevision(body, "expectedRevision"),
    model === null || model === undefined || typeof model === "string"
      ? null
      : "`model` must be a string or null.",
    reasoningEffort === null ||
      reasoningEffort === undefined ||
      typeof reasoningEffort === "string"
      ? null
      : "`reasoningEffort` must be a string or null.",
  );
  if (issue !== null) return { ok: false, detail: issue };
  return {
    ok: true,
    value: {
      expectedRevision: body["expectedRevision"] as number,
      ...(typeof model === "string" || model === null ? { model } : {}),
      ...(typeof reasoningEffort === "string" || reasoningEffort === null
        ? { reasoningEffort }
        : {}),
    },
  };
}

export function validateSubmitTeammateTaskRequest(
  body: Record<string, unknown>,
): ValidationResult<SubmitTeammateTaskRequest> {
  // No `agentId`: the agent is a path parameter — the one the authorizer
  // ruled on, and the only admission identity that counts.
  const issue = first(
    checkClosedMembers(
      body,
      [
        "taskId",
        "admissionKey",
        "kind",
        "payload",
        "priority",
        "dependsOn",
        "parentTaskId",
        "taskProfile",
        "projectSources",
        "approvals",
      ],
      "body",
    ),
    checkOptionalString(body, "taskId"),
    checkRequiredNonEmptyString(body, "admissionKey"),
    checkRequiredNonEmptyString(body, "kind"),
    isRecord(body["payload"]) ? null : "`payload` must be an object.",
    checkOptionalNumber(body, "priority", {
      integer: true,
      min: -MAX_SAFE,
      max: MAX_SAFE,
    }),
    checkStringArray(body["dependsOn"], "dependsOn", false),
    checkOptionalString(body, "parentTaskId"),
    checkTaskProfile(body["taskProfile"]),
    checkProjectSources(body["projectSources"]),
    checkApprovals(body["approvals"]),
  );
  if (issue !== null) return { ok: false, detail: issue };
  // Rebuilt field by field, the `grantAllowance` rule: the value goes to the
  // admission service, and a stray member riding through a cast would be a
  // second answer to a question the closed schema already settled.
  return {
    ok: true,
    value: {
      admissionKey: body["admissionKey"] as string,
      kind: body["kind"] as string,
      payload: body["payload"] as Record<string, unknown>,
      ...(body["taskId"] === undefined
        ? {}
        : { taskId: body["taskId"] as string }),
      ...(body["priority"] === undefined
        ? {}
        : { priority: body["priority"] as number }),
      ...(body["dependsOn"] === undefined
        ? {}
        : { dependsOn: body["dependsOn"] as string[] }),
      ...(body["parentTaskId"] === undefined
        ? {}
        : { parentTaskId: body["parentTaskId"] as string }),
      ...(body["taskProfile"] === undefined
        ? {}
        : {
            taskProfile: body[
              "taskProfile"
            ] as SubmitTeammateTaskRequest["taskProfile"],
          }),
      ...(body["projectSources"] === undefined
        ? {}
        : {
            projectSources: body[
              "projectSources"
            ] as SubmitTeammateTaskRequest["projectSources"],
          }),
      ...(body["approvals"] === undefined
        ? {}
        : {
            approvals: body[
              "approvals"
            ] as SubmitTeammateTaskRequest["approvals"],
          }),
    },
  };
}
