import { unlink } from "node:fs/promises";
import { withRecordClaim } from "../../src/coordination-claim.js";
import { readRecord } from "../../src/coordination-record.js";

const path = process.argv[2]!;
const boundary = process.argv[3]!;
const record = await readRecord(path);
if (record === "gone" || record === "corrupt")
  throw new Error("Missing test marker");
const barrier = async (): Promise<void> => {
  process.stdout.write("ready\n");
  await new Promise<void>((resolve) => {
    process.stdin.once("data", () => resolve());
  });
};
await withRecordClaim(
  path,
  record,
  async () => {
    if (boundary === "before-removal") await barrier();
    await unlink(path);
    if (boundary === "after-removal") await barrier();
  },
  { afterClaim: boundary === "after-claim" ? barrier : undefined },
);
