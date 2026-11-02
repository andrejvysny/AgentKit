/**
 * Structure pins for `.github/workflows/native-release.yml` — the lane that
 * builds the five prebuilt platform packages and (gated) publishes the
 * native npm stack.
 *
 * The matrix must mirror `rust/scripts/native-platform-targets.mjs` exactly
 * — that table is already pinned against the package manifests by
 * `packages/native/tests/distribution.test.ts`; this pins the CI half, so a
 * triple can never quietly disappear from the release while its package
 * stays published (consumers on that platform would then get the loader's
 * "native binding failed to load" with no artifact ever built).
 *
 * The publish trust model is pinned too: dispatch-only, environment-gated,
 * dry-run by default, `NPM_TOKEN` only on the execute step, no rebuild in
 * the publish job (it consumes the verified digests — same discipline as
 * publish.yml), and no `${{ }}` input interpolation inside `run:` scripts
 * (the release-workflow.test.ts rule: textual substitution runs before the
 * shell sees the line).
 */
import { describe, expect, it } from "bun:test";
import { NATIVE_PLATFORM_TARGETS } from "../../rust/scripts/native-platform-targets.mjs";

const yaml = await Bun.file(
  new URL("../../.github/workflows/native-release.yml", import.meta.url),
).text();

function section(name: string): string {
  const start = yaml.indexOf(`\n  ${name}:`);
  expect(start, `job ${name} exists`).toBeGreaterThan(-1);
  const next = yaml.indexOf("\n  [a-z]", start + 1);
  return yaml.slice(start, next === -1 ? yaml.length : next);
}

