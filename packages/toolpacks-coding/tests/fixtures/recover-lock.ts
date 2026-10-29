import { recoverWorkspaceMarker } from "../../src/coordination-recovery.js";

const path = process.argv[2]!;
const boundary = process.argv[3]!;
const barrier = async (): Promise<void> => {
  process.stdout.write("ready\n");
  await new Promise<void>((resolve) => {
    process.stdin.once("data", () => resolve());
  });
};
await recoverWorkspaceMarker(path, "exclusive", {
  afterVerification: boundary === "after-verification" ? barrier : undefined,
  afterPersistence: boundary === "after-persistence" ? barrier : undefined,
  afterSeal: boundary === "after-seal" ? barrier : undefined,
  afterKill: async (index, total) => {
    if (
      boundary === `after-kill-${index}` ||
      (boundary === "after-kill-last" && index === total)
    )
      await barrier();
  },
});
