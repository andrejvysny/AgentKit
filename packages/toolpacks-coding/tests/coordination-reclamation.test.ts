import { afterEach, expect, test } from "bun:test";
import { spawn, type ChildProcess } from "node:child_process";
import { createHash } from "node:crypto";
import { link, mkdtemp, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { withRecordClaim } from "../src/coordination-claim.js";
import {
  liveOwner,
  readRecord,
  remove,
  type LockRecord,
} from "../src/coordination-record.js";
import { persistRecord } from "../src/coordination-storage.js";
import { captureProcessIdentity } from "../src/process-identity.js";

const directories: string[] = [];
const children: ChildProcess[] = [];
afterEach(async () => {
  for (const child of children.splice(0)) child.kill("SIGKILL");
  await Promise.all(
    directories
      .splice(0)
      .map((dir) => rm(dir, { recursive: true, force: true })),
  );
});

async function fixture(): Promise<{ path: string; record: LockRecord }> {
  const dir = await mkdtemp(join(tmpdir(), "coding-reclaim-"));
  directories.push(dir);
  const path = join(dir, "exclusive.lock");
  const record: LockRecord = {
    lockId: "dead-generation",
    pid: 2_147_483_647,
    tool: "test",
    label: "test",
    createdAt: 1,
  };
  await writeFile(path, JSON.stringify(record));
  return { path, record };
}

async function pausedClaim(
  path: string,
  boundary: string,
): Promise<ChildProcess> {
  const child = spawn(
    process.execPath,
    [join(import.meta.dir, "fixtures/reclaim-lock.ts"), path, boundary],
    { stdio: ["pipe", "pipe", "inherit"] },
  );
  children.push(child);
  await new Promise<void>((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code) =>
      reject(new Error(`Reclaimer exited before barrier: ${code}`)),
    );
    child.stdout!.on("data", (data) => {
      if (String(data).includes("ready")) resolve();
    });
  });
  return child;
}

async function crash(child: ChildProcess): Promise<void> {
  const exited = new Promise<void>((resolve) => {
    child.once("exit", () => resolve());
  });
  child.kill("SIGKILL");
  await exited;
}

test("legacy hard-link receipt cannot permanently strand a dead plain lease", async () => {
  const { path, record } = await fixture();
  const digest = createHash("sha256")
    .update(JSON.stringify(record))
    .digest("hex");
  await link(path, `${path}.reclaimed-${digest}`);
  expect(await liveOwner(path, () => {})).toBeUndefined();
  expect(await liveOwner(path, () => {})).toBeUndefined();
});

for (const boundary of ["after-claim", "before-removal"]) {
  test(`dead reclaimer after ${boundary} yields immutable successor claim`, async () => {
    const { path } = await fixture();
    await crash(await pausedClaim(path, boundary));
    expect(await liveOwner(path, () => {})).toBeUndefined();
    const claims = (await readdir(directories[directories.length - 1]!)).filter(
      (name) => name.includes(".claims-v2-"),
    );
    expect(claims).toHaveLength(2);
  });
}

test("crash after removal cannot reclaim a replacement generation", async () => {
  const { path, record } = await fixture();
  const child = await pausedClaim(path, "after-removal");
  const next = { ...record, lockId: "replacement", pid: process.pid };
  await writeFile(path, JSON.stringify(next), { flag: "wx" });
  await crash(child);
  expect(await liveOwner(path, () => {})).toEqual(next);
  expect(await readRecord(path)).toEqual(next);
});

test("replacement while a stale claimant is publishing remains intact", async () => {
  const { path, record } = await fixture();
  const published = Promise.withResolvers<void>();
  const resume = Promise.withResolvers<void>();
  let mutations = 0;
  const pending = withRecordClaim(
    path,
    record,
    async () => {
      mutations++;
      await remove(path);
    },
    {
      afterClaim: async () => {
        published.resolve();
        await resume.promise;
      },
    },
  );
  await published.promise;
  const next = { ...record, lockId: "replacement", pid: process.pid };
  await persistRecord(path, next);
  resume.resolve();
  expect(await pending).toBe(false);
  expect(mutations).toBe(0);
  expect(await readRecord(path)).toEqual(next);
});

test("live claimant excludes concurrent reclaimer; completed own claim supports retry", async () => {
  const { path, record } = await fixture();
  const published = Promise.withResolvers<void>();
  const resume = Promise.withResolvers<void>();
  const pending = withRecordClaim(path, record, async () => {}, {
    afterClaim: async () => {
      published.resolve();
      await resume.promise;
    },
  });
  await published.promise;
  expect(await withRecordClaim(path, record, () => remove(path))).toBe(false);
  resume.resolve();
  expect(await pending).toBe(true);
  expect(await withRecordClaim(path, record, () => remove(path))).toBe(true);
});

test("corrupt or uninspectable claimant remains blocking", async () => {
  const { path, record } = await fixture();
  const digest = createHash("sha256")
    .update(JSON.stringify(record))
    .digest("hex");
  const claimPath = `${path}.claims-v2-${digest}-1`;
  await writeFile(claimPath, "{broken");
  expect(await liveOwner(path, () => {})).toEqual(record);
  await writeFile(
    claimPath,
    JSON.stringify({
      version: 2,
      recordDigest: digest,
      sequence: 1,
      claimant: await captureProcessIdentity(process.pid),
      token: "external",
      target: {
        dev: String((await stat(path)).dev),
        ino: String((await stat(path)).ino),
      },
    }),
  );
  expect(
    await withRecordClaim(path, record, () => remove(path), {
      inspect: async () => ({ state: "unknown" }),
    }),
  ).toBe(false);
  expect(await readRecord(path)).toEqual(record);
});

test("directory sync failure after claim publication permits exact own-token retry", async () => {
  const { path, record } = await fixture();
  await expect(
    withRecordClaim(path, record, () => remove(path), {
      syncDirectory: async () => {
        throw new Error("directory sync failed");
      },
    }),
  ).rejects.toThrow("directory sync failed");
  expect(await readRecord(path)).toEqual(record);
  expect(await withRecordClaim(path, record, () => remove(path))).toBe(true);
});

test("an old claim cannot remove identical record content restored on a different inode", async () => {
  const { path, record } = await fixture();
  expect(await withRecordClaim(path, record, () => remove(path))).toBe(true);
  await writeFile(path, JSON.stringify(record));
  expect(await withRecordClaim(path, record, () => remove(path))).toBe(false);
  expect(await readRecord(path)).toEqual(record);
});
