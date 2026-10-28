import { scanMarkedIdentities } from "../../src/process-tree.js";

const execId = process.argv[2];
if (!execId) throw new Error("usage: <execId>");
const census = await scanMarkedIdentities();
const own = census.processes.filter((process) => process.execId === execId);
process.stdout.write(
  `${JSON.stringify({
    complete: census.complete,
    ownerPids: [...new Set(own.map((process) => process.ownerPid))],
    guardians: own
      .filter((process) => process.guardian)
      .map((process) => process.pid),
  })}\n`,
);
