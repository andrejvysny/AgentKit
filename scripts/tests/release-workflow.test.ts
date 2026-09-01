/**
 * One invariant over `.github/workflows/release.yml`: no `${{ }}` expression is
 * interpolated into a `run:` script.
 *
 * `${{ }}` is textual substitution performed BEFORE the shell parses the
 * script, so a value carrying shell metacharacters is EXECUTED rather than
 * quoted. This workflow's values are ref names — `steps.meta.outputs.tag` comes
 * from `GITHUB_REF`, which is attacker-influenceable on a public repo. Passing
 * them through `env:` and writing `"$TAG"` makes them inert whatever they hold.
 *
 * Asserted here rather than left to review because the wrong form is the one
 * that looks natural, and nothing else in the repo would ever notice.
 *
 * Not covered by `bun run typecheck`: `scripts/` has no tsconfig, as its `.mjs`
 * files never did. Runs under `bun test`.
 */
import { describe, expect, it } from "bun:test";

const WORKFLOW = new URL(
  "../../.github/workflows/release.yml",
  import.meta.url,
);

/**
 * Every line inside a `run:` block, with its 1-based line number.
 *
 * Deliberately crude — a real YAML parse would be more code than the thing it
 * checks. `run:` blocks are the only place a shell sees this file's text, and
 * the two forms are `run: <inline>` and `run: |` + an indented block.
 */
function runScriptLines(yaml: string): Array<{ line: number; text: string }> {
  const out: Array<{ line: number; text: string }> = [];
  const lines = yaml.split("\n");
  let blockIndent: number | null = null;
  for (let i = 0; i < lines.length; i += 1) {
    const text = lines[i] ?? "";
    const indent = text.length - text.trimStart().length;
    if (blockIndent !== null) {
      if (text.trim() === "") continue;
      if (indent > blockIndent) {
        out.push({ line: i + 1, text });
        continue;
      }
      blockIndent = null;
    }
    const match = /^(\s*)-?\s*run:\s*(.*)$/.exec(text);
    if (!match) continue;
    const rest = (match[2] ?? "").trim();
    if (rest === "|" || rest === ">" || rest === "|-" || rest === ">-") {
      blockIndent = indent;
      continue;
    }
    if (rest !== "") out.push({ line: i + 1, text: rest });
  }
  return out;
}

describe("release.yml — no expression interpolation into a shell", () => {
  it("passes every workflow value through env:, never into a run: block", async () => {
    const yaml = await Bun.file(WORKFLOW).text();
    const offenders = runScriptLines(yaml).filter((entry) =>
      entry.text.includes("${{"),
    );
    expect(offenders.map((o) => `line ${o.line}: ${o.text.trim()}`)).toEqual(
      [],
    );
  });

  it("still reads the values it needs — through env:", async () => {
    const yaml = await Bun.file(WORKFLOW).text();
    // The guard above is satisfiable by deleting the steps; this says the tag
    // and branch are still plumbed in, as env vars.
    expect(yaml).toContain("TAG: ${{ steps.meta.outputs.tag }}");
    expect(yaml).toContain(
      "RELEASE_BRANCH: ${{ steps.meta.outputs.release_branch }}",
    );
    expect(yaml).toContain('git push -f origin "$TAG"');
    expect(yaml).toContain('git push -f origin "$RELEASE_BRANCH"');
  });

  it("recognises both run: forms", () => {
    // Guarding the guard: a parser that missed the inline form would report a
    // clean workflow no matter what it contained.
    const sample = [
      "jobs:",
      "  x:",
      "    steps:",
      "      - run: echo ${{ inline }}",
      "      - name: block",
      "        run: |",
      "          echo ${{ blocked }}",
      "        env:",
      "          A: ${{ fine }}",
    ].join("\n");
    const found = runScriptLines(sample).filter((e) => e.text.includes("${{"));
    expect(found.map((f) => f.line)).toEqual([4, 7]);
  });
});
