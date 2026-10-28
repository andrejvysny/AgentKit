import { randomUUID } from "node:crypto";
import { link, mkdir, open, unlink } from "node:fs/promises";
import { dirname } from "node:path";
import type {
  AiProviderCapabilities,
  AiProviderModel,
  AiRunEvent,
  AiToolResult,
} from "@agentkit/contracts";
import { CONTRACT_VERSION } from "@agentkit/contracts";
import type {
  AiChatRequest,
  AiProviderClient,
  AiTool,
  AiToolExecutionContext,
} from "@agentkit/core";
import { AiToolRegistry } from "@agentkit/core";
import { CassetteError } from "./errors.js";
import {
  assertFreshRecordTarget,
  loadFixture,
  parseFixtureJson,
} from "./fixture.js";
import {
  canonicalJson,
  fingerprintToolCall,
  fingerprintToolDefinitions,
  providerRequestView,
  sha256,
} from "./fingerprint.js";
import { Recorder, eventStep } from "./recorder.js";
import { Replayer } from "./replayer.js";
import type { RedactPolicy } from "./redaction.js";
import { recordToolResult, scrubDeep } from "./redaction.js";
import type {
  CassetteCapture,
  CassetteFixture,
  CassetteMode,
  CassetteOptions,
  RecordedRunEvent,
  RecordedToolEffect,
} from "./types.js";
import { asToolResult } from "./types.js";

export interface CassetteSummary {
  mode: CassetteMode;
  capture: CassetteCapture;
  recorded: number;
  consumed: number;
  unconsumed: number;
}

const REPLAY_EPOCH = "1970-01-01T00:00:00.000Z";

/** Hard ceiling on a persisted fixture; runaway recordings must fail, not balloon. */
const MAX_FIXTURE_BYTES = 64 * 1024 * 1024;

function abortError(message?: string): Error {
  return Object.assign(new Error(message ?? "cassette: recorded abort"), {
    name: "AbortError",
  });
}

function classifyError(cause: unknown): {
  type: "error" | "abort";
  errorCode?: string;
  message?: string;
} {
  if (cause instanceof Error && cause.name === "AbortError")
    return { type: "abort", message: cause.message };
  return {
    type: "error",
    errorCode:
      cause instanceof Error && cause.name && cause.name !== "Error"
        ? cause.name
        : "error",
    message: cause instanceof Error ? cause.message : String(cause),
  };
}

/** Rebuild a recorded stream event into a valid `AiRunEvent` for replay. */
function replayEvent(
  runId: string,
  seq: number,
  recorded: { type: string; data: unknown },
): AiRunEvent {
  return {
    type: recorded.type as AiRunEvent["type"],
    runId,
    timestamp: REPLAY_EPOCH,
    contractVersion: CONTRACT_VERSION,
    seq,
    data: recorded.data,
  } as AiRunEvent;
}

function neutralToolResult(): AiToolResult {
  return {
    ok: true,
    data: null,
    truncated: false,
    sources: [],
    warnings: [],
    limits: { profile: "large", maxBytes: 0 },
  };
}

class Cassette {
  readonly mode: CassetteMode;
  readonly capture: CassetteCapture;
  private readonly onUnconsumed: "fail" | "ignore";
  private readonly fixturePath?: string;
  private readonly recorder: Recorder | undefined;
  private replayer: Replayer | undefined;
  private readonly fixturePromise: Promise<void> | undefined;
  private readonly policy: RedactPolicy | undefined;
  private closed = false;

  constructor(options: CassetteOptions) {
    this.mode = options.mode ?? "off";
    this.capture = options.capture ?? "metadata";
    this.onUnconsumed = options.onUnconsumed ?? "fail";
    this.fixturePath = options.fixture;
    this.policy = options.redactPolicy;
    if (this.mode === "record") {
      if (!this.fixturePath)
        throw new CassetteError(
          "cassette_schema",
          "record mode requires a fixture path to write",
        );
      assertFreshRecordTarget(this.fixturePath);
      this.recorder = new Recorder(this.capture, {
        policy: this.policy,
        maxEffects: options.maxEffects,
      });
    }
    if (this.mode === "replay" || this.mode === "verify") {
      if (!this.fixturePath)
        throw new CassetteError(
          "cassette_schema",
          `${this.mode} mode requires a fixture path`,
        );
      this.fixturePromise = loadFixture(this.fixturePath).then((fixture) => {
        this.replayer = new Replayer(fixture);
      });
    }
  }

