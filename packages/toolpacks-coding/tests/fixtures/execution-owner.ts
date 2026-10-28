import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createLockDirWorkspaceCoordinator } from "../../src/coordination-lockdir.js";
import { isExecutionLease } from "../../src/coordination.js";

const [dir, root, mode] = process.argv.slice(2);
if (!dir || !root) throw new Error("usage: <dir> <root> <intent|process>");
const lease = await createLockDirWorkspaceCoordinator({ dir }).acquireWorkspace(
  {
    root,
    holder: {
      runId: "owner-run",
      toolCallId: "owner-call",
      tool: "run_command",
      label: "fixture",
    },
  },
);
if (!isExecutionLease(lease)) throw new Error("missing durable ownership");
const executionAttemptId = randomUUID();
const execId = `${process.pid}:${executionAttemptId}`;
await lease.beginExecution({
  runId: "owner-run",
  toolCallId: "owner-call",
  executionAttemptId,
  execId,
});
if (mode === "process") {
  const child = spawn(
    process.execPath,
    ["-e", "setTimeout(() => {}, 300000)"],
    {
      env: { ...process.env, AGENTKIT_EXEC_ID: execId },
      detached: true,
      stdio: "ignore",
    },
  );
  if (!child.pid) throw new Error("fixture failed to spawn");
  await lease.spawned({ pid: child.pid });
  process.stdout.write(`ready:${child.pid}\n`);
} else process.stdout.write("ready:0\n");
setInterval(() => {}, 1000);
