/**
 * Semantic version of the **wire contract** — the shape of the DTOs and JSON
 * Schemas exported from this package. Deliberately independent of the npm package
 * version in `package.json`: a packaging-only release bumps the package version
 * and leaves this alone. While this contract is `0.x`, a breaking change to a
 * DTO bumps the MINOR; from `1.0.0` on it bumps the major. Additive changes —
 * a new optional field, a new warning code, a new event type — never bump
 * either.
 */
export const CONTRACT_VERSION = "0.4.0";
