import { randomUUID } from "node:crypto";
import { link, open, rename, unlink } from "node:fs/promises";
import { dirname } from "node:path";
import { code, type LockRecord } from "./coordination-record.js";

/** The previous marker stays present until its durable replacement is ready. */
export async function persistRecord(
  path: string,
  record: LockRecord,
  exclusive = false,
): Promise<void> {
  await persistJsonRecord(path, record, exclusive);
}

/** Publish durable recovery evidence without reopening the mutable ownership marker. */
export async function persistJsonRecord(
  path: string,
  record: unknown,
  exclusive = false,
): Promise<void> {
  const temp = `${path}.update-${randomUUID()}`;
  const file = await open(temp, "wx", 0o600);
  try {
    await file.writeFile(JSON.stringify(record));
    await file.sync();
  } finally {
    await file.close();
  }
  try {
    if (exclusive) {
      try {
        await link(temp, path);
      } catch (error) {
        if (code(error) !== "EEXIST") throw error;
      }
    } else await rename(temp, path);
    const directory = await open(dirname(path), "r");
    try {
      await directory.sync();
    } finally {
      await directory.close();
    }
  } finally {
    await unlink(temp).catch((error: NodeJS.ErrnoException) => {
      if (error.code !== "ENOENT") throw error;
    });
  }
}
