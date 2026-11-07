import { testServiceOptions } from "./helpers/test-profiles.js";
/**
 * The managed-content binding surface: `NativeManagedContent` typed methods
 * over the `callStore` port path (`promptTemplates.*`, `skills.*`,
 * `connections.*`, `runContent.*`). Every engine refusal here — tombstones,
 * revision CAS, catalog order, frontmatter validation, redaction, deny-all
 * `secretRefs`, alias uniqueness, immutable identity, the connector gate,
 * unavailable tolerance — is decided inside `agentkit-store::managed_content`;
 * what these tests pin is that the typed wrapper reaches the same authority
 * the Rust port tests do, fails malformed wires before the FFI with the
 * same stable code, survives a reopen of the same sqlite file, and that the
 * documented MCP-OAuth reduction surfaces as `not_implemented` — never as a
 * fake connection.
 */
import { expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  NativeAgentError,
  NativeManagedContent,
  NativeStore,
  type NativeStorePort,
} from "../src/index.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;

/** The engine's hex64 witness (the sha-256 of "hello"). */
const HEX64 =
  "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

function template(id: string, revision: string, content: string) {
  return {
    id,
    revision,
    description: "greeting",
    argumentsSchema: {
      type: "object" as const,
      properties: { name: { type: "string" as const } },
    },
    messages: [{ role: "user" as const, content }],
  };
}

function skillText(name: string, requires: string) {
  return `---\nname: ${name}\ndescription: does things\nmetadata:\n  requires-connectors: ${requires}\n---\nbody of ${name}\n`;
}

function connection(
  id: string,
  revision: number,
  alias: string,
  config: Record<string, unknown> = {},
) {
  return {
    id,
    connectorId: "github",
    accountId: "acc",
    revision,
    config: {
      alias,
      transport: { kind: "stdio", command: "run", env: { TOKEN: "literal" } },
      ...config,
    },
  };
}

const fresh = async () => {
  const store = await NativeStore.inMemory();
  return { store, managed: new NativeManagedContent({ store }) };
};

/** The stored transport `env` bag, read through runtime narrowing (the
 * engine echoes this JSON over the FFI; the tests assert its exact shape,
 * so the reads must not fabricate one). */
function transportEnv(config: unknown): Record<string, string> | undefined {
  if (typeof config !== "object" || config === null || !("transport" in config))
    return undefined;
  const transport = config.transport;
  if (
    typeof transport !== "object" ||
    transport === null ||
    !("env" in transport)
  )
    return undefined;
  const env = transport.env;
  if (typeof env !== "object" || env === null || Array.isArray(env))
    return undefined;
  return env as Record<string, string>;
}

nativeTest(
  "prompt templates: publish, list order, get, render, remove leaves a tombstone",
  async () => {
    const { store, managed } = await fresh();
    try {
      const published = await managed.promptTemplates.publish(
        template("greet", "1", "Hi {{name}}"),
      );
      expect(published.id).toBe("greet");
      await managed.promptTemplates.publish(template("greet", "2", "Hello"));

      const listed = await managed.promptTemplates.list();
      expect(listed.map((t) => `${t.id}@${t.revision}`)).toEqual([
        "greet@1",
        "greet@2",
      ]);

      expect((await managed.promptTemplates.get("greet", "1"))?.revision).toBe(
        "1",
      );
      expect(await managed.promptTemplates.get("greet", "9")).toBeNull();

      const rendered = await managed.promptTemplates.render({
        id: "greet",
        revision: "1",
        arguments: { name: "ada" },
      });
      expect(rendered.messages[0]?.content).toBe("Hi ada");
      expect(rendered.provenance).toMatchObject({ id: "greet", revision: "1" });
      await expect(
        managed.promptTemplates.render({
          id: "greet",
          revision: "9",
          arguments: {},
        }),
      ).rejects.toMatchObject({ code: "not_found" });

      // Revision immutability: re-publishing a live revision refuses.
      await expect(
        managed.promptTemplates.publish(template("greet", "2", "again")),
      ).rejects.toMatchObject({ code: "invalid_input" });

      await managed.promptTemplates.remove("greet", "1");
      expect(await managed.promptTemplates.get("greet", "1")).toBeNull();
      // The tombstone permanently reserves the revision.
      await expect(
        managed.promptTemplates.publish(template("greet", "1", "x")),
      ).rejects.toMatchObject({ code: "invalid_input" });
      // Removed revisions read as absent in the list too.
      expect((await managed.promptTemplates.list()).length).toBe(1);
    } finally {
      await store.close();
    }
  },
);

