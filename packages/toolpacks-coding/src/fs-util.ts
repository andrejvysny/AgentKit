import { createHash, randomBytes } from "node:crypto";
import { constants } from "node:fs";
import { chmod, lstat, open, rename, stat, unlink } from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import { CodingError } from "./errors.js";

export function sha256(data: Buffer | string): string {
  return createHash("sha256").update(data).digest("hex");
}

function errno(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error
    ? String((error as { code: unknown }).code)
    : undefined;
}

/**
 * Read one regular file the confinement already resolved. `O_NOFOLLOW`
 * refuses a symlink swapped in after the check; the dev/ino comparison
 * refuses a different file swapped in between lstat and open.
 */
export async function readRegularFile(
  path: string,
  label: string,
  maxBytes: number,
): Promise<Buffer> {
  let handle: Awaited<ReturnType<typeof open>> | undefined;
  try {
    const before = await lstat(path);
    if (!before.isFile())
      throw new CodingError("not_a_file", `${label} is not a regular file`);
    handle = await open(
      path,
      constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK,
    );
    const opened = await handle.stat();
    if (
      !opened.isFile() ||
      opened.dev !== before.dev ||
      opened.ino !== before.ino
    )
      throw new CodingError("file_changed", `${label} changed while opening`);
    if (opened.size > maxBytes)
      throw new CodingError(
        "file_too_large",
        `${label} is ${opened.size} bytes; the limit is ${maxBytes}`,
      );
    const buffer = Buffer.alloc(opened.size);
    let offset = 0;
    while (offset < buffer.length) {
      const { bytesRead } = await handle.read(
        buffer,
        offset,
        buffer.length - offset,
        offset,
      );
      if (bytesRead === 0) break;
      offset += bytesRead;
    }
    return buffer.subarray(0, offset);
  } catch (error) {
    if (error instanceof CodingError) throw error;
    const code = errno(error);
    if (code === "ENOENT")
      throw new CodingError("not_found", `${label} does not exist`);
    if (code === "ELOOP")
      throw new CodingError("path_symlink", `${label} is a symlink; refused`);
    throw error;
  } finally {
    await handle?.close();
  }
}

/** Strict UTF-8 decode that keeps a BOM; `undefined` when the bytes are not UTF-8. */
export function decodeUtf8(buffer: Buffer): string | undefined {
  try {
    return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(
      buffer,
    );
  } catch {
    return undefined;
  }
}

export function looksBinary(buffer: Buffer): boolean {
  return buffer.subarray(0, 8192).includes(0);
}

/** Write via a temp file in the same directory, then rename over the target. */
export async function atomicWrite(
  path: string,
  data: Buffer,
  label: string,
): Promise<void> {
  const existing = await stat(path).catch(() => null);
  if (existing && !existing.isFile())
    throw new CodingError("not_a_file", `${label} is not a regular file`);
  const temp = join(
    dirname(path),
    `.${basename(path)}.${randomBytes(6).toString("hex")}.tmp`,
  );
  try {
    const handle = await open(
      temp,
      constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL,
      0o666,
    );
    try {
      await handle.writeFile(data);
      await handle.sync();
    } finally {
      await handle.close();
    }
    if (existing) await chmod(temp, existing.mode & 0o7777);
    await rename(temp, path);
  } catch (error) {
    await unlink(temp).catch(() => undefined);
    throw error;
  }
}
