// The exact platform-addon version this wrapper was built against. Embedded
// (not read from a package.json at runtime) because the same compiled
// loader ships in two layouts — `@agentkit/native/dist` and the `agentkit`
// umbrella's `dist/native` — whose nearest package.json differ in path and
// version. `scripts/release/native-lockstep.mjs` rewrites this literal with
// the rest of the native stack; `tests/distribution.test.ts` pins it to the
// wrapper's package.json version in the dev tree.
export const NATIVE_ADDON_VERSION = "0.1.0-dev";