nativeTest(
  "connections: create/update CAS, *** redaction, deny-all, alias uniqueness, immutable identity, lifecycle refused",
  async () => {
    const { store, managed } = await fresh();
    try {
      const saved = await managed.connections.save(
        connection("c1", 1, " github ") as never,
        null,
      );
      expect(saved.status).toBe("disconnected");
      expect(saved.config.alias).toBe("github");
      // The literal never comes back through any read path.
      expect(transportEnv(saved.config)?.TOKEN).toBe("***");

      // Create-only CAS: a second create on the same id refuses.
      await expect(
        managed.connections.save(connection("c1", 1, "github") as never, null),
      ).rejects.toMatchObject({ code: "invalid_state" });

      // The legitimate update: expectedRevision = current, revision = +1.
      const updated = await managed.connections.save(
        connection("c1", 2, "github") as never,
        1,
      );
      expect(updated.revision).toBe(2);

      // CAS loss: a stale expectedRevision is a state violation.
      await expect(
        managed.connections.save(connection("c1", 3, "github") as never, 1),
      ).rejects.toMatchObject({ code: "invalid_state" });

      // Cross-account alias uniqueness (normalized comparison).
      await expect(
        managed.connections.save(
          { ...connection("c2", 1, "Github"), accountId: "other" } as never,
          null,
        ),
      ).rejects.toMatchObject({ code: "invalid_input" });

      // Immutable identity: accountId may not move on an update.
      await expect(
        managed.connections.save(
          { ...connection("c1", 3, "github"), accountId: "other" } as never,
          2,
        ),
      ).rejects.toMatchObject({ code: "invalid_input" });

      // Deny-all secretRefs: the binding resolves none, so a reference is
      // refused with its dedicated stable code, never accepted-and-dropped.
      await expect(
        managed.connections.save(
          connection("c2", 1, "other", {
            secretRefs: { token: "vault://ref" },
          }) as never,
          null,
        ),
      ).rejects.toMatchObject({ code: "mcp_secret_missing" });

      expect((await managed.connections.list()).map((c) => c.id)).toEqual([
        "c1",
      ]);
      const got = await managed.connections.get("c1");
      expect(transportEnv(got?.config)?.TOKEN).toBe("***");
      expect(await managed.connections.get("nope")).toBeNull();

      // The documented MCP-OAuth reduction: the lifecycle legs reach the
      // binding and come back refused, never pretending to connect.
      for (const refused of [
        () => managed.connections.action("c1", "connect"),
        () => managed.connections.connect("c1"),
        () => managed.connections.reconnect("c1"),
        () => managed.connections.test("c1"),
        () => managed.connections.disconnect("c1"),
      ]) {
        await expect(refused()).rejects.toMatchObject({
          code: "not_implemented",
        });
      }

      await managed.connections.remove("c1");
      expect(await managed.connections.get("c1")).toBeNull();
      await managed.connections.remove("nope"); // unknown ids are a no-op
    } finally {
      await store.close();
    }
  },
);

