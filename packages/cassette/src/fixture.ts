import { type Stats, statSync } from "node:fs";
import { readFile, stat } from "node:fs/promises";
import { type Static, Type } from "@sinclair/typebox";
import { Ajv } from "ajv";
import { CassetteError } from "./errors.js";
import type { CassetteFixture } from "./types.js";

const Usage = Type.Partial(
  Type.Object({
    promptTokens: Type.Number(),
    completionTokens: Type.Number(),
    cachedPromptTokens: Type.Number(),
    reasoningTokens: Type.Number(),
  }),
);

const MessageShape = Type.Object({
  role: Type.String(),
  contentSha256: Type.Optional(Type.String()),
  contentText: Type.Optional(Type.String()),
  toolCalls: Type.Optional(
    Type.Array(
      Type.Object({
        id: Type.String(),
        name: Type.Optional(Type.String()),
        argsSha256: Type.String(),
      }),
    ),
  ),
  toolCallId: Type.Optional(Type.String()),
  name: Type.Optional(Type.String()),
});

const ProviderRequest = Type.Object({
  providerId: Type.String(),
  providerKind: Type.String(),
  model: Type.String(),
  temperature: Type.Optional(Type.Number()),
  maxOutputTokens: Type.Optional(Type.Number()),
  reasoningEffort: Type.Optional(Type.String()),
  toolsFingerprint: Type.String(),
  messageShape: Type.Array(MessageShape),
  continuation: Type.Object({
    present: Type.Boolean(),
    scopeChatId: Type.Optional(Type.String()),
    scopeBranchId: Type.Optional(Type.String()),
  }),
  hasRunContent: Type.Boolean(),
});

const RunEvent = Type.Object({
  type: Type.String(),
  data: Type.Unknown(),
});

const EffectBase = {
  effectId: Type.String(),
  parentEffectId: Type.Optional(Type.String()),
  sequence: Type.Number(),
  structuralFingerprint: Type.String(),
  fullFingerprint: Type.String(),
  trace: Type.Object({
    steps: Type.Array(Type.String()),
    usage: Type.Optional(Usage),
  }),
};

const ProviderEffect = Type.Object({
  ...EffectBase,
  kind: Type.Literal("provider"),
  request: ProviderRequest,
  outcome: Type.Union([
    Type.Object({ type: Type.Literal("stream"), events: Type.Array(RunEvent) }),
    Type.Object({
      type: Type.Literal("error"),
      errorCode: Type.String(),
      message: Type.Optional(Type.String()),
      events: Type.Optional(Type.Array(RunEvent)),
    }),
    Type.Object({
      type: Type.Literal("abort"),
      message: Type.Optional(Type.String()),
      events: Type.Optional(Type.Array(RunEvent)),
    }),
  ]),
});

const ToolEffect = Type.Object({
  ...EffectBase,
  kind: Type.Literal("tool"),
  tool: Type.Object({
    name: Type.String(),
    definitionFingerprint: Type.String(),
  }),
  argsSha256: Type.String(),
  arguments: Type.Optional(Type.Unknown()),
  resultSha256: Type.Optional(Type.String()),
  outcome: Type.Union([
    Type.Object({ type: Type.Literal("result"), result: Type.Unknown() }),
    Type.Object({
      type: Type.Literal("suspended"),
      prompt: Type.String(),
    }),
    Type.Object({
      type: Type.Literal("error"),
      errorCode: Type.String(),
      message: Type.Optional(Type.String()),
    }),
    Type.Object({
      type: Type.Literal("abort"),
      message: Type.Optional(Type.String()),
    }),
  ]),
});

const CapabilitiesEffect = Type.Object({
  ...EffectBase,
  kind: Type.Literal("capabilities"),
  model: Type.Optional(Type.String()),
  capabilities: Type.Unknown(),
});

const ModelsEffect = Type.Object({
  ...EffectBase,
  kind: Type.Literal("models"),
  models: Type.Unknown(),
});

const FixtureSchema = Type.Object({
  schemaVersion: Type.Literal(1),
  capture: Type.Union([Type.Literal("metadata"), Type.Literal("content")]),
  createdAt: Type.Optional(Type.String()),
  generator: Type.Optional(
    Type.Object({
      name: Type.String(),
      version: Type.Optional(Type.String()),
    }),
  ),
  effects: Type.Array(
    Type.Union([ProviderEffect, ToolEffect, CapabilitiesEffect, ModelsEffect]),
  ),
});

type FixtureWire = Static<typeof FixtureSchema>;

const ajv = new Ajv({ allErrors: true, strict: false });
const validateFixture = ajv.compile(FixtureSchema);

/**
 * Validate and return a fixture. Every fixture read goes through here —
 * a malformed fixture is `cassette_schema`, never a downstream crash.
 */
export function parseFixtureJson(
  text: string,
  source: string,
): CassetteFixture {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (cause) {
    throw new CassetteError(
      "cassette_schema",
      `${source} is not valid JSON: ${(cause as Error).message}`,
    );
  }
  if (!validateFixture(parsed)) {
    throw new CassetteError(
      "cassette_schema",
      `${source} failed fixture validation: ${ajv
        .errorsText(validateFixture.errors)
        .slice(0, 500)}`,
    );
  }
  const fixture = parsed as FixtureWire;
  // Cross-field invariants the JSON schema cannot express.
  const seen = new Set<string>();
  for (const effect of fixture.effects) {
    if (seen.has(effect.effectId))
      throw new CassetteError(
        "cassette_schema",
        `${source}: duplicate effectId ${effect.effectId}`,
      );
    seen.add(effect.effectId);
    if (!Number.isInteger(effect.sequence) || effect.sequence < 0)
      throw new CassetteError(
        "cassette_schema",
        `${source}: effect ${effect.effectId} has an invalid sequence`,
      );
  }
  const ordered = [...fixture.effects].sort((a, b) => a.sequence - b.sequence);
  for (const effect of ordered) {
    if (effect.parentEffectId !== undefined && !seen.has(effect.parentEffectId))
      throw new CassetteError(
        "cassette_schema",
        `${source}: effect ${effect.effectId} references unknown parent ${effect.parentEffectId}`,
      );
  }
  return fixture as CassetteFixture;
}

/** A fixture larger than this is treated as hostile or corrupt, never parsed. */
export const MAX_FIXTURE_READ_BYTES = 64 * 1024 * 1024;

export async function loadFixture(path: string): Promise<CassetteFixture> {
  let text: string;
  try {
    const stats = await stat(path);
    if (stats.size > MAX_FIXTURE_READ_BYTES)
      throw new CassetteError(
        "cassette_schema",
        `fixture ${path} is ${stats.size} bytes, over the ${MAX_FIXTURE_READ_BYTES}-byte ceiling`,
      );
    text = await readFile(path, "utf8");
  } catch (cause) {
    if (cause instanceof CassetteError) throw cause;
    throw new CassetteError(
      "cassette_schema",
      `cannot read fixture ${path}: ${(cause as Error).message}`,
    );
  }
  return parseFixtureJson(text, path);
}

/** Cheap guard for record targets: refuse to overwrite a non-fixture file. */
export function assertFreshRecordTarget(path: string): void {
  let stats: Stats | undefined;
  try {
    stats = statSync(path);
  } catch {
    return; // does not exist — fine
  }
  if (!stats.isFile())
    throw new CassetteError(
      "cassette_schema",
      `record target ${path} exists and is not a file`,
    );
}
