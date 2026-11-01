/**
 * `NativeManagedContent` — the typed wrapper over the Rust managed-content
 * port surface: `promptTemplates.*`, `skills.*`, `connections.*` and
 * `runContent.*`.
 *
 * Like every other store-port wrapper here, this module moves JSON across
 * the boundary and projects records; validation, ordering, CAS, hashing,
 * redaction and tombstone semantics all live in the engine
 * (`rust/crates/store/src/managed_content.rs`, reached through
 * `rust/crates/napi/src/store_managed_content.rs`). The only client-side
 * logic is the strict wire shape the binding itself enforces: an argument
 * of the wrong JS type is refused HERE with the same stable code the
 * binding would report (`invalid_input`), so a malformed call never pays
 * the FFI round-trip. Nothing here decides a state question.
 *
 * DOCUMENTED GAP (verbatim reduction, `docs/rust/parity.md`): MCP OAuth is
 * not available in the native binding
 * (`docs/rust/native-binding-milestone.md` "Open gaps": no OAuth, deny-all
 * `secretRefs`). The connection lifecycle legs — `connect`, `reconnect`,
 * `test`, `disconnect`, and the generic `action` (the REST
 * `connectionAction`) — therefore have no engine arm: every one of them is
 * carried across the port surface and refused there with
 * `not_implemented`, exactly as the HTTP lane answers REST
 * `connectionAction` with 501. The wrapper forwards honestly instead of
 * pretending to connect.
 */
import type {
  ConnectionRecord,
  PromptTemplate,
  PromptTemplateSelection,
  RunContentSelection,
  SkillSelection,
} from "@agentkit/contracts";
import type { NativeStore as RawNativeStore } from "../generated/index.js";
import {
  createNativeStorePort,
  NativeAgentError,
  type NativeStore,
} from "./index.js";
import type { NativeStorePort } from "./store-transaction.js";

// ---------------------------------------------------------------------------
// Record shapes: what the engine's port surface answers with.
// ---------------------------------------------------------------------------

/** `promptTemplates.render` output: the frozen `{provenance, messages}`. */
export interface RenderedPromptTemplate {
  provenance: {
    id: string;
    revision: string;
    origin?: PromptTemplate["origin"];
  };
  messages: PromptTemplate["messages"];
}

/** One `skills.list`/`get`/`publish` catalog entry: frontmatter metadata
 * without the document body (the body stays in the store, keyed by its
 * content-hash revision). */
export interface ManagedSkillEntry {
  id: string;
  revision: string;
  sourceId: string;
  path: string;
  name: string;
  description: string;
  connectorRequirements: string[];
}

/** A non-secret connection draft for `connections.save`: exactly the stored
 * record minus the engine-owned lifecycle fields. The engine normalizes
 * `config.alias` and stamps `status: "disconnected"`. */
export type ManagedConnectionDraft = Omit<
  ConnectionRecord,
  "status" | "errorCode"
>;

/** One context block from `runContent.resolve` (the frozen
 * `AiPromptContextBlock` shape). */
export interface RunContentBlock {
  id: string;
  title: string;
  content: string;
  priority: number;
}

/** One skill `runContent.resolve` skipped under `tolerateUnavailable`. */
export interface UnavailableRunSkill {
  id: string;
  reason: string;
}

/** `runContent.resolve` output. */
export interface RunContentResolution {
  blocks: RunContentBlock[];
  messages: PromptTemplate["messages"];
  unavailable: UnavailableRunSkill[];
}

/** One `runContent.listResources` row: the record
 * `runContent.recordResource` stored per activated-skill read. */
export interface RunContentResourceRecord {
  skill: SkillSelection;
  path: string;
  revision: string;
}

/** `connections.save` returns the stored record with every
 * credential-shaped transport value (`env`/`headers`) redacted to `***`. */
export type RedactedConnectionRecord = ConnectionRecord;

// ---------------------------------------------------------------------------
// Client-side wire shape (mirrors the binding's arm validators, same code).
// ---------------------------------------------------------------------------