nativeTest(
  "skills: catalog publish order, frontmatter validation, get and remove",
  async () => {
    const { store, managed } = await fresh();
    try {
      const review = await managed.skills.publish({
        source: "docs",
        path: "review",
        content: skillText("reviewer", "github"),
      });
      expect(review).toMatchObject({
        id: "docs:review",
        sourceId: "docs",
        path: "review",
        name: "reviewer",
        description: "does things",
        connectorRequirements: ["github"],
      });
      expect(review.revision).toMatch(/^[0-9a-f]{64}$/);
      await managed.skills.publish({
        source: "docs",
        path: "guide",
        content: skillText("guide", ""),
      });

      // Catalog order is publish order, not identity order.
      expect((await managed.skills.list()).map((s) => s.id)).toEqual([
        "docs:review",
        "docs:guide",
      ]);
      // Entries carry no document body.
      expect(
        Object.keys((await managed.skills.get("docs:review")) ?? {}),
      ).not.toContain("content");
      expect(await managed.skills.get("docs:nope")).toBeNull();

      // The frontmatter subset is validated at publish.
      await expect(
        managed.skills.publish({
          source: "docs",
          path: "broken",
          content: "no frontmatter at all",
        }),
      ).rejects.toMatchObject({ code: "invalid_input" });
      await expect(
        managed.skills.publish({
          source: "docs",
          path: "broken",
          content: "---\nname: Not Kebab\ndescription: x\n---\nbody\n",
        }),
      ).rejects.toMatchObject({ code: "invalid_input" });

      // Re-publishing a path replaces the document under a new content hash.
      const republished = await managed.skills.publish({
        source: "docs",
        path: "review",
        content: skillText("reviewer", "github gitlab"),
      });
      expect(republished.id).toBe("docs:review");
      expect(republished.revision).not.toBe(review.revision);

      await managed.skills.remove("docs:review");
      expect(await managed.skills.get("docs:review")).toBeNull();
      await managed.skills.remove("docs:nope"); // unknown ids are a no-op
    } finally {
      await store.close();
    }
  },
);

nativeTest(
  "runContent: resolve renders the template, gates skills, tolerates unavailable, records resources",
  async () => {
    const { store, managed } = await fresh();
    try {
      await managed.promptTemplates.publish(
        template("greet", "1", "Hi {{name}}"),
      );
      const skill = await managed.skills.publish({
        source: "docs",
        path: "review",
        content: skillText("reviewer", "github"),
      });

      const resolved = await managed.runContent.resolve({
        selection: {
          template: { id: "greet", revision: "1", arguments: { name: "ada" } },
          skills: [{ id: "docs:review", revision: skill.revision }],
        },
        authorizedConnectors: ["github"],
      });
      expect(resolved.messages[0]?.content).toBe("Hi ada");
      expect(resolved.blocks[0]).toMatchObject({
        title: "Skill: reviewer",
        priority: 100,
      });
      expect(resolved.unavailable).toEqual([]);

      // The connector gate: an unmet requirement refuses without tolerance.
      await expect(
        managed.runContent.resolve({
          selection: {
            skills: [{ id: "docs:review", revision: skill.revision }],
          },
          authorizedConnectors: [],
        }),
      ).rejects.toMatchObject({ code: "invalid_input" });

      // A moved revision refuses too, but reports instead of throwing when
      // tolerated (the reference `onSkillUnavailable` leg).
      const tolerated = await managed.runContent.resolve({
        selection: { skills: [{ id: "docs:review", revision: "wrong" }] },
        authorizedConnectors: ["github"],
        tolerateUnavailable: true,
      });
      expect(tolerated.blocks).toEqual([]);
      expect(tolerated.unavailable[0]?.id).toBe("docs:review");

      // Duplicate skill identity is a selection error.
      await expect(
        managed.runContent.resolve({
          selection: {
            skills: [
              { id: "docs:review", revision: skill.revision },
              { id: "docs:review", revision: skill.revision },
            ],
          },
          authorizedConnectors: ["github"],
        }),
      ).rejects.toMatchObject({ code: "invalid_input" });

      const skillSelection = { id: "docs:review", revision: skill.revision };
      await managed.runContent.recordResource({
        skill: skillSelection,
        path: "references/guide.md",
        revision: HEX64,
      });
      const mine = await managed.runContent.listResources("docs:review");
      expect(mine).toEqual([
        { skill: skillSelection, path: "references/guide.md", revision: HEX64 },
      ]);
      expect((await managed.runContent.listResources()).length).toBe(1);

      // Reference paths stay inside the skill root.
      await expect(
        managed.runContent.recordResource({
          skill: skillSelection,
          path: "../escape",
          revision: HEX64,
        }),
      ).rejects.toMatchObject({ code: "invalid_input" });
    } finally {
      await store.close();
    }
  },
);

