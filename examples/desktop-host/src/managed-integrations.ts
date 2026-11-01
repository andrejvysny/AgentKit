/**
 * The managed-integrations recipe — now fully binding-backed.
 *
 * Every leg this recipe assembles runs in Rust through the typed wrapper
 * `NativeManagedContent` (`@agentkit/native`, `packages/native/src/managed-content.ts`):
 * prompt templates, store-managed skills, connection configuration and
 * run-content assembly are `callStore` arms executed by
 * `agentkit-store::managed_content` (engine authority:
 * `rust/crates/store/src/managed_content.rs`, binding surface:
 * `rust/crates/napi/src/store_managed_content.rs`). The TypeScript
 * `ConnectionService`, `PromptTemplateService`, `SkillService` and
 * `createRunContentProvider` implementations this file used to build are
 * GONE: validation, ordering, compare-and-swap, alias normalization,
 * redaction (`***`), deny-all `secretRefs` (`mcp_secret_missing`) and
 * template tombstones are decided by the engine, on the one native store
 * file the composition already owns.
 *
 * The honest gap (unchanged, documented, refused — never faked): the MCP
 * OAuth lifecycle leg. `connections.connect`/`reconnect`/`test`/
 * `disconnect`/`action` cross the port surface and come back refused with
 * `not_implemented`, exactly as the REST lane answers `connectionAction`
 * with 501 (`docs/rust/native-binding-milestone.md` "Open gaps": no OAuth,
 * deny-all `secretRefs`). There is no OAuth transport factory, no secret
 * vault, no per-connection revoke hook to hand over — because the binding
 * has none to honor. Remote MCP tools themselves ARE native here: the
 * stdio/streamable-HTTP bridge (`store.mcpConnect` + `registerMcpTools`,
 * see `./wiring.ts`) runs the crate's client manager with host-decided
 * effect, local schema validation and approval parking; this recipe stays
 * what it always was — the opt-in settings/UI slice, never the hot path.
 */
import { NativeManagedContent, type NativeStore } from "@agentkit/native";

/** Optional recipe for applications with their own settings and approval
 * UI. One input: the native store the composition already opens — the
 * managed-content tables live in that same SQLite file and the same
 * transaction gate. */
export function createManagedIntegrations(options: { store: NativeStore }) {
  const managed = new NativeManagedContent({ store: options.store });
  return {
    /** Connection configuration: `list`/`get`/`save`/`remove` execute in
     * the engine (CAS, alias uniqueness, immutable identity, redaction);
     * `connect`/`reconnect`/`test`/`disconnect`/`action` are the documented
     * MCP-OAuth reduction and reject with `not_implemented`. */
    connections: managed.connections,
    /** `publish`/`list`/`get`/`remove`/`render` over the frozen template
     * contract, tombstones included. */
    templates: managed.promptTemplates,
    /** `publish`/`list`/`get`/`remove` for store-managed skill documents
     * (content-hashed revisions, frontmatter subset validation). */
    skills: managed.skills,
    /** `resolve`/`recordResource`/`listResources`: template messages plus
     * one authorized skill block per selection, the reference's
     * unavailable-skill tolerance carried as a boolean. */
    runContent: managed.runContent,
    /** The same surfaces under the REST-facing names, for an app wiring
     * its own settings routes on top of the loopback façade. */
    rest: {
      connections: managed.connections,
      promptTemplates: managed.promptTemplates,
      skills: managed.skills,
    },
    /** All four surfaces inside one binding transaction (the arms are
     * transaction-compatible; see `NativeManagedContent.transaction`). */
    transaction: managed.transaction.bind(managed),
  };
}

export { createManagedConnectorAcceptance } from "./managed-connector-acceptance.js";
