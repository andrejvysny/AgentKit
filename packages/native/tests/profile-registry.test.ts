import { expect, test } from "bun:test";

import { NativeAgentError, NativeStore } from "../src/index.js";
import type { NativeAgentProfile } from "../src/index.js";
import { available, missing, required } from "./helpers/binding.js";
import { testProfile } from "./helpers/profiles.js";

if (required && !available) {
  throw new Error(
    `AGENTKIT_REQUIRE_NATIVE=1 but the binding is not built (missing ${missing.join(", ")}); run \`bun run build:native\` in packages/native`,
  );
}

const suite = available ? test : test.skip;

function codeOf(error: unknown): string | undefined {
  return error instanceof NativeAgentError ? error.code : undefined;
}

suite(
  "declared profiles become the sessions' policy; an undeclared id is still refused",
  async () => {
    const store = await NativeStore.inMemory();
    const service = await store.createService({
      profiles: [testProfile("declared", { tools: ["echo"] })],
    });
    try {
      await service.createSession({
        sessionId: "declared",
        profileId: "declared",
      });
      // The option EXTENDS the registry: the built-ins stay admissible
      // beside the declared profile.
      await service.createSession({
        sessionId: "builtin",
        profileId: "coding",
      });
      const state = await service.getState("declared");
      const descriptor =
        "descriptor" in state && state.descriptor
          ? (state.descriptor as { profile?: { id?: string } })
          : {};
      expect(descriptor.profile?.id).toBe("declared");

      let error: unknown;
      try {
        await service.createSession({
          sessionId: "undeclared",
          profileId: "undeclared",
        });
      } catch (caught) {
        error = caught;
      }
      expect(error).toBeInstanceOf(NativeAgentError);
      expect(codeOf(error)).toBe("invalid_input");
    } finally {
      await service.close();
      await store.close();
    }
  },
);

suite(
  "a service with no `profiles` option still admits only the built-ins",
  async () => {
    const store = await NativeStore.inMemory();
    const service = await store.createService();
    try {
      await service.createSession({ sessionId: "plain", profileId: "coding" });
      let error: unknown;
      try {
        await service.createSession({
          sessionId: "invented",
          profileId: "declared-elsewhere",
        });
      } catch (caught) {
        error = caught;
      }
      expect(error).toBeInstanceOf(NativeAgentError);
      expect(codeOf(error)).toBe("invalid_input");
      const state = await service.getState("plain");
      const descriptor =
        "descriptor" in state && state.descriptor
          ? (state.descriptor as { profile?: { id?: string } })
          : {};
      expect(descriptor.profile?.id).toBe("coding");
    } finally {
      await service.close();
      await store.close();
    }
  },
);

suite(
  "a definition the host rejects fails construction, never a looser session policy",
  async () => {
    const store = await NativeStore.inMemory();
    const malformed: [string, NativeAgentProfile][] = [
      [
        "unknown kind",
        {
          ...testProfile("kind"),
          kind: "wizard" as NativeAgentProfile["kind"],
        },
      ],
      [
        "duplicate tool names",
        { ...testProfile("dup"), tools: ["echo", "echo"] },
      ],
      [
        "a reviewer holding a write tool",
        {
          ...testProfile("reviewer", { tools: ["write_file"] }),
          role: "reviewer",
        },
      ],
      [
        "a zero limit",
        { ...testProfile("zero"), limits: { maxToolIterations: 0 } },
      ],
    ];
    const outcomes: string[] = [];
    for (const [label, profile] of malformed) {
      try {
        const service = await store.createService({ profiles: [profile] });
        await service.close();
        outcomes.push(`${label}: accepted`);
      } catch (caught) {
        outcomes.push(`${label}: ${codeOf(caught) ?? "not an agent error"}`);
      }
    }
    expect(outcomes).toEqual(
      malformed.map(([label]) => `${label}: invalid_input`),
    );

    // A non-array is a refusal too: the binding never advertises a registry
    // it did not actually build.
    let notAnArray: unknown;
    try {
      await store.createService({
        profiles: { id: "object" } as unknown as NativeAgentProfile[],
      });
    } catch (caught) {
      notAnArray = caught;
    }
    expect(notAnArray).toBeInstanceOf(NativeAgentError);
    expect(codeOf(notAnArray)).toBe("invalid_input");
    await store.close();
  },
);