  /** Await fixture load; surfaces schema failures early and once. */
  private async ready(): Promise<Replayer> {
    if (!this.fixturePromise)
      throw new CassetteError(
        "cassette_live_call",
        "no fixture attached for this mode",
      );
    await this.fixturePromise;
    if (!this.replayer)
      throw new CassetteError("cassette_schema", "fixture failed to load");
    return this.replayer;
  }

  /**
   * Wrap a provider client. In `record` the live client executes and effects
   * are captured. In `replay`/`verify` the wrapper answers from the fixture —
   * in replay the wrapped client is NEVER touched (a throwing client proves
   * the isolation); in verify it is called and the normalized trace compared.
   */
  provider(inner: AiProviderClient): AiProviderClient {
    if (this.mode === "off") return inner;
    return {
      id: inner.id,
      kind: inner.kind,
      ...(inner.protocol !== undefined ? { protocol: inner.protocol } : {}),
      ...(inner.tracksTransportRequests !== undefined
        ? { tracksTransportRequests: inner.tracksTransportRequests }
        : {}),
      // The Responses continuation system authenticates the CLIENT object by
      // this trusted identity; a wrapper that dropped it would turn every
      // recording of a responses provider into `invalid_provider_connection`.
      // It carries no credentials — only provider/protocol/connection/generation.
      ...(inner.connectionIdentity !== undefined
        ? { connectionIdentity: inner.connectionIdentity }
        : {}),
      capabilities: (signal, model) => this.capabilities(inner, signal, model),
      listModels: (signal) => this.listModels(inner, signal),
      streamChat: (request) => this.streamChat(inner, request),
    };
  }

  private async capabilities(
    inner: AiProviderClient,
    signal: AbortSignal | undefined,
    model: string | undefined,
  ): Promise<AiProviderCapabilities> {
    if (this.mode === "record") {
      const value = await inner.capabilities(signal, model);
      this.recorder?.recordCapabilities(model, value);
      return value;
    }
    const replayer = await this.ready();
    return replayer.expectCapabilities(model)
      .capabilities as AiProviderCapabilities;
  }

  private async listModels(
    inner: AiProviderClient,
    signal: AbortSignal | undefined,
  ): Promise<AiProviderModel[]> {
    if (this.mode === "record") {
      const value = await inner.listModels(signal);
      this.recorder?.recordModels(value);
      return value;
    }
    const replayer = await this.ready();
    return replayer.expectModels().models as AiProviderModel[];
  }

