import { afterEach, describe, expect, it } from "bun:test";
import { appendFileSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  verifyCandidate,
  verifyJobs,
  verifyRun,
} from "../release/publication.mjs";
import { publicationFixture, writeJson } from "./publication-fixture";

const directories: string[] = [];
function fixture(): ReturnType<typeof publicationFixture> {
  const base = mkdtempSync(join(tmpdir(), "agentkit-publish-test-"));
  directories.push(base);
  return publicationFixture(base);
}
afterEach(() => {
  for (const directory of directories.splice(0))
    rmSync(directory, { recursive: true, force: true });
});

describe("publication byte and evidence guards", () => {
  it("accepts complete foundation evidence without any Responses artifact", () => {
    const { directory, expected } = fixture();
    expect(verifyCandidate(directory, expected).tag).toBe("v0.6.0");
  });
  it("rejects changed exact package bytes", () => {
    const { directory, expected } = fixture();
    appendFileSync(join(directory, "agentkit-0.6.0.tgz"), "changed");
    expect(() => verifyCandidate(directory, expected)).toThrow(
      "package bytes or qualification differ",
    );
  });
  it("rejects incomplete native, browser, and minimum source evidence", () => {
    for (const name of [
      "npm-node-native-process-reopen",
      "npm-node-import-bundle-native-host",
      "electron-native-after-rebuild",
    ]) {
      const { directory, expected } = fixture();
      const qualification = JSON.parse(
        readFileSync(
          join(directory, "qualification/qualification.json"),
          "utf8",
        ),
      );
      qualification.checks = qualification.checks.filter(
        (check: { name: string }) => check.name !== name,
      );
      writeJson(
        join(directory, "qualification"),
        "qualification.json",
        qualification,
      );
      expect(() => verifyCandidate(directory, expected)).toThrow(
        `missing or duplicate ${name}`,
      );
    }
    const { directory, expected } = fixture();
    const gates = JSON.parse(
      readFileSync(join(directory, "source-gates.json"), "utf8"),
    );
    gates.checks.find(
      (check: { name: string }) => check.name === "tests",
    ).passed = false;
    writeJson(directory, "source-gates.json", gates);
    expect(() => verifyCandidate(directory, expected)).toThrow(
      "failed or missing checks",
    );
  });
  it("rejects a changed frozen source archive", () => {
    const { directory, expected } = fixture();
    appendFileSync(join(directory, "source.tar.gz"), "changed");
    expect(() => verifyCandidate(directory, expected)).toThrow(
      "source.tar.gz digest differs",
    );
  });
  it("rejects wrong source/version/track and dirty Git proof", () => {
    const { directory, expected } = fixture();
    expect(() =>
      verifyCandidate(directory, { ...expected, commit: "c".repeat(40) }),
    ).toThrow("wrong source provenance");
    expect(() =>
      verifyCandidate(directory, {
        ...expected,
        version: "0.7.0",
        track: "responses",
      }),
    ).toThrow("wrong candidate identity");
    appendFileSync(join(directory, "dirty.patch"), "diff");
    expect(() => verifyCandidate(directory, expected)).toThrow(
      "publication requires clean Git source",
    );
  });
});

describe("trusted run guards", () => {
  it("checks run, artifact ID, repository, default branch, workflow, and track", () => {
    const { expected } = fixture();
    const run = {
      id: 10,
      status: "completed",
      conclusion: "success",
      event: "workflow_dispatch",
      path: ".github/workflows/release.yml",
      head_sha: expected.commit,
      head_branch: "master",
      repository: { full_name: expected.repository },
      head_repository: { full_name: expected.repository },
    };
    const artifact = {
      id: 20,
      name: `agentkit-candidates-foundation-${expected.commit}-macos-latest`,
      expired: false,
      workflow_run: { id: 10, head_sha: expected.commit },
    };
    expect(verifyRun(run, artifact, expected)).toBe(artifact.name);
    expect(() =>
      verifyRun({ ...run, conclusion: "failure" }, artifact, expected),
    ).toThrow("qualification did not succeed");
    expect(() =>
      verifyRun({ ...run, head_branch: "untrusted" }, artifact, expected),
    ).toThrow("trusted default branch");
    expect(() => verifyRun(run, { ...artifact, id: 21 }, expected)).toThrow(
      "wrong or expired macOS artifact",
    );
    expect(() =>
      verifyRun(run, { ...artifact, expired: true }, expected),
    ).toThrow("wrong or expired macOS artifact");
    expect(() =>
      verifyRun(run, artifact, { ...expected, track: "responses" }),
    ).toThrow("wrong or expired macOS artifact");
  });
  it("rejects skipped, failed, or absent minimum Bun and platform jobs", () => {
    const jobs = [
      { name: "bun-minimum", conclusion: "success" },
      { name: "acceptance", conclusion: "success" },
      { name: "qualification (macos-latest, 41.6.1)", conclusion: "success" },
      { name: "qualification (ubuntu-latest, )", conclusion: "success" },
    ];
    expect(() => verifyJobs(jobs)).not.toThrow();
    expect(() => verifyJobs(jobs.slice(1))).toThrow(
      "bun-minimum did not succeed",
    );
    expect(() =>
      verifyJobs(
        jobs.map((job) =>
          job.name === "bun-minimum" ? { ...job, conclusion: "skipped" } : job,
        ),
      ),
    ).toThrow("bun-minimum did not succeed");
    expect(() => verifyJobs(jobs.slice(0, 3))).toThrow(
      "platform qualification is missing",
    );
  });
});