nativeTest(
  "transaction arms: commit makes writes visible, rollback leaves no trace",
  async () => {
    const { store, managed } = await fresh();
    try {
      await managed.transaction(async (tx) => {
        await tx.promptTemplates.publish(template("tx", "1", "x"));
        await tx.skills.publish({
          source: "docs",
          path: "tx",
          content: skillText("tx-skill", ""),
        });
      });
      expect(await managed.promptTemplates.get("tx", "1")).not.toBeNull();
      expect(await managed.skills.get("docs:tx")).not.toBeNull();

      await expect(
        managed.transaction(async (tx) => {
          await tx.promptTemplates.publish(template("tx", "2", "y"));
          throw new Error("abort the transaction");
        }),
      ).rejects.toThrow("abort the transaction");
      expect(await managed.promptTemplates.get("tx", "2")).toBeNull();
      expect((await managed.promptTemplates.list()).length).toBe(1);
    } finally {
      await store.close();
    }
  },
);

nativeTest(
  "malformed inputs fail client-side with the binding's code, before the FFI",
  async () => {
    let ffiCalls = 0;
    const port: NativeStorePort = {
      callStore: async () => {
        ffiCalls++;
        throw new Error("must not reach the FFI");
      },
      transaction: async () => {
        ffiCalls++;
        throw new Error("must not reach the FFI");
      },
    };
    const managed = new NativeManagedContent({ port });

    let constructed: unknown;
    try {
      new NativeManagedContent({});
    } catch (error) {
      constructed = error;
    }
    expect(constructed).toBeInstanceOf(NativeAgentError);
    expect(constructed).toMatchObject({ code: "invalid_input" });

    const refusals: (() => Promise<unknown>)[] = [
      () => managed.promptTemplates.publish(null as never),
      () => managed.promptTemplates.publish(template("", "1", "x") as never),
      () =>
        managed.promptTemplates.publish({
          ...template("t", "1", "x"),
          messages: [],
        } as never),
      () =>
        managed.promptTemplates.publish({
          ...template("t", "1", "x"),
          messages: [{ role: "tool", content: "x" }],
        } as never),
      () =>
        managed.promptTemplates.publish({
          ...template("t", "1", "x"),
          argumentsSchema: "not-an-object",
        } as never),
      () => managed.promptTemplates.get(42 as never, "1"),
      () =>
        managed.promptTemplates.render({
          id: "t",
          revision: "1",
          arguments: "nope",
        } as never),
      () => managed.skills.publish({ source: "a:b", path: "p", content: "c" }),
      () =>
        managed.skills.publish({
          source: "a",
          path: "p",
          content: 42 as never,
        }),
      () => managed.skills.get(""),
      () => managed.skills.remove(7 as never),
      () =>
        managed.connections.save(
          { ...connection("c", 0, "github") } as never,
          null,
        ),
      () =>
        managed.connections.save(connection("c", 1, "github") as never, 1.5),
      () => managed.connections.save(null as never, null),
      () => managed.connections.get(""),
      () => managed.connections.remove(null as never),
      () =>
        managed.runContent.resolve({
          selection: {},
          authorizedConnectors: "github",
        } as never),
      () => managed.runContent.resolve({ selection: "nope" } as never),
      () =>
        managed.runContent.recordResource({
          skill: { id: "", revision: "r" },
          path: "p",
          revision: "r",
        }),
      () =>
        managed.runContent.recordResource({
          skill: { id: "s", revision: "r" },
          path: "",
          revision: "r",
        }),
      () => managed.runContent.listResources(42 as never),
    ];
    for (const refusal of refusals) {
      await expect(refusal()).rejects.toBeInstanceOf(NativeAgentError);
      await expect(refusal()).rejects.toMatchObject({ code: "invalid_input" });
    }
    expect(ffiCalls).toBe(0);
  },
);

