/**
 * Semantic version of the **wire contract** — the shape of the DTOs and JSON
 * Schemas exported from this package. Deliberately independent of the npm package
 * version in `package.json`: a packaging-only release bumps the package version
 * and leaves this alone, while any breaking change to a DTO bumps this major.
 */
export const CONTRACT_VERSION = "0.1.0";