function invalid(message: string): never {
  throw new NativeAgentError("invalid_input", message);
}

/** `ports::text` mirror: required string, refused before the FFI. */
function text(value: unknown, field: string): string {
  if (typeof value !== "string")
    invalid(`${field} must be a string`);
  return value as string;
}

function requiredText(value: unknown, field: string): string {
  if (typeof value !== "string" || value.length === 0)
    invalid(`${field} must be a non-empty string`);
  return value as string;
}

function optionalText(value: unknown, field: string): string | undefined {
  if (value === undefined || value === null) return undefined;
  return text(value, field);
}

function object(value: unknown, field: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value))
    invalid(`${field} must be an object`);
  return value as Record<string, unknown>;
}

function strings(value: unknown, field: string): string[] {
  if (!Array.isArray(value)) invalid(`${field} must be an array`);
  return (value as unknown[]).map((item, index) =>
    text(item, `${field}[${index}]`),
  );
}

/** The engine's `required_text` on `source` also refuses `':'` (the id
 * separator `source:path`). */
function skillSource(value: unknown): string {
  const source = requiredText(value, "source");
  if (source.includes(":"))
    invalid("source must be a non-empty string without ':'");
  return source;
}

/** `validate_prompt_template`'s shape class, cheaply: identity, message
 * count/shape, and the object-schema head. Unknown variables, expression
 * leftovers and schema compilation stay the engine's (`invalid_input`). */
function validateTemplateShape(template: unknown): PromptTemplate {
  const value = object(template, "template");
  requiredText(value["id"], "template.id");
  requiredText(value["revision"], "template.revision");
  text(value["description"], "template.description");
  object(value["argumentsSchema"], "template.argumentsSchema");
  const messages = value["messages"];
  if (!Array.isArray(messages) || messages.length === 0 || messages.length > 100)
    invalid("template.messages must be an array of 1 to 100 messages");
  for (const message of messages as unknown[]) {
    const entry = object(message, "template.messages[]");
    if (
      entry["role"] !== "system" &&
      entry["role"] !== "user" &&
      entry["role"] !== "assistant"
    )
      invalid("template.messages[].role must be system, user, or assistant");
    text(entry["content"], "template.messages[].content");
  }
  return template as PromptTemplate;
}

/** The connection-arm checks the engine makes BEFORE any state question:
 * identities, revision integer, config object, optional workspace, and the
 * `expectedRevision` wire union. Alias normalization, uniqueness, the
 * immutability comparison, the transport validator and the deny-all
 * `secretRefs` refusal stay the engine's. */
function validateConnectionDraft(
  record: unknown,
  expectedRevision: number | null,
): { draft: ManagedConnectionDraft; expected: number | null } {
  const value = object(record, "record");
  requiredText(value["id"], "record.id");
  requiredText(value["connectorId"], "record.connectorId");
  requiredText(value["accountId"], "record.accountId");
  const revision = value["revision"];
  if (!Number.isInteger(revision) || (revision as number) < 1)
    invalid("record.revision must be an integer >= 1");
  object(value["config"], "record.config");
  const workspace = value["workspaceId"];
  if (
    workspace !== undefined &&
    workspace !== null &&
    typeof workspace !== "string"
  )
    invalid("record.workspaceId must be a string or null");
  if (
    expectedRevision !== null &&
    expectedRevision !== undefined &&
    !Number.isInteger(expectedRevision)
  )
    invalid("expectedRevision must be an integer or null");
  return {
    draft: record as ManagedConnectionDraft,
    expected: expectedRevision ?? null,
  };
}