  private streamChat(
    inner: AiProviderClient,
    request: AiChatRequest,
  ): AsyncIterable<AiRunEvent> {
    if (this.mode === "off") return inner.streamChat(request);
    const cassette = this;
    return {
      async *[Symbol.asyncIterator]() {
        const view = providerRequestView(
          inner.id,
          inner.kind,
          request,
          cassette.capture,
          cassette.policy,
        );
        if (cassette.mode === "record") {
          const recorder = cassette.recorder;
          if (!recorder)
            throw new CassetteError("cassette_closed", "recorder missing");
          const seen: AiRunEvent[] = [];
          try {
            for await (const event of inner.streamChat(request)) {
              seen.push(event);
              yield event;
            }
          } catch (cause) {
            const classified = classifyError(cause);
            recorder.recordProvider({
              request: view,
              events: seen,
              outcome:
                classified.type === "abort"
                  ? {
                      type: "abort",
                      events: recorder.sanitized(seen),
                      ...(classified.message !== undefined
                        ? {
                            message: String(
                              scrubDeep(classified.message, 0, cassette.policy),
                            ),
                          }
                        : {}),
                    }
                  : {
                      type: "error",
                      errorCode: classified.errorCode ?? "provider_error",
                      events: recorder.sanitized(seen),
                      ...(classified.message !== undefined
                        ? {
                            message: String(
                              scrubDeep(classified.message, 0, cassette.policy),
                            ),
                          }
                        : {}),
                    },
            });
            throw cause;
          }
          recorder.recordProvider({
            request: view,
            events: seen,
            outcome: recorder.recordedEvents(seen),
          });
          return;
        }
        const replayer = await cassette.ready();
        const { effect } = replayer.expectProvider(view);
        if (cassette.mode === "verify") {
          const seen: AiRunEvent[] = [];
          try {
            for await (const event of inner.streamChat(request)) {
              seen.push(event);
              yield event;
            }
          } catch (cause) {
            const classified = classifyError(cause);
            cassette.assertProviderTrace(
              effect,
              seen,
              classified.type,
              replayer,
            );
            throw cause;
          }
          cassette.assertProviderTrace(effect, seen, undefined, replayer);
          return;
        }
        // Metadata capture hashes argumentsJson out of stream events; the
        // loop assembles tool calls from those events (both
        // run.tool.requested and run.message.completed.toolCalls), so
        // restore them by args hash from the tool effects queued behind
        // this provider effect.
        const pendingArgs = replayer.pendingToolArgsByHash();
        const restoreArgs = (recorded: RecordedRunEvent): RecordedRunEvent => {
          if (pendingArgs.size === 0) return recorded;
          const data = recorded.data as Record<string, unknown> | undefined;
          if (data === undefined || typeof data !== "object") return recorded;
          const withArgs = (
            entry: Record<string, unknown>,
          ): Record<string, unknown> => {
            if (typeof entry.argumentsJson === "string") return entry;
            const hash = entry.argsSha256 ?? data.argsSha256;
            if (typeof hash !== "string") return entry;
            const args = pendingArgs.get(hash);
            return args === undefined
              ? entry
              : { ...entry, argumentsJson: JSON.stringify(args) };
          };
          if (recorded.type === "run.tool.requested") {
            const patched = withArgs(data);
            return patched === data ? recorded : { ...recorded, data: patched };
          }
          if (recorded.type === "run.message.completed") {
            const calls = data.toolCalls;
            if (!Array.isArray(calls) || calls.length === 0) return recorded;
            let changed = false;
            const restored = calls.map((call) => {
              if (typeof call !== "object" || call === null) return call;
              const patched = withArgs(call as Record<string, unknown>);
              if (patched !== call) changed = true;
              return patched;
            });
            return changed
              ? { ...recorded, data: { ...data, toolCalls: restored } }
              : recorded;
          }
          return recorded;
        };
        if (effect.outcome.type === "error") {
          let pre = 0;
          for (const recorded of effect.outcome.events ?? [])
            yield replayEvent(request.runId, pre++, restoreArgs(recorded));
          throw Object.assign(
            new Error(effect.outcome.message ?? effect.outcome.errorCode),
            { name: effect.outcome.errorCode },
          );
        }
        if (effect.outcome.type === "abort") {
          let pre = 0;
          for (const recorded of effect.outcome.events ?? [])
            yield replayEvent(request.runId, pre++, restoreArgs(recorded));
          throw abortError(effect.outcome.message);
        }
        let seq = 0;
        for (const recorded of effect.outcome.events)
          yield replayEvent(request.runId, seq++, restoreArgs(recorded));
      },
    };
  }

  private assertProviderTrace(
    effect: { effectId: string; trace: { steps: string[] } },
    liveEvents: AiRunEvent[],
    terminalStep: string | undefined,
    replayer: Replayer,
  ): void {
    const expected = replayer.traceAt(replayer.consumed - 1);
    if (!expected) return;
    const legacyTypeOnly = expected.steps.every((step) => !step.includes("|"));
    const liveTypes = liveEvents.map((event) =>
      legacyTypeOnly ? event.type : eventStep(event),
    );
    if (terminalStep !== undefined) liveTypes.push(terminalStep);
    if (JSON.stringify(expected.steps) !== JSON.stringify(liveTypes))
      throw new CassetteError(
        "cassette_mismatch",
        `live provider stream trace differs from the fixture: ` +
          `live=[${liveTypes.join(",")}] recorded=[${expected.steps.join(",")}]`,
        { effectId: effect.effectId },
      );
    // Usage: the recorded trace pins token counts; the live stream must agree.
    if (expected.usage !== undefined) {
      const liveUsage = liveEvents.find((e) => e.type === "run.usage")?.data as
        | Record<string, unknown>
        | undefined;
      for (const key of [
        "promptTokens",
        "completionTokens",
        "cachedPromptTokens",
        "reasoningTokens",
      ] as const) {
        const want = expected.usage[key];
        if (want === undefined) continue;
        if (liveUsage?.[key] !== want)
          throw new CassetteError(
            "cassette_mismatch",
            `live usage ${key}=${String(liveUsage?.[key])} differs from recorded ${want}`,
            { effectId: effect.effectId },
          );
      }
    }
  }

