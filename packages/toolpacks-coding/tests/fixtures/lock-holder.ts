// Child process for coordination.test.ts: takes a lease from a lock-dir
// coordinator, reports "ready", and releases when stdin says "release".
import { createLockDirWorkspaceCoordinator } from "../../src/index.js";

const [dir, root, mode] = process.argv.slice(2);
if (!dir || !root || !mode)
  throw new Error("usage: <dir> <root> <file|workspace>");
const coordinator = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
const holder = { runId: "child", tool: "child", label: `child ${mode}` };
const lease =
  mode === "workspace"
    ? await coordinator.acquireWorkspace({ root, holder })
    : await coordinator.acquireFile({ root, path: `${root}/f.txt`, holder });
process.stdout.write("ready\n");
process.stdin.on("data", async (chunk) => {
  if (String(chunk).includes("release")) {
    await lease.release();
    process.exit(0);
  }
});