function validateRunContentSelection(selection: unknown): RunContentSelection {
  const value = object(selection, "selection");
  const template = value["template"];
  if (template !== undefined && template !== null)
    object(template, "selection.template");
  const skills = value["skills"];
  if (skills !== undefined && skills !== null) {
    if (!Array.isArray(skills) || skills.length > 20)
      invalid("selection.skills must be an array of at most 20 selections");
    for (const skill of skills as unknown[]) {
      const entry = object(skill, "selection.skills[]");
      requiredText(entry["id"], "selection.skills[].id");
      text(entry["revision"], "selection.skills[].revision");
    }
  }
  return selection as RunContentSelection;
}

function validateSkillSelection(skill: unknown): SkillSelection {
  const value = object(skill, "skill");
  requiredText(value["id"], "skill.id");
  requiredText(value["revision"], "skill.revision");
  return skill as SkillSelection;
}

// ---------------------------------------------------------------------------
// Namespace surfaces.
// ---------------------------------------------------------------------------

export interface NativePromptTemplates {
  /** Validate and insert one immutable revision; re-publishing a live or
   * removed (`id`, `revision`) is an `invalid_input` refusal. Returns the
   * stored template. */
  publish(template: PromptTemplate): Promise<PromptTemplate>;
  /** Every live template, `(id, revision)` order; tombstoned revisions are
   * absent. */
  list(): Promise<PromptTemplate[]>;
  /** One template, or `null` for an unknown or removed revision. */
  get(id: string, revision: string): Promise<PromptTemplate | null>;
  /** Erase the body while permanently reserving the revision. Unknown
   * revisions are a no-op. */
  remove(id: string, revision: string): Promise<void>;
  /** Render one revision with arguments; an unknown revision is
   * `not_found`, a bad argument set is `invalid_input`. */
  render(selection: PromptTemplateSelection): Promise<RenderedPromptTemplate>;
}

export interface NativeSkills {
  /** Publish one store-managed skill document under `(source, path)`; the
   * revision is the content hash of `content`. */
  publish(input: {
    source: string;
    path: string;
    content: string;
  }): Promise<ManagedSkillEntry>;
  /** Catalog entries in publish order, without document bodies. */
  list(): Promise<ManagedSkillEntry[]>;
  /** One entry by its `source:path` identity, or `null`. */
  get(id: string): Promise<ManagedSkillEntry | null>;
  /** Remove one document; unknown identities are a no-op. */
  remove(id: string): Promise<void>;
}

export interface NativeConnections {
  /** Records by id, credential-shaped transport values redacted. */
  list(): Promise<RedactedConnectionRecord[]>;
  /** One record, redacted, or `null`. */
  get(connectionId: string): Promise<RedactedConnectionRecord | null>;
  /** One atomic compare-and-swap: `expectedRevision` is `null` to create
   * (an existing id refuses with `invalid_state`) or the current revision
   * to update (a stale value refuses with `invalid_state`). The engine
   * normalizes `config.alias`, refuses a non-empty `secretRefs` with
   * `mcp_secret_missing`, enforces cross-account alias uniqueness and the
   * immutable `accountId`/`connectorId`/`workspaceId` identity, and returns
   * the stored record with `status: "disconnected"`, redacted. */
  save(
    record: ManagedConnectionDraft,
    expectedRevision: number | null,
  ): Promise<RedactedConnectionRecord>;
  /** Local deletion; unknown ids are a no-op. */
  remove(connectionId: string): Promise<void>;
  /** DOCUMENTED GAP (MCP OAuth, see the module header): forwarded to the
   * port surface, which refuses with `not_implemented`. */
  connect(connectionId: string): Promise<RedactedConnectionRecord>;
  /** DOCUMENTED GAP: refused with `not_implemented`. */
  reconnect(connectionId: string): Promise<RedactedConnectionRecord>;
  /** DOCUMENTED GAP: refused with `not_implemented`. */
  test(connectionId: string): Promise<RedactedConnectionRecord>;
  /** DOCUMENTED GAP: refused with `not_implemented`. */
  disconnect(connectionId: string): Promise<void>;
  /** The REST `connectionAction` shape; DOCUMENTED GAP: refused with
   * `not_implemented`, exactly as the HTTP lane answers the route with
   * 501. */
  action(
    connectionId: string,
    action: string,
  ): Promise<RedactedConnectionRecord>;
}