describe("native-release workflow structure", () => {
  it("is manual-dispatch only", () => {
    const on = yaml.slice(
      yaml.indexOf("\non:"),
      yaml.indexOf("\npermissions:"),
    );
    expect(on).toContain("workflow_dispatch:");
    expect(on).not.toMatch(/\n {2}push:/);
    expect(on).not.toMatch(/\n {2}pull_request:/);
    expect(on).not.toMatch(/\n {2}schedule:/);
  });

  it("builds exactly the platform table — no more, no less", () => {
    const build = section("build");
    const triples = [...build.matchAll(/triple: ([\w-]+)/g)].map((m) => m[1]);
    const keys = [...build.matchAll(/key: ([\w-]+)/g)].map((m) => m[1]);
    expect(triples.sort()).toEqual(
      NATIVE_PLATFORM_TARGETS.map((t) => t.triple).sort(),
    );
    expect(keys.sort()).toEqual(
      NATIVE_PLATFORM_TARGETS.map((t) => t.key).sort(),
    );
    expect(triples.length).toBe(NATIVE_PLATFORM_TARGETS.length);
    for (const target of NATIVE_PLATFORM_TARGETS) {
      const entry = new RegExp(
        `- triple: ${target.triple}\\n\\s+key: ${target.key}\\n\\s+os: ([\\w.-]+)`,
      ).exec(build);
      expect(entry, `matrix entry for ${target.triple}`).not.toBeNull();
      expect(entry?.[1]).toBe(target.runner);
    }
  });

  it("builds per-triple addons and verifies every artifact", () => {
    const build = section("build");
    expect(build).toContain("build-native-release-artifact.mjs");
    expect(build).toContain("--target ${{ matrix.triple }}");
    expect(build).toContain("check-native-artifact.mjs");
    expect(build).toContain("artifact-manifest.${{ matrix.key }}.json");
    // The snapshot is digest-bound before use: a corrupted artifact never
    // reaches a compiler.
    expect(build).toContain("sha256sum -c -");
  });

  it("gates publication behind the environment, provenance, and dry-run default", () => {
    const publish = section("publish");
    expect(publish).toContain("environment: agentkit-native-publication");
    expect(publish).toContain("id-token: write");
    expect(yaml).toMatch(/dry_run:[\s\S]*?default: "true"/);
    expect(publish).toContain("if: inputs.dry_run == 'false'");
    expect(publish).toContain("--execute");
    expect(publish).toContain("npm publish (platforms → wrapper → umbrella)");
    // The token only exists on the execute step.
    const executeStep = publish.slice(
      publish.indexOf("if: inputs.dry_run == 'false'"),
      publish.indexOf("if: inputs.dry_run != 'false'"),
    );
    expect(executeStep).toContain("NPM_TOKEN");
    const dryStep = publish.slice(
      publish.indexOf("if: inputs.dry_run != 'false'"),
    );
    expect(dryStep).not.toContain("NPM_TOKEN");
    // No rebuild in the publish job: it consumes verified digests only.
    expect(publish).not.toContain("npm pack");
    expect(publish).not.toContain("bun run build");
    expect(publish).toContain("native-publish.mjs verify");
  });

  it("freezes one snapshot that both the builds and the umbrella come from", () => {
    const snapshot = section("snapshot");
    expect(snapshot).toContain("scripts/snapshot-release.mjs");
    expect(snapshot).toContain("bun-version: 1.4.0");
    expect(snapshot).toContain("npm pack ./packages/agentkit");
    expect(snapshot).toContain("actions/upload-artifact@v4");
    // Only the default branch may freeze a release.
    expect(snapshot).toContain("github.event.repository.default_branch");
    expect(snapshot).toContain("merge-base --is-ancestor");
  });

  it("binds executed code to the reviewed default-branch commit, not the dispatch ref", () => {
    // workflow_dispatch sets GITHUB_SHA to the DISPATCH ref — an ancestry
    // check against it is vacuous while the jobs check out
    // inputs.source_commit. Every job that executes checkout content
    // (snapshot: bun install/cargo/npm; publish: NPM_TOKEN + id-token)
    // must bind HEAD to the input and ancestry-check HEAD itself.
    expect(yaml).not.toContain('is-ancestor "$GITHUB_SHA"');
    for (const job of ["snapshot", "publish"]) {
      const text = section(job);
      expect(text, `${job} binds HEAD to source_commit`).toContain(
        'test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"',
      );
      expect(text, `${job} validates the SHA shape`).toContain(
        "^[0-9a-f]{40}$",
      );
      expect(text, `${job} ancestry-checks HEAD`).toContain(
        "merge-base --is-ancestor HEAD",
      );
    }
  });

  it("runs bash on every build row and passes --allow-cross only where the table says cross", () => {
    const build = section("build");
    // The win32 row's default shell is PowerShell; the steps below are
    // bash ($RUNNER_TEMP, mkdir -p, sha256sum). Without the pin the row
    // dies and the complete-set gate blocks EVERY release.
    expect(build).toMatch(/defaults:\s*\n\s+run:\s*\n\s+shell: bash/);
    expect(build).toContain('mkdir -p "$RUNNER_TEMP/out"');
    expect(build).toContain(
      "CROSS_FLAG: ${{ matrix.cross == 'true' && '--allow-cross' || '' }}",
    );
    expect(build).toContain('--out "$RUNNER_TEMP/out" $CROSS_FLAG');
    for (const target of NATIVE_PLATFORM_TARGETS) {
      const entry = new RegExp(
        `- triple: ${target.triple}[\\s\\S]*?cross: "([a-z]+)"`,
      ).exec(build);
      expect(entry, `cross flag for ${target.triple}`).not.toBeNull();
      expect(entry?.[1]).toBe(String(target.cross));
    }
  });

  it("keeps input interpolation out of run scripts", () => {
    assertNoRunInterpolation(yaml);
  });
});