  /**
   * Wrap one tool. `record` executes live and captures the outcome (result,
   * suspension, error, abort). `replay` never executes the tool body — it
   * matches identity and reconstitutes the recorded result. `verify` executes
   * live and compares the outcome kind and definition fingerprint.
   */
  wrapTool(tool: AiTool): AiTool {
    if (this.mode === "off") return tool;
    const definitionFingerprint = fingerprintToolDefinitions([tool.definition]);
    return {
      definition: tool.definition,
      // Contract convention: `execute(ctx, input)` — context first.
      execute: async (context, args) => {
        const call = fingerprintToolCall(tool.definition.name, args);
        if (this.mode === "record") {
          const recorder = this.recorder;
          if (!recorder)
            throw new CassetteError("cassette_closed", "recorder missing");
          const tracked = this.trackSuspend(context);
          try {
            const result = await tool.execute(tracked.context, args);
            const suspended = tracked.getSuspended();
            recorder.recordTool({
              name: tool.definition.name,
              definition: tool.definition,
              args,
              argsSha256: call.argsSha256,
              ...(suspended
                ? {}
                : { resultSha256: recorder.resultHash(result) }),
              outcome: suspended
                ? {
                    type: "suspended",
                    prompt: suspended.prompt ?? "",
                  }
                : recorder.outcomeFromResult(result),
            });
            return result;
          } catch (cause) {
            const classified = classifyError(cause);
            recorder.recordTool({
              name: tool.definition.name,
              definition: tool.definition,
              args,
              argsSha256: call.argsSha256,
              outcome:
                classified.type === "abort"
                  ? {
                      type: "abort",
                      ...(classified.message !== undefined
                        ? {
                            message: String(
                              scrubDeep(classified.message, 0, this.policy),
                            ),
                          }
                        : {}),
                    }
                  : {
                      type: "error",
                      errorCode: classified.errorCode ?? "tool_error",
                      ...(classified.message !== undefined
                        ? {
                            message: String(
                              scrubDeep(classified.message, 0, this.policy),
                            ),
                          }
                        : {}),
                    },
            });
            throw cause;
          }
        }
        const replayer = await this.ready();
        const { effect } = replayer.expectTool({
          name: tool.definition.name,
          argsSha256: call.argsSha256,
        });
        if (this.mode === "verify") {
          try {
            const result = await tool.execute(context, args);
            this.assertToolOutcome(
              effect,
              "result",
              definitionFingerprint,
              result,
            );
            return result;
          } catch (cause) {
            const classified = classifyError(cause);
            this.assertToolOutcome(
              effect,
              classified.type,
              definitionFingerprint,
            );
            throw cause;
          }
        }
        if (effect.outcome.type === "error")
          throw Object.assign(
            new Error(effect.outcome.message ?? effect.outcome.errorCode),
            { name: effect.outcome.errorCode },
          );
        if (effect.outcome.type === "abort")
          throw abortError(effect.outcome.message);
        if (effect.outcome.type === "suspended") {
          // Reproduce the recorded interaction: park the call exactly as the
          // recording did. The loop ignores the return value once suspended.
          context.suspend?.({ prompt: effect.outcome.prompt });
          return neutralToolResult();
        }
        return asToolResult(effect.outcome.result);
      },
    };
  }

  private trackSuspend(context: AiToolExecutionContext): {
    context: AiToolExecutionContext;
    getSuspended(): { prompt?: string } | undefined;
  } {
    if (!context.suspend) return { context, getSuspended: () => undefined };
    let request: { prompt?: string } | undefined;
    const original = context.suspend;
    const wrapped: AiToolExecutionContext = {
      ...context,
      suspend: (suspendRequest) => {
        request = { prompt: suspendRequest?.prompt };
        original(suspendRequest);
      },
    };
    return { context: wrapped, getSuspended: () => request };
  }

  private assertToolOutcome(
    effect: RecordedToolEffect,
    liveKind: string,
    definitionFingerprint: string,
    liveResult?: AiToolResult,
  ): void {
    if (effect.tool.definitionFingerprint !== definitionFingerprint)
      throw new CassetteError(
        "cassette_mismatch",
        `tool ${effect.tool.name} definition changed since recording`,
        { effectId: effect.effectId },
      );
    const expected = effect.trace.steps[0];
    if (expected !== liveKind)
      throw new CassetteError(
        "cassette_mismatch",
        `tool ${effect.tool.name} outcome differs from the fixture: ` +
          `live=${liveKind} recorded=${String(expected)}`,
        { effectId: effect.effectId },
      );
    // Result-content check under BOTH captures: the fixture never needs to
    // hold the payload, only its metadata-normalized hash.
    if (effect.resultSha256 !== undefined && liveResult !== undefined) {
      const liveHash = sha256(
        canonicalJson(recordToolResult(liveResult, "metadata")),
      );
      if (liveHash !== effect.resultSha256)
        throw new CassetteError(
          "cassette_mismatch",
          `tool ${effect.tool.name} result differs from the recording (hash)`,
          { effectId: effect.effectId },
        );
    }
  }

