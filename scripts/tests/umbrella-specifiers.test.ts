/**
 * The guard `build-umbrella.mjs` runs between rewriting `@agentkit/*` module
 * specifiers and collapsing whatever text is left.
 *
 * The failure it exists for is invisible by construction: an unrecognised
 * specifier gets collapsed to a bare `agentkit/...` import of a module that
 * does not exist, and the build's final "no `@agentkit/` remains anywhere"
 * check then PASSES — because the collapse is what removed the evidence. So
 * the only place this can be caught is right here, before the collapse.
 *
 * These tests are not covered by `bun run typecheck`: `scripts/` has no
 * tsconfig, exactly as its `.mjs` files never did. They run under `bun test`.
 */
import { describe, expect, it } from "bun:test";
import { findResidualSpecifiers } from "../umbrella-specifiers.mjs";

const lines = (found: Array<{ line: number }>) => found.map((f) => f.line);

describe("findResidualSpecifiers", () => {
  it("flags a quoted specifier the rewrite could not handle", () => {
    // A SUBPATH import: the rewrite's regex stops at the package name, so this
    // survives it — and used to be collapsed into `import "agentkit/host/x.js"`,
    // which resolves to nothing.
    const found = findResidualSpecifiers(
      ['import { a } from "@agentkit/host/internal.js";'].join("\n"),
    );
    expect(found.length).toBe(1);
    expect(found[0]?.line).toBe(1);
    expect(found[0]?.text).toContain("@agentkit/host/internal.js");
  });

  it("flags a single-quoted specifier and an unknown package alike", () => {
    const found = findResidualSpecifiers(
      ["const x = 1;", "export * from '@agentkit/not-a-subpath';"].join("\n"),
    );
    expect(lines(found)).toEqual([2]);
  });

  it("flags a specifier built in a template literal", () => {
    const found = findResidualSpecifiers(
      ["", "const m = await import(`@agentkit/${name}`);"].join("\n"),
    );
    expect(lines(found)).toEqual([2]);
  });

  it("reports every offending line, with 1-based numbers", () => {
    const found = findResidualSpecifiers(
      [
        "// fine",
        'import a from "@agentkit/host/a.js";',
        "// fine",
        "// fine",
        "const b = `@agentkit/${pkg}`;",
      ].join("\n"),
    );
    expect(lines(found)).toEqual([2, 5]);
  });

  it("leaves prose mentions alone — including backticked ones", () => {
    // This codebase writes `@agentkit/host` inside doc comments constantly.
    // Those are precisely what the collapse to `agentkit/` is FOR; flagging
    // them would fail every build.
    const found = findResidualSpecifiers(
      [
        "/**",
        " * The `@agentkit/host` port this implements.",
        " * See @agentkit/contracts for the schema.",
        " */",
        "// resolved against @agentkit/core at build time",
      ].join("\n"),
    );
    expect(found).toEqual([]);
  });

  it("says nothing about a file the rewrite fully handled", () => {
    expect(
      findResidualSpecifiers(
        'import { x } from "../host/index.js";\nexport const y = 1;\n',
      ),
    ).toEqual([]);
  });
});