export interface NativeRunContent {
  /** Assemble one run: rendered template messages plus one context block
   * per authorized skill. A skill whose revision moved or whose connector
   * requirements are unmet refuses with `invalid_input` unless
   * `tolerateUnavailable` reports it in `unavailable` instead (the
   * reference `onSkillUnavailable` callback, carried as a boolean). */
  resolve(input: {
    selection: RunContentSelection;
    authorizedConnectors: readonly string[];
    tolerateUnavailable?: boolean;
  }): Promise<RunContentResolution>;
  /** Record one `{skill, path, revision}` reference per activated-skill
   * read; the path must stay inside its root and the revision is a
   * sha-256 hex digest. */
  recordResource(input: {
    skill: SkillSelection;
    path: string;
    revision: string;
  }): Promise<void>;
  /** Recorded references, oldest first; optionally one skill's only. */
  listResources(skillId?: string): Promise<RunContentResourceRecord[]>;
}

// ---------------------------------------------------------------------------
// The aggregate.
// ---------------------------------------------------------------------------

/** Build every managed-content surface over one call path (the root store
 * or an open transaction's view). All state is decided inside the engine;
 * each method here validates the wire shape, calls, and projects. */
function buildSurfaces(port: NativeStorePort): {
  promptTemplates: NativePromptTemplates;
  skills: NativeSkills;
  connections: NativeConnections;
  runContent: NativeRunContent;
} {
  const call = <T>(operation: string, input: unknown): Promise<T> =>
    port.callStore<T>(operation, input);

  const promptTemplates: NativePromptTemplates = {
    publish: async (template) => {
      validateTemplateShape(template);
      return call<PromptTemplate>("promptTemplates.publish", { template });
    },
    list: async () => call<PromptTemplate[]>("promptTemplates.list", {}),
    get: async (id, revision) =>
      (await call<PromptTemplate | null>("promptTemplates.get", {
        id: requiredText(id, "id"),
        revision: requiredText(revision, "revision"),
      })) ?? null,
    remove: async (id, revision) => {
      await call<null>("promptTemplates.remove", {
        id: requiredText(id, "id"),
        revision: requiredText(revision, "revision"),
      });
    },
    render: async (selection) => {
      const value = object(selection, "selection");
      return call<RenderedPromptTemplate>("promptTemplates.render", {
        id: requiredText(value["id"], "selection.id"),
        revision: requiredText(value["revision"], "selection.revision"),
        arguments: object(value["arguments"], "selection.arguments"),
      });
    },
  };

  const skills: NativeSkills = {
    publish: async (input) => {
      const value = object(input, "input");
      return call<ManagedSkillEntry>("skills.publish", {
        source: skillSource(value["source"]),
        path: requiredText(value["path"], "path"),
        content: text(value["content"], "content"),
      });
    },
    list: async () => call<ManagedSkillEntry[]>("skills.list", {}),
    get: async (id) =>
      (await call<ManagedSkillEntry | null>("skills.get", {
        id: requiredText(id, "id"),
      })) ?? null,
    remove: async (id) => {
      await call<null>("skills.remove", { id: requiredText(id, "id") });
    },
  };

  const lifecycle =
    (method: string) =>
    async (connectionId: string): Promise<never> =>
      call<never>(`connections.${method}`, {
        connectionId: requiredText(connectionId, "connectionId"),
      });
  const connections: NativeConnections = {
    list: async () => call<RedactedConnectionRecord[]>("connections.list", {}),
    get: async (connectionId) =>
      (await call<RedactedConnectionRecord | null>("connections.get", {
        connectionId: requiredText(connectionId, "connectionId"),
      })) ?? null,
    save: async (record, expectedRevision) => {
      const { draft, expected } = validateConnectionDraft(
        record,
        expectedRevision,
      );
      return call<RedactedConnectionRecord>("connections.save", {
        record: draft,
        expectedRevision: expected,
      });
    },
    remove: async (connectionId) => {
      await call<null>("connections.remove", {
        connectionId: requiredText(connectionId, "connectionId"),
      });
    },
    // The documented MCP-OAuth reduction: forwarded, never emulated. The
    // binding's catch-all answers `not_implemented` for every arm it does
    // not implement, so these calls reach the same authority the REST
    // lane's 501 comes from.
    connect: lifecycle("connect"),
    reconnect: lifecycle("reconnect"),
    test: lifecycle("test"),
    disconnect: lifecycle("disconnect"),
    action: async (connectionId, action) =>
      call<RedactedConnectionRecord>("connections.action", {
        connectionId: requiredText(connectionId, "connectionId"),
        action: requiredText(action, "action"),
      }),
  };

  const runContent: NativeRunContent = {
    resolve: async (input) => {
      const value = object(input, "input");
      const tolerate = value["tolerateUnavailable"];
      if (tolerate !== undefined && tolerate !== null && typeof tolerate !== "boolean")
        invalid("tolerateUnavailable must be a boolean");
      return call<RunContentResolution>("runContent.resolve", {
        selection: validateRunContentSelection(value["selection"]),
        authorizedConnectors: strings(
          value["authorizedConnectors"],
          "authorizedConnectors",
        ),
        tolerateUnavailable: tolerate ?? false,
      });
    },
    recordResource: async (input) => {
      const value = object(input, "input");
      await call<null>("runContent.recordResource", {
        skill: validateSkillSelection(value["skill"]),
        path: requiredText(value["path"], "path"),
        revision: requiredText(value["revision"], "revision"),
      });
    },
    listResources: async (skillId) =>
      call<RunContentResourceRecord[]>("runContent.listResources", {
        skillId: optionalText(skillId, "skillId") ?? null,
      }),
  };

  return { promptTemplates, skills, connections, runContent };
}