nativeTest(
  "reopen stability: a fresh NativeStore.open on the same file sees everything",
  async () => {
    const directory = mkdtempSync(join(tmpdir(), "native-managed-content-"));
    const path = join(directory, "managed.sqlite");
    try {
      const first = await NativeStore.open(path);
      const managed = new NativeManagedContent({ store: first });
      await managed.promptTemplates.publish(
        template("greet", "1", "Hi {{name}}"),
      );
      await managed.promptTemplates.publish(template("greet", "2", "Hello"));
      await managed.promptTemplates.remove("greet", "1");
      await managed.skills.publish({
        source: "docs",
        path: "review",
        content: skillText("reviewer", "github"),
      });
      await managed.connections.save(
        connection("c1", 1, " github ") as never,
        null,
      );
      await first.close();

      // Witness: a completely fresh binding handle on the same file.
      const second = await NativeStore.open(path);
      const reopened = new NativeManagedContent({ store: second });
      try {
        expect(
          (await reopened.promptTemplates.list()).map(
            (t) => `${t.id}@${t.revision}`,
          ),
        ).toEqual(["greet@2"]);
        expect(await reopened.promptTemplates.get("greet", "1")).toBeNull();
        await expect(
          reopened.promptTemplates.publish(template("greet", "1", "x")),
        ).rejects.toMatchObject({ code: "invalid_input" });

        const skills = await reopened.skills.list();
        expect(skills.map((s) => s.id)).toEqual(["docs:review"]);
        const connectionAfter = await reopened.connections.get("c1");
        expect(connectionAfter).toMatchObject({
          id: "c1",
          revision: 1,
          status: "disconnected",
          config: { alias: "github" },
        });
        expect(transportEnv(connectionAfter?.config)?.TOKEN).toBe("***");
      } finally {
        await second.close();
      }
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  },
);

/** 32 bytes: the exact floor `DesktopBackend::new` enforces. */
const TOKEN = "12345678901234567890123456789012";
const AUTH = { authorization: `Bearer ${TOKEN}` };

nativeTest(
  "HTTP lane pass-through: publish answers 201 and delete 204 on the shared engine",
  async () => {
    const store = await NativeStore.inMemory();
    const managed = new NativeManagedContent({ store });
    const service = await store.createService(testServiceOptions());
    try {
      const { baseUrl } = await service.startHttpServer({ bearerToken: TOKEN });

      const published = await fetch(`${baseUrl}/v1/prompt-templates`, {
        method: "POST",
        headers: { ...AUTH, "content-type": "application/json" },
        body: JSON.stringify(template("greet", "1", "Hi {{name}}")),
      });
      expect(published.status).toBe(201);
      expect(JSON.parse(await published.text())).toMatchObject({ id: "greet" });
      // The pass-through claim: the typed wrapper reads the SAME engine
      // row the Rust route just wrote.
      expect((await managed.promptTemplates.get("greet", "1"))?.revision).toBe(
        "1",
      );

      const removed = await fetch(`${baseUrl}/v1/prompt-templates/greet/1`, {
        method: "DELETE",
        headers: AUTH,
      });
      expect(removed.status).toBe(204);
      expect(await managed.promptTemplates.get("greet", "1")).toBeNull();
    } finally {
      await service.close();
      await store.close();
    }
  },
  20_000,
);
