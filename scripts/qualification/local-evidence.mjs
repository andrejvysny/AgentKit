import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  copyFileSync,
  createWriteStream,
  lstatSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { stripVTControlCharacters } from "node:util";

export const digest = (bytes) =>
  createHash("sha256").update(bytes).digest("hex");

export function git(root, args) {
  const result = spawnSync("git", args, {
    cwd: root,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.status !== 0) throw new Error(result.stderr || "Git read failed");
  return result.stdout;
}

export function captureSource(root) {
  const paths = [
    ...new Set(
      git(root, [
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
      ])
        .split("\0")
        .filter(Boolean),
    ),
  ].sort();
  const files = paths.flatMap((path) => {
    const file = join(root, path);
    let stat;
    try {
      stat = lstatSync(file);
    } catch (error) {
      if (error.code === "ENOENT") return [];
      throw error;
    }
    if (!stat.isFile()) throw new Error(`Nonregular source file: ${path}`);
    return [
      {
        path,
        sha256: digest(readFileSync(file)),
        executable: !!(stat.mode & 0o111),
      },
    ];
  });
  return {
    sha: git(root, ["rev-parse", "HEAD"]).trim(),
    dirty: git(root, ["status", "--porcelain=v1", "--untracked-files=all"]),
    patchSha256: digest(git(root, ["diff", "--binary", "HEAD"])),
    sourceDigest: digest(JSON.stringify(files)),
    files,
  };
}

export function sameSource(before, after) {
  return ["sha", "dirty", "patchSha256", "sourceDigest"].every(
    (key) => before[key] === after[key],
  );
}

export function freezeSource(root, output, source) {
  const directory = join(output, "source");
  mkdirSync(directory);
  for (const file of source.files) {
    const target = join(directory, file.path);
    mkdirSync(dirname(target), { recursive: true });
    copyFileSync(join(root, file.path), target);
    if (digest(readFileSync(target)) !== file.sha256)
      throw new Error(`Source changed during copy: ${file.path}`);
    chmodSync(target, file.executable ? 0o555 : 0o444);
  }
  if (!sameSource(source, captureSource(root)))
    throw new Error("Source changed during capture");
  writeFileSync(
    join(output, "source-manifest.json"),
    `${JSON.stringify(source, null, 2)}\n`,
    { flag: "wx" },
  );
}

export function prepareExecution(root, output, source) {
  const work = join(output, "work");
  for (const args of [
    ["clone", "--no-checkout", "--shared", "--", root, work],
    ["-C", work, "update-ref", "--no-deref", "HEAD", source.sha],
    ["-C", work, "read-tree", source.sha],
  ]) {
    const result = spawnSync("git", args, {
      encoding: "utf8",
      timeout: 30_000,
    });
    if (result.error || result.status !== 0)
      throw result.error ?? new Error(result.stderr);
  }
  for (const file of source.files) {
    const target = join(work, file.path);
    mkdirSync(dirname(target), { recursive: true });
    copyFileSync(join(output, "source", file.path), target);
    chmodSync(target, file.executable ? 0o755 : 0o644);
  }
  const captured = captureSource(work);
  if (
    captured.sha !== source.sha ||
    captured.sourceDigest !== source.sourceDigest
  )
    throw new Error("Execution copy differs from captured source");
  return work;
}

export function resultStatus({ code, signal, error, timedOut }) {
  if (error?.code === "ENOENT") return "BLOCKED";
  if (timedOut || signal) return "INCONCLUSIVE";
  return !error && code === 0 ? "PASS" : "FAIL";
}

function killGroup(child) {
  if (child.pid && process.platform !== "win32") {
    try {
      process.kill(-child.pid, "SIGKILL");
    } catch {
      child.kill("SIGKILL");
    }
  } else {
    if (process.platform === "win32" && child.pid)
      spawnSync("taskkill", ["/pid", String(child.pid), "/T", "/F"], {
        timeout: 5000,
      });
    child.kill("SIGKILL");
  }
}

function testEvidence(gate, output) {
  const summaries = output.split("\n").flatMap((line) => {
    try {
      const row = JSON.parse(line);
      return typeof row?.complete === "boolean" ? [row] : [];
    } catch {
      return [];
    }
  });
  const passed = [
    ...output.matchAll(
      /(?:^|\n)\s*(\d+) pass\b|test result: ok\. (\d+) passed|^# pass (\d+)|"pass":\s*(\d+)/gm,
    ),
  ].reduce(
    (total, match) =>
      total + Number(match[1] ?? match[2] ?? match[3] ?? match[4]),
    0,
  );
  const skips = [...output.matchAll(/(?:^|\n)\s*(\d+) skip\b/g)].map((match) =>
    Number(match[1]),
  );
  skips.push(...summaries.map((row) => row.skip ?? 0));
  const incomplete =
    (gate.requireTests && passed === 0) ||
    (gate.requireTests &&
      summaries.some((row) => !row.complete || row.pass === 0)) ||
    (gate.noSkips && skips.some((count) => count > 0));
  return {
    passed,
    skips,
    incomplete: !!incomplete,
    testSummaries: summaries,
    ignored: [...output.matchAll(/; (\d+) ignored/g)].map((match) =>
      Number(match[1]),
    ),
  };
}

function gateResult(gate, log, state, code, signal) {
  const evidence = testEvidence(
    gate,
    stripVTControlCharacters(readFileSync(log, "utf8")),
  );
  const status = resultStatus({
    code,
    signal,
    error: state.error,
    timedOut: state.timedOut,
  });
  return {
    id: gate.id,
    command: [gate.command, ...gate.args],
    cwd: gate.cwd,
    environment: gate.env ?? {},
    unsetEnvironment: Object.entries(gate.env ?? {})
      .filter(([, value]) => value === undefined)
      .map(([name]) => name),
    startedAt: state.startedAt,
    finishedAt: new Date().toISOString(),
    status: status === "PASS" && evidence.incomplete ? "INCONCLUSIVE" : status,
    ...evidence,
    exitCode: code,
    signal,
    error: state.error?.message ?? null,
    timedOut: state.timedOut,
    log,
    logSha256: digest(readFileSync(log)),
  };
}

function recordOutput(child, stream, state) {
  for (const pipe of [child.stdout, child.stderr]) {
    pipe.on("data", (chunk) => {
      if (!state.finished) stream.write(chunk);
    });
  }
  child.on("error", (error) => {
    state.error = error;
    if (!state.finished) stream.write(`${error.message}\n`);
  });
}

export function runGate(gate, log, environment = process.env) {
  return new Promise((resolve) => {
    const state = {
      startedAt: new Date().toISOString(),
      timedOut: false,
      error: undefined,
      finished: false,
    };
    const stream = createWriteStream(log, { flags: "wx" });
    const child = spawn(gate.command, gate.args, {
      cwd: gate.cwd,
      env: { ...environment, ...gate.env },
      detached: process.platform !== "win32",
      stdio: ["ignore", "pipe", "pipe"],
    });
    recordOutput(child, stream, state);
    let completion;
    const finish = (code, signal) => {
      if (state.finished) return;
      state.finished = true;
      clearTimeout(timeout);
      clearTimeout(completion);
      stream.end(() => resolve(gateResult(gate, log, state, code, signal)));
    };
    const timeout = setTimeout(() => {
      state.timedOut = true;
      killGroup(child);
      completion = setTimeout(() => {
        child.stdout.destroy();
        child.stderr.destroy();
        finish(null, "SIGKILL");
      }, 2000);
    }, gate.timeoutMs ?? 3_600_000);
    child.on("close", finish);
  });
}

export function overallStatus(rows) {
  for (const status of ["FAIL", "INCONCLUSIVE", "BLOCKED", "NOT RUN"])
    if (rows.some((row) => row.status === status)) return status;
  return rows.length ? "PASS" : "NOT RUN";
}