export interface NativeManagedContentOptions {
  /** A ready `NativeStore` handle — the usual construction. */
  store?: NativeStore;
  /** The binding's raw `NativeStore` object, wrapped in the same
   * JSON/error contract as `NativeStore.callStore`. */
  raw?: RawNativeStore;
  /** Internal: an already-open transaction view (what `transaction()`
   * hands the callback's aggregate). */
  port?: NativeStorePort;
}

/**
 * The managed-content aggregate over the Rust SQLite engine: prompt
 * templates, store-managed skills, non-secret connection configuration and
 * run-content assembly, each namespace a thin typed surface over
 * `callStore("<namespace>.<method>", input)`.
 *
 * `transaction(fn)` opens a real binding transaction and hands `fn` a
 * view whose EVERY call runs inside it (the arms are transaction-compatible
 * through the same `ports::execute` the transaction handle uses; see
 * `withNativeTransaction` for the flattening rule).
 */
export class NativeManagedContent {
  readonly promptTemplates: NativePromptTemplates;
  readonly skills: NativeSkills;
  readonly connections: NativeConnections;
  readonly runContent: NativeRunContent;

  private readonly port: NativeStorePort;

  constructor(options: NativeManagedContentOptions) {
    const port =
      options.port ??
      options.store ??
      (options.raw !== undefined
        ? createNativeStorePort(options.raw)
        : undefined);
    if (port === undefined) {
      throw new NativeAgentError(
        "invalid_input",
        "NativeManagedContent needs one of { store } | { raw } | { port }.",
      );
    }
    this.port = port;
    const surfaces = buildSurfaces(port);
    this.promptTemplates = surfaces.promptTemplates;
    this.skills = surfaces.skills;
    this.connections = surfaces.connections;
    this.runContent = surfaces.runContent;
  }

  async transaction<T>(
    fn: (tx: NativeManagedContent) => Promise<T>,
  ): Promise<T> {
    return this.port.transaction((view) =>
      fn(new NativeManagedContent({ port: view })),
    );
  }
}
