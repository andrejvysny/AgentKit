/**
 * The process the crash test kills. It runs ONE real long command through the
 * coding pack and then answers — and it is SIGKILLed while the command is in
 * flight, so the mutating call is genuinely unsettled: the side effect happened,
 * the result never got recorded, and no in-process cleanup runs.
 *
 * argv: <repo> <dbPath> <leaseTtlMs>
 */
import { MockProviderClient } from "@agentkit/testing";
import { CodingAgentApp } from "../../src/app.js";
import { CRASH_COMMAND } from "./crash-command.js";

const [repo = "", dbPath = "", leaseTtl = "1500"] = process.argv.slice(2);
console.error(
  `[child] repo=${repo} db=${dbPath} leaseTtl=${leaseTtl} argv=${JSON.stringify(process.argv)}`,
);
if (!repo || !dbPath)
  throw new Error("usage: crash-child <repo> <db> [leaseTtl]");

const mock = new MockProviderClient();
mock.setScript([
  {
    steps: [
      {
        kind: "tool_call",
        toolCallId: "c1",
        name: "run_command",
        // Appends a line, then sleeps: the append is the observable side effect,
        // the sleep is the window in which the host dies.
        argumentsJson: JSON.stringify({ command: CRASH_COMMAND }),
      },
    ],
  },
  { steps: [{ kind: "text", content: "the command finished" }] },
]);

const app = await CodingAgentApp.open({
  repo,
  dbPath,
  sessionId: "crash",
  profileId: "coding",
  model: "mock-model",
  baseUrl: "http://127.0.0.1:1",
  mode: "full_auto",
  leaseTtlMs: Number(leaseTtl ?? 1_500),
  providerFactory: () => mock as never,
});
await app.ensureSession();
await app.startWorker();
const started = await app.send("run the long command");
if (started.kind !== "turn")
  throw new Error("expected a turn, got a queued follow-up");
await app.waitForTurn(started.turnId, { timeoutMs: 30_000 });
await app.close();
