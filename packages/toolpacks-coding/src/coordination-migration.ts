import { createHash, randomUUID } from "node:crypto";
import { join } from "node:path";
import { readRecord } from "./coordination-record.js";
import { persistRecord } from "./coordination-storage.js";

/** Old profile-local command ownership cannot be discarded during upgrade. */
export async function importLegacyOwnership(
  dir: string,
  key: string,
  legacyDirs: readonly string[],
): Promise<void> {
  for (const legacyDir of legacyDirs) {
    const path = join(legacyDir, key, "exclusive.lock");
    const old = await readRecord(path);
    if (
      old === "gone" ||
      (old !== "corrupt" && old.execution?.cleanup?.state === "stopped")
    )
      continue;
    const id = `legacy-${createHash("sha256").update(path).digest("hex")}`;
    await persistRecord(
      join(dir, "quarantine.lock"),
      {
        lockId: randomUUID(),
        pid: process.pid,
        tool: "legacy_command",
        label: "unresolved legacy command ownership",
        createdAt: Date.now(),
        execution: {
          runId:
            old !== "corrupt" ? (old.execution?.runId ?? "legacy") : "legacy",
          executionAttemptId: id,
          execId: `${process.pid}:${id}`,
          cleanup: {
            executionAttemptId: id,
            state: "unknown",
            survivors: [],
            reason: "legacy command ownership has no descendant cleanup proof",
          },
        },
      },
      true,
    );
  }
}
