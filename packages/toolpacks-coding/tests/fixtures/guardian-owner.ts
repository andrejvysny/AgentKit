import { createLockDirWorkspaceCoordinator } from "../../src/coordination-lockdir.js";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import { isExecutionLease } from "../../src/coordination.js";
import { captureProcessIdentity } from "../../src/process-identity.js";
import {
  launchGuardian,
  GUARDIAN_PROTOCOL,
} from "../../src/process-launcher.js";

const dir = process.argv[2]!;
const root = process.argv[3]!;
const coordinator = createLockDirWorkspaceCoordinator({ dir });
const lease = await coordinator.acquireWorkspace({
  root,
  holder: { runId: "guardian-run", tool: "run_command", label: "fixture" },
});
if (!isExecutionLease(lease)) throw new Error("Missing execution lease");
const execId = `${process.pid}:guardian-attempt`;
await lease.beginExecution({
  runId: "guardian-run",
  executionAttemptId: "guardian-attempt",
  execId,
});
await writeFile(
  join(root, "writer.sh"),
  "#!/bin/bash\nprintf 'START\\n' >> sentinel\nprintf 'ready\\n'\nsleep 60\n",
);
const launch = launchGuardian(root, execId, {
  file: "/bin/bash",
  args: ["-c", "bash writer.sh"],
  cwd: root,
  env: { PATH: "/usr/bin:/bin", AGENTKIT_EXEC_ID: execId },
});
const identity = await captureProcessIdentity(launch.child.pid!);
if (!identity) throw new Error("Guardian identity unavailable");
await lease.spawned({ ...identity, guardianProtocol: GUARDIAN_PROTOCOL });
await lease.observed?.([identity], true, false);
launch.child.stdout!.on("data", (data) => {
  if (String(data).includes("ready")) process.stdout.write("ready\n");
});
launch.release();
await new Promise<void>(() => {});
