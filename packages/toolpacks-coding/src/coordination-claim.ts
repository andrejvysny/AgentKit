import { createHash, randomUUID } from "node:crypto";
import { link, open, readdir, readFile, stat, unlink } from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import { code, readRecord, type LockRecord } from "./coordination-record.js";
import {
  captureProcessIdentity,
  inspectProcess,
  type ProcessIdentity,
} from "./process-identity.js";

interface Claim {
  version: 2;
  recordDigest: string;
  sequence: number;
  claimant: ProcessIdentity;
  token: string;
  target: { dev: string; ino: string };
}

interface OwnedClaim {
  token: string;
  busy: boolean;
}
const ownedClaims = new Map<string, OwnedClaim>();
interface ClaimDependencies {
  inspect?: typeof inspectProcess;
  syncDirectory?(path: string): Promise<void>;
  /** Deterministic persistence-boundary fault injection; never exposed by the public coordinator. */
  afterClaim?(): Promise<void>;
}

function validClaim(
  value: unknown,
  digest: string,
  sequence: number,
): value is Claim {
  if (!value || typeof value !== "object") return false;
  const claim = value as Claim;
  return (
    claim.version === 2 &&
    claim.recordDigest === digest &&
    claim.sequence === sequence &&
    typeof claim.token === "string" &&
    claim.token.length > 0 &&
    !!claim.claimant &&
    !!claim.target &&
    /^\d+$/.test(claim.target.dev) &&
    /^\d+$/.test(claim.target.ino) &&
    Number.isSafeInteger(claim.claimant.pid) &&
    claim.claimant.pid > 0 &&
    typeof claim.claimant.startIdentity === "string" &&
    claim.claimant.startIdentity.length > 0
  );
}

async function publish(path: string, claim: Claim): Promise<boolean> {
  const temp = `${path}.tmp-${claim.token}`;
  const file = await open(temp, "wx", 0o600);
  try {
    await file.writeFile(JSON.stringify(claim));
    await file.sync();
  } finally {
    await file.close();
  }
  try {
    await link(temp, path);
    return true;
  } catch (error) {
    if (code(error) === "EEXIST") return false;
    throw error;
  } finally {
    await unlink(temp);
  }
}

async function tail(
  path: string,
  digest: string,
): Promise<{ path: string; claim: Claim } | "corrupt" | undefined> {
  const prefix = `${basename(path)}.claims-v2-${digest}-`;
  const names = (await readdir(dirname(path))).filter(
    (name) => name.startsWith(prefix) && !name.includes(".tmp-"),
  );
  const slots = names
    .map((name) => ({ name, sequence: Number(name.slice(prefix.length)) }))
    .sort((a, b) => a.sequence - b.sequence);
  let last: { path: string; claim: Claim } | undefined;
  for (let index = 0; index < slots.length; index++) {
    const slot = slots[index]!;
    if (
      !Number.isSafeInteger(slot.sequence) ||
      slot.sequence !== index + 1 ||
      slot.name !== `${prefix}${slot.sequence}`
    )
      return "corrupt";
    const claimPath = join(dirname(path), slot.name);
    let value: unknown;
    try {
      value = JSON.parse(await readFile(claimPath, "utf8"));
    } catch (error) {
      if (error instanceof SyntaxError || code(error) === "ENOENT")
        return "corrupt";
      throw error;
    }
    if (!validClaim(value, digest, slot.sequence)) return "corrupt";
    last = { path: claimPath, claim: value };
  }
  return last;
}

async function anchorTarget(
  path: string,
  digest: string,
  expected: string,
): Promise<Claim["target"] | undefined> {
  const anchor = `${path}.anchor-v2-${digest}`;
  try {
    await link(path, anchor);
  } catch (error) {
    if (code(error) === "ENOENT") return undefined;
    if (code(error) !== "EEXIST") throw error;
  }
  const original = await stat(anchor, { bigint: true });
  if (JSON.stringify(await readRecord(anchor)) !== expected) return undefined;
  return { dev: original.dev.toString(), ino: original.ino.toString() };
}

