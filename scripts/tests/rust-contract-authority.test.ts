import { describe, expect, it } from "bun:test";
import * as schemas from "../../packages/contracts/src/schemas.js";
import { loadSchemaBundle } from "../export-rust-fixtures.js";

/**
 * ADR 0022: the Rust bundle (rust/crates/contracts/src/schema_bundle.json) is the
 * wire authority; the hand-written TypeBox schemas must emit exactly the same
 * JSON Schema. Fix drift in TypeScript, or change the Rust bundle first.
 */
describe("TypeBox schemas match the Rust-owned schema bundle", () => {
  const bundle = loadSchemaBundle() as Record<string, unknown>;
  const emitted = JSON.parse(JSON.stringify(schemas)) as Record<
    string,
    unknown
  >;

  it("exports exactly the bundled schema names", () => {
    expect(Object.keys(emitted).sort()).toEqual(Object.keys(bundle).sort());
    expect(Object.keys(bundle)).toHaveLength(130);
  });

  it.each(Object.keys(bundle))("%s", (name) => {
    expect(emitted[name]).toEqual(bundle[name]);
  });
});
