/**
 * Serialises mutations that target the same file.
 *
 * Adapted from Pi (https://github.com/earendil-works/pi), v1.4.0, commit
 * 4a080252f06caf63a525d7873c5d94d3c23887e0, path
 * `packages/coding-agent/src/core/tools/file-mutation-queue.ts`.
 * Copyright (c) 2025 Mario Zechner. MIT License.
 * Changes: formatting only.
 */
import { realpath } from "node:fs/promises";
import { resolve } from "node:path";

const queues = new Map<string, Promise<void>>();
let registrationQueue = Promise.resolve();

function isMissingPathError(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    (error.code === "ENOENT" || error.code === "ENOTDIR")
  );
}

async function queueKey(filePath: string): Promise<string> {
  const resolved = resolve(filePath);
  try {
    return await realpath(resolved);
  } catch (error) {
    if (isMissingPathError(error)) return resolved;
    throw error;
  }
}

/** Operations on the same realpath run one at a time; others stay parallel. */
export async function withFileMutationQueue<T>(
  filePath: string,
  fn: () => Promise<T>,
): Promise<T> {
  const registration = registrationQueue.then(async () => {
    const key = await queueKey(filePath);
    const current = queues.get(key) ?? Promise.resolve();
    let releaseNext!: () => void;
    const next = new Promise<void>((done) => {
      releaseNext = done;
    });
    const chained = current.then(() => next);
    queues.set(key, chained);
    return { key, current, chained, releaseNext };
  });
  registrationQueue = registration.then(
    () => undefined,
    () => undefined,
  );
  const { key, current, chained, releaseNext } = await registration;
  await current;
  try {
    return await fn();
  } finally {
    releaseNext();
    if (queues.get(key) === chained) queues.delete(key);
  }
}
