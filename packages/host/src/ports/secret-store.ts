/**
 * Secret material (API keys, tokens) kept out of the records that describe it.
 *
 * A provider config is read, listed, logged and shipped to a UI; the key it uses
 * must not travel with it. Hosts store a `ref` on the config and resolve the
 * value here, at the moment a client is built.
 */
export interface SecretStore {
  /** Null when unset — an absent secret is a normal state, not an error. */
  get(ref: string): Promise<string | null>;
  set(ref: string, value: string): Promise<void>;
  delete(ref: string): Promise<void>;
  /** The refs that exist. Values are never listed. */
  listRefs(): Promise<string[]>;
}
