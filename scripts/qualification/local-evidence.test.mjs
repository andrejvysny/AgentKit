import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import {
  captureSource,
  digest,
  freezeSource,
  overallStatus,
  prepareExecution,
  runGate,
  sameSource,
} from "./local-evidence.mjs";

test("source stability includes dirty inputs and executable content identity", () => {
  const source = {
    sha: "abc",
    dirty: " M file",
    patchSha256: "patch",
    sourceDigest: "files",
  };
  assert.equal(sameSource(source, { ...source }), true);
  for (const key of Object.keys(source))
    assert.equal(sameSource(source, { ...source, [key]: "changed" }), false);
});

test("qualification never promotes missing or unexecuted evidence", () => {
  assert.equal(overallStatus([]), "NOT RUN");
  assert.equal(
    overallStatus([{ status: "PASS" }, { status: "BLOCKED" }]),
    "BLOCKED",
  );
  assert.equal(
    overallStatus([{ status: "PASS" }, { status: "NOT RUN" }]),
    "NOT RUN",
  );
  assert.equal(
    overallStatus([{ status: "FAIL" }, { status: "BLOCKED" }]),
    "FAIL",
  );
});

test("gate records logs, exit failures, absent tools and external timeout distinctly", async () => {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-evidence-test-"));
  try {
    for (const [id, command, args, status, timeoutMs] of [
      [
        "pass",
        process.execPath,
        ["-e", "console.log('proof'); console.error('stderr proof')"],
        "PASS",
        5000,
      ],
      ["fail", process.execPath, ["-e", "process.exit(7)"], "FAIL", 5000],
      ["missing", join(directory, "no-tool"), [], "BLOCKED", 5000],
      [
        "frozen",
        process.execPath,
        ["-e", "while(true){}"],
        "INCONCLUSIVE",
        100,
      ],
    ]) {
      const log = join(directory, `${id}.log`);
      const result = await runGate({ id, command, args, timeoutMs }, log);
      assert.equal(result.status, status);
      assert.equal(result.logSha256, digest(readFileSync(log)));
      if (id === "pass")
        assert.match(readFileSync(log, "utf8"), /stderr proof/);
      if (id === "fail") assert.equal(result.exitCode, 7);
      if (id === "frozen") assert.equal(result.timedOut, true);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("execution reads captured source despite later original edits", async () => {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-snapshot-test-"));
  try {
    const root = fileURLToPath(new URL("../../", import.meta.url));
    const original = join(directory, "original");
    const clone = spawnSync("git", [
      "clone",
      "--no-checkout",
      "--shared",
      "--",
      root,
      original,
    ]);
    assert.equal(clone.status, 0);
    writeFileSync(join(original, "input.txt"), "captured");
    const source = captureSource(original);
    const output = join(directory, "evidence");
    mkdirSync(output);
    freezeSource(original, output, source);
    const parent = spawnSync("git", ["-C", original, "rev-parse", "HEAD^"], {
      encoding: "utf8",
    }).stdout.trim();
    assert.equal(
      spawnSync("git", [
        "-C",
        original,
        "update-ref",
        "--no-deref",
        "HEAD",
        parent,
      ]).status,
      0,
    );
    const work = prepareExecution(original, output, source);
    assert.equal(captureSource(work).sha, source.sha);
    writeFileSync(join(original, "input.txt"), "changed during execution");
    const result = await runGate(
      {
        id: "snapshot",
        command: process.execPath,
        args: [
          "-e",
          "console.log(require('node:fs').readFileSync('input.txt','utf8'))",
        ],
        cwd: work,
      },
      join(output, "execution.log"),
    );
    assert.equal(result.status, "PASS");
    assert.equal(
      readFileSync(join(output, "execution.log"), "utf8").trim(),
      "captured",
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("mandatory suites cannot pass with zero execution or skipped native tests", async () => {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-skips-test-"));
  try {
    for (const [id, text, requireTests, noSkips, expected] of [
      ["all-skipped", "0 pass\n32 skip\n0 fail", true, false, "INCONCLUSIVE"],
      ["native-skipped", "2 pass\n1 skip\n0 fail", true, true, "INCONCLUSIVE"],
      ["executed", "2 pass\n0 skip\n0 fail", true, true, "PASS"],
      ["colored", "\u001b[32m2 pass\u001b[0m\n0 skip", true, true, "PASS"],
      [
        "incomplete-parity",
        '{"pass":2,"skip":0,"complete":false}',
        true,
        false,
        "INCONCLUSIVE",
      ],
    ]) {
      const result = await runGate(
        {
          id,
          command: process.execPath,
          args: ["-e", `console.log(${JSON.stringify(text)})`],
          requireTests,
          noSkips,
        },
        join(directory, `${id}.log`),
      );
      assert.equal(result.status, expected);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