function assertNoRunInterpolation(source: string): void {
  const lines = source.split("\n");
  let blockIndent: number | null = null;
  for (const text of lines) {
    const indent = text.length - text.trimStart().length;
    if (blockIndent !== null) {
      if (text.trim() === "") continue;
      if (indent > blockIndent) {
        expect(text).not.toMatch(/\$\{\{.*inputs\./);
        continue;
      }
      blockIndent = null;
    }
    const match = /^(\s*)-?\s*run:\s*(.*)$/.exec(text);
    if (!match) continue;
    const rest = (match[2] ?? "").trim();
    if (["|", ">", "|-", ">-"].includes(rest)) blockIndent = indent;
    else expect(rest).not.toMatch(/\$\{\{.*inputs\./);
  }
}

/**
 * The rehearsal lane (`.github/workflows/native-release-test.yml`) must
 * stay a faithful copy of the real lane's execution shape — same matrix,
 * same scripts, same toolchain pins — or it rehearses nothing. Unlike the
 * release lane it must NEVER hold credentials: it runs on untrusted PR
 * code, so secrets, environments, and any registry reachability are
 * structural failures, not style choices.
 */
const rehearsalYaml = await Bun.file(
  new URL("../../.github/workflows/native-release-test.yml", import.meta.url),
).text();

function rehearsalSection(name: string): string {
  const start = rehearsalYaml.indexOf(`\n  ${name}:`);
  expect(start, `job ${name} exists`).toBeGreaterThan(-1);
  const next = rehearsalYaml.indexOf("\n  [a-z]", start + 1);
  return rehearsalYaml.slice(start, next === -1 ? rehearsalYaml.length : next);
}

describe("native release lane rehearsal workflow", () => {
  it("carries no credentials, no environment gate, and no registry path", () => {
    expect(rehearsalYaml).not.toContain("secrets.");
    expect(rehearsalYaml).not.toContain("environment:");
    expect(rehearsalYaml).not.toContain("npm publish");
    expect(rehearsalYaml).not.toContain("NPM_TOKEN");
    expect(rehearsalYaml).not.toContain("id-token");
    expect(rehearsalYaml).toContain("workflow_dispatch:");
    expect(rehearsalYaml).toContain("pull_request:");
  });

  it("rehearses exactly the platform table, with the same cross flags", () => {
    const build = rehearsalSection("rehearsal-build");
    const triples = [...build.matchAll(/triple: ([\w-]+)/g)].map((m) => m[1]);
    expect(triples.sort()).toEqual(
      NATIVE_PLATFORM_TARGETS.map((t) => t.triple).sort(),
    );
    for (const target of NATIVE_PLATFORM_TARGETS) {
      const entry = new RegExp(
        `- triple: ${target.triple}\\n\\s+key: ${target.key}\\n\\s+os: ([\\w.-]+)[\\s\\S]*?cross: "([a-z]+)"`,
      ).exec(build);
      expect(entry, `matrix entry for ${target.triple}`).not.toBeNull();
      expect(entry?.[1]).toBe(target.runner);
      expect(entry?.[2]).toBe(String(target.cross));
    }
    expect(build).toMatch(/defaults:\s*\n\s+run:\s*\n\s+shell: bash/);
  });

  it("executes the real release entry points with identical pins", () => {
    for (const script of [
      "scripts/snapshot-release.mjs",
      "build-native-release-artifact.mjs",
      "check-native-artifact.mjs",
      "native-publish.mjs verify",
      "npm pack ./packages/agentkit",
    ])
      expect(rehearsalYaml, `rehearsal must run ${script}`).toContain(script);
    // Same frozen toolchains as the release lane — a rehearsal on a
    // different compiler proves nothing about the shipped bytes.
    for (const pin of [
      "toolchain: 1.97.0",
      "bun-version: 1.4.0",
      'node-version: "22.23.2"',
      "bun install --frozen-lockfile --ignore-scripts",
      "CROSS_FLAG: ${{ matrix.cross == 'true' && '--allow-cross' || '' }}",
      '--out "$RUNNER_TEMP/out" $CROSS_FLAG',
      "sha256sum -c -",
    ])
      expect(rehearsalYaml, `missing pin ${pin}`).toContain(pin);
  });

  it("gates the whole matrix through the publish verifier before going green", () => {
    expect(rehearsalYaml).toContain("needs: rehearsal-snapshot");
    expect(rehearsalYaml).toContain(
      "needs: [rehearsal-snapshot, rehearsal-build]",
    );
    const gate = rehearsalSection("rehearsal-gate");
    expect(gate).toContain("native-publish.mjs verify");
    // The commit binding is the checkout itself — the same discipline as
    // the release lane, where it binds to inputs.source_commit instead.
    expect(gate).toContain("SOURCE_COMMIT: ${{ github.sha }}");
    expect(gate).toContain("9.9.9");
    expect(rehearsalYaml).toContain('--commit "$SOURCE_COMMIT"');
  });

  it("keeps input interpolation out of run scripts", () => {
    assertNoRunInterpolation(rehearsalYaml);
  });
});
