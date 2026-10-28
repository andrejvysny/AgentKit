import { mkdir, writeFile } from "node:fs/promises";
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
  fingerprintToolCall,
  fingerprintToolDefinitions,
  providerRequestView,
} from "./fingerprint.js";
import { Recorder } from "./recorder.js";
import { Replayer } from "./replayer.js";
import type {
  CassetteCapture,
  CassetteFixture,
  CassetteMode,
  CassetteOptions,
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
  private readonly strict: boolean;
  private readonly onUnconsumed: "fail" | "ignore";
  private readonly fixturePath?: string;
  private readonly recorder: Recorder | undefined;
  private replayer: Replayer | undefined;
  private readonly fixturePromise: Promise<void> | undefined;
  private closed = false;

  constructor(options: CassetteOptions) {
    this.mode = options.mode ?? "off";
    this.capture = options.capture ?? "metadata";
    this.strict = options.strict ?? true;
    this.onUnconsumed = options.onUnconsumed ?? "fail";
    this.fixturePath = options.fixture;
    if (this.mode === "record") {
      if (!this.fixturePath)
        throw new CassetteError(
          "cassette_schema",
          "record mode requires a fixture path to write",
        );
      assertFreshRecordTarget(this.fixturePath);
      this.recorder = new Recorder(this.capture);
    }
    if (this.mode === "replay" || this.mode === "verify") {
      if (!this.fixturePath)
        throw new CassetteError(
          "cassette_schema",
          `${this.mode} mode requires a fixture path`,
        );
      this.fixturePromise = loadFixture(this.fixturePath).then((fixture) => {
        this.replayer = new Replayer(fixture, this.strict);
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
                        ? { message: classified.message }
                        : {}),
                    }
                  : {
                      type: "error",
                      errorCode: classified.errorCode ?? "provider_error",
                      events: recorder.sanitized(seen),
                      ...(classified.message !== undefined
                        ? { message: classified.message }
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
          const seenTypes: string[] = [];
          try {
            for await (const event of inner.streamChat(request)) {
              seenTypes.push(event.type);
              yield event;
            }
          } catch (cause) {
            const classified = classifyError(cause);
            cassette.assertProviderTrace(
              effect,
              seenTypes.concat(classified.type),
              replayer,
            );
            throw cause;
          }
          cassette.assertProviderTrace(effect, seenTypes, replayer);
          return;
        }
        if (effect.outcome.type === "error") {
          let pre = 0;
          for (const recorded of effect.outcome.events ?? [])
            yield replayEvent(request.runId, pre++, recorded);
          throw Object.assign(
            new Error(effect.outcome.message ?? effect.outcome.errorCode),
            { name: effect.outcome.errorCode },
          );
        }
        if (effect.outcome.type === "abort") {
          let pre = 0;
          for (const recorded of effect.outcome.events ?? [])
            yield replayEvent(request.runId, pre++, recorded);
          throw abortError(effect.outcome.message);
        }
        let seq = 0;
        for (const recorded of effect.outcome.events)
          yield replayEvent(request.runId, seq++, recorded);
      },
    };
  }

  private assertProviderTrace(
    effect: { effectId: string; trace: { steps: string[] } },
    liveTypes: string[],
    replayer: Replayer,
  ): void {
    const expected = replayer.traceAt(replayer.consumed - 1);
    if (!expected) return;
    if (JSON.stringify(expected.steps) !== JSON.stringify(liveTypes))
      throw new CassetteError(
        "cassette_mismatch",
        `live provider stream trace differs from the fixture: ` +
          `live=[${liveTypes.join(",")}] recorded=[${expected.steps.join(",")}]`,
        { effectId: effect.effectId },
      );
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
                        ? { message: classified.message }
                        : {}),
                    }
                  : {
                      type: "error",
                      errorCode: classified.errorCode ?? "tool_error",
                      ...(classified.message !== undefined
                        ? { message: classified.message }
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
            this.assertToolOutcome(effect, "result", definitionFingerprint);
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
      // run, not every future replay.
      const text = `${JSON.stringify(fixture, null, 2)}\n`;
      parseFixtureJson(text, this.fixturePath);
      await mkdir(dirname(this.fixturePath), { recursive: true });
      await writeFile(this.fixturePath, text, "utf8");
      return;
    }
    if (this.mode === "replay" || this.mode === "verify") {
      // Await the fixture: strictness must apply even when no effect was
      // ever requested (the load itself may also fail).
      const replayer = await this.ready();
      if (
        this.strict &&
        this.onUnconsumed === "fail" &&
        replayer.unconsumed > 0
      )
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
