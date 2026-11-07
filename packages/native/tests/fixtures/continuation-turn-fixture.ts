import { testServiceOptions } from "../helpers/test-profiles.js";
/**
 * One application process of the provider-continuity scenario: registers
 * the provider connection itself, opens the on-disk store, runs one turn
 * through the Rust provider lane, and reports the settled task. It exists
 * as a separate Bun process because the W3a witness is precisely that a
 * SECOND process — with a fresh binding-instance connection registry —
 * resumes the continuation the first process stored. In this repo's
 * binding the connection registry is process-global (`CONNECTIONS` in
 * `rust/crates/napi/src/provider_connection.rs`), so only a real restart
 * reproduces the reviewer's defect.
 *
 * Usage: `bun <this file> <optionsJson>` with
 * ```ts
 * {
 *   storePath: string;
 *   baseUrl: string;
 *   apiKey: string | null;
 *   phase: 1 | 2 | 3;             // phase 1 creates the session
 *   chatId: string;
 *   connectionKey?: string;       // durable logical connection identity
 *   providerId?: string;          // default "continuity"
 *   revokeAndReRegister?: boolean; // revoke then re-register before the turn
 *   restoreContinuity?: { expectedGeneration: number; previousGeneration: number; confirmUnchangedScope: true };
 * }
 * ```
 * Prints `RESULT <json>` with `{ connectionId, turnId, status, error }`
 * and exits 0 whenever the turn settled (success OR failure), or an explicitly
 * expected admission refusal occurred; exit 2 on harness trouble.
 */
import {
  NativeAgentError,
  NativeSessionService,
  NativeStore,
} from "../../src/index.js";

interface Options {
  storePath: string;
  baseUrl: string;
  apiKey: string | null;
  phase: number;
  chatId: string;
  connectionKey?: string;
  providerId?: string;
  expectAdmissionError?: "unknown_provider";
  revokeAndReRegister?: boolean;
  restoreContinuity?: {
    expectedGeneration: number;
    previousGeneration: number;
    confirmUnchangedScope: true;
  };
  /** Register a second, keyless (process-local) connection instead of the
   * keyed one — the scope-check pin: continuations bound to a durable
   * logical identity must still be REJECTED by any other connection. */
  foreignConnection?: boolean;
}

const options = JSON.parse(process.argv[2] ?? "") as Options;

async function main(): Promise<number> {
  const registration =
    options.connectionKey === undefined || options.foreignConnection === true
      ? undefined
      : {
          connectionKey: options.connectionKey,
          storePath: options.storePath,
          accountScope: "continuity-account",
        };
  const store = await NativeStore.open(options.storePath);
  const register = async (
    restoreContinuity?: Options["restoreContinuity"],
  ): Promise<string> =>
    NativeSessionService.registerProviderConnection(
      options.baseUrl,
      options.apiKey,
      registration === undefined
        ? undefined
        : { ...registration, restoreContinuity },
    );
  let connectionId = await register(
    options.revokeAndReRegister ? undefined : options.restoreContinuity,
  );
  if (options.revokeAndReRegister === true) {
    if (!(await NativeSessionService.revokeProviderConnection(connectionId)))
      return 2;
    connectionId = await register(options.restoreContinuity);
  }
  let service: NativeSessionService | undefined;
  try {
    service = await store.createService(
      testServiceOptions({
        providerConfig: {
          protocol: "responses",
          providerId: options.providerId ?? "continuity",
          connectionId,
          model: "model-c",
        },
      }),
    );
    if (options.phase === 1) {
      await service.createSession({
        sessionId: options.chatId,
        profileId: "p-continuity",
        model: "model-c",
      });
    }
    const tasksBefore =
      options.expectAdmissionError === undefined
        ? undefined
        : await store.callStore<unknown[]>("tasks.listByScope", {
            scopeId: options.chatId,
          });
    let sent: Awaited<ReturnType<NativeSessionService["send"]>>;
    try {
      sent = await service.send(options.chatId, {
        content: `turn for phase ${options.phase}`,
      });
    } catch (error) {
      if (
        options.expectAdmissionError === undefined ||
        !(error instanceof NativeAgentError) ||
        error.message !== options.expectAdmissionError
      )
        throw error;
      const tasksAfter = await store.callStore<unknown[]>("tasks.listByScope", {
        scopeId: options.chatId,
      });
      console.log(
        `RESULT ${JSON.stringify({ connectionId, turnId: "", status: "rejected", error: error.message, errorCode: error.code, taskCountBefore: tasksBefore?.length, taskCountAfter: tasksAfter.length })}`,
      );
      return 0;
    }
    if (options.expectAdmissionError !== undefined)
      throw new Error("Expected provider admission refusal.");
    if (sent.kind !== "turn") return 2;
    const turnId = sent.turnId;
    // No cursor: afterSeq is EXCLUSIVE; a passing admission event owns
    // seq 0 (emitted before run.started) and `0` would skip it.
    const subscription = await service.subscribeTurn(turnId);
    let settled: { status?: string; error?: string } | undefined;
    const started = Date.now();
    while (Date.now() - started < 20_000) {
      const batch = await subscription.poll(64, 100);
      for (const update of batch.events as {
        type?: string;
        task?: { status?: string; error?: string };
      }[]) {
        if (update.type === "settled" && update.task) settled = update.task;
      }
      if (batch.done) break;
    }
    subscription.cancel();
    console.log(
      `RESULT ${JSON.stringify({
        connectionId,
        turnId,
        status: settled?.status ?? "unsettled",
        error: settled?.error ?? null,
      })}`,
    );
    return 0;
  } finally {
    await service?.close();
    await store.close();
  }
}

main().then(
  (code) => process.exit(code),
  (error) => {
    console.error(`CHILD FAILURE ${String(error)}`);
    process.exit(2);
  },
);