  tools(tools: AiTool[]): AiTool[] {
    return tools.map((tool) => this.wrapTool(tool));
  }

  /** Wrap one tool — the common case; avoids `[0]` at every call site. */
  tool(tool: AiTool): AiTool {
    return this.wrapTool(tool);
  }

  instrumentRegistry(registry: AiToolRegistry): AiToolRegistry {
    const wrapped = new AiToolRegistry();
    for (const tool of registry.list()) wrapped.register(this.wrapTool(tool));
    return wrapped;
  }

  summary(): CassetteSummary {
    return {
      mode: this.mode,
      capture: this.capture,
      recorded: this.recorder?.snapshot().length ?? 0,
      consumed: this.replayer?.consumed ?? 0,
      unconsumed: this.replayer?.unconsumed ?? 0,
    };
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    if (this.mode === "record" && this.recorder && this.fixturePath) {
      const fixture: CassetteFixture = {
        schemaVersion: 1,
        capture: this.capture,
        createdAt: new Date().toISOString(),
        generator: { name: "@agentkit/cassette" },
        effects: this.recorder.snapshot(),
      };
      // Self-validate before writing: a broken recorder fails the recording
      // run, not every future replay. Errors name only the path — never the
      // fixture's contents.
      const text = `${JSON.stringify(fixture, null, 2)}\n`;
      parseFixtureJson(text, this.fixturePath);
      if (text.length > MAX_FIXTURE_BYTES)
        throw new CassetteError(
          "cassette_limit",
          `fixture for ${this.fixturePath} exceeds the ${MAX_FIXTURE_BYTES}-byte cap; lower maxEffects or use metadata capture`,
        );
      await mkdir(dirname(this.fixturePath), { recursive: true });
      // Atomic, exclusive publication: a temp file written under 0600 (fixtures
      // can be sensitive even scrubbed), fsynced, then HARD-LINKED — link(2)
      // fails EEXIST instead of overwriting, and there is no check-then-write
      // window for another file to appear at the target.
      const temp = `${this.fixturePath}.tmp-${process.pid}-${randomUUID()}`;
      const handle = await open(temp, "wx", 0o600);
      try {
        await handle.writeFile(text, "utf8");
        await handle.sync();
      } finally {
        await handle.close();
      }
      try {
        await link(temp, this.fixturePath);
      } catch (cause) {
        const code = (cause as { code?: string }).code;
        throw new CassetteError(
          "cassette_schema",
          code === "EEXIST"
            ? `record target ${this.fixturePath} already exists; recordings never overwrite a fixture — remove it explicitly to re-record`
            : `cannot publish fixture ${this.fixturePath}: ${(cause as Error).message}`,
        );
      } finally {
        await unlink(temp).catch(() => undefined);
      }
      return;
    }
    if (this.mode === "replay" || this.mode === "verify") {
      // Await the fixture: the unconsumed gate must apply even when no
      // effect was ever requested (the load itself may also fail).
      const replayer = await this.ready();
      if (this.onUnconsumed === "fail" && replayer.unconsumed > 0)
        throw new CassetteError(
          "cassette_extra",
          `${replayer.unconsumed} recorded effect(s) were never consumed; ` +
            "the run did not reproduce the recorded execution",
          { unconsumed: replayer.unconsumed },
        );
    }
  }
}

export interface RunWithCassetteOptions<T> {
  cassette: Cassette;
  execute: () => Promise<T>;
}

/**
 * Create a cassette. `off` is the default and adds nothing: `provider()` and
 * `tools()` return their inputs untouched, and `close()` is a no-op.
 */
export function createCassette(options: CassetteOptions = {}): Cassette {
  return new Cassette(options);
}

/**
 * Run `execute` with the cassette finalized afterwards. Finalization is the
 * security/strictness gate (fixture persistence, unconsumed-effect failure),
 * so it runs even when `execute` throws.
 */
export async function runWithCassette<T>(
  options: RunWithCassetteOptions<T>,
): Promise<T> {
  let failure: unknown;
  let result: T | undefined;
  try {
    result = await options.execute();
  } catch (cause) {
    failure = cause;
  }
  try {
    await options.cassette.close();
  } catch (closeCause) {
    if (failure === undefined) failure = closeCause;
  }
  if (failure !== undefined) throw failure;
  return result as T;
}

export type { Cassette };
