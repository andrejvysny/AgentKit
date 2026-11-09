import { describe, expect, it } from "bun:test";
import { describeSecretStoreConformance } from "./support/secret-store-conformance.js";
import { MemorySecretStore } from "../src/index.js";

describeSecretStoreConformance({
  name: "MemorySecretStore",
  create: async () => ({ store: new MemorySecretStore() }),
  test: { describe, it, expect },
});