async function registerClaim(
  path: string,
  claim: Claim,
): Promise<OwnedClaim | undefined> {
  if (ownedClaims.has(path)) return undefined;
  const owned = { token: claim.token, busy: true };
  ownedClaims.set(path, owned);
  try {
    if (!(await publish(path, claim))) {
      ownedClaims.delete(path);
      return undefined;
    }
    return owned;
  } catch (error) {
    // A failure after link still elected this token; a same-process retry may finish it.
    try {
      const current: unknown = JSON.parse(await readFile(path, "utf8"));
      if (
        !validClaim(current, claim.recordDigest, claim.sequence) ||
        current.token !== claim.token
      )
        ownedClaims.delete(path);
    } catch {
      ownedClaims.delete(path);
    }
    owned.busy = false;
    throw error;
  }
}

async function electClaim(
  path: string,
  digest: string,
  target: Claim["target"],
  previous: { path: string; claim: Claim } | undefined,
  inspect: typeof inspectProcess,
): Promise<OwnedClaim | undefined> {
  const owned = previous && ownedClaims.get(previous.path);
  if (previous && owned?.token === previous.claim.token) {
    if (owned.busy) return undefined;
    owned.busy = true;
    return owned;
  }
  if (previous) {
    const inspection = await inspect(previous.claim.claimant);
    if (!["gone", "reused", "zombie"].includes(inspection.state))
      return undefined;
  }
  const claimant = await captureProcessIdentity(process.pid);
  if (!claimant) return undefined;
  const sequence = (previous?.claim.sequence ?? 0) + 1;
  return registerClaim(`${path}.claims-v2-${digest}-${sequence}`, {
    version: 2,
    recordDigest: digest,
    sequence,
    claimant,
    token: randomUUID(),
    target,
  });
}

async function syncDirectory(path: string): Promise<void> {
  const directory = await open(path, "r");
  try {
    await directory.sync();
  } finally {
    await directory.close();
  }
}

async function sameRecord(
  path: string,
  expected: string,
  target: Claim["target"],
): Promise<boolean> {
  const before = await stat(path, { bigint: true }).catch((error: unknown) => {
    if (code(error) === "ENOENT") return undefined;
    throw error;
  });
  if (
    !before ||
    before.dev.toString() !== target.dev ||
    before.ino.toString() !== target.ino
  )
    return false;
  const current = await readRecord(path);
  if (
    current === "gone" ||
    current === "corrupt" ||
    JSON.stringify(current) !== expected
  )
    return false;
  const after = await stat(path, { bigint: true });
  return before.ino === after.ino && before.dev === after.dev;
}

/** Immutable succession prevents an abandoned reclaimer or stale reader deleting a newer generation. */
export async function withRecordClaim(
  path: string,
  judged: LockRecord,
  mutate: () => Promise<void>,
  dependencies: ClaimDependencies = {},
): Promise<boolean> {
  const expected = JSON.stringify(judged);
  const digest = createHash("sha256").update(expected).digest("hex");
  const previous = await tail(path, digest);
  if (previous === "corrupt") return false;
  const target = await anchorTarget(path, digest, expected);
  if (
    !target ||
    (previous &&
      (previous.claim.target.dev !== target.dev ||
        previous.claim.target.ino !== target.ino))
  )
    return false;
  const owned = await electClaim(
    path,
    digest,
    target,
    previous,
    dependencies.inspect ?? inspectProcess,
  );
  if (!owned) return false;
  owned.busy = true;
  try {
    await (dependencies.syncDirectory ?? syncDirectory)(dirname(path));
    await dependencies.afterClaim?.();
    // All valid removers share the claim chain; a live predecessor cannot be superseded.
    if (!(await sameRecord(path, expected, target))) return false;
    await mutate();
    return true;
  } finally {
    owned.busy = false;
  }
}
