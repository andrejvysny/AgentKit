/**
 * Single source of truth for the prebuilt platform matrix.
 *
 * Each entry maps a Rust target triple (as declared in the `napi.targets`
 * field of `packages/native/package.json`) to the npm platform package that
 * ships its prebuilt addon: package directory, loader platform key
 * (`<platform>-<arch>[-<libc>]`, the suffix the NAPI-RS loader and the
 * `--platform` build flag use), and the manifest `os`/`cpu`/`libc` gating
 * fields. `packages/native/tests/distribution.test.ts` asserts the manifests
 * agree with this table, so a target without a package (or a package with a
 * stale triple) fails the binding suite instead of silently shipping a
 * consumer-facing "native binding failed to load".
 */
export const NATIVE_PLATFORM_TARGETS = [
  {
    triple: "x86_64-unknown-linux-gnu",
    package: "native-linux-x64-gnu",
    key: "linux-x64-gnu",
    os: "linux",
    cpu: "x64",
    libc: "glibc",
    runner: "ubuntu-latest",
    cross: false,
  },
  {
    triple: "aarch64-unknown-linux-gnu",
    package: "native-linux-arm64-gnu",
    key: "linux-arm64-gnu",
    os: "linux",
    cpu: "arm64",
    libc: "glibc",
    runner: "ubuntu-24.04-arm",
    cross: false,
  },
  {
    triple: "aarch64-apple-darwin",
    package: "native-darwin-arm64",
    key: "darwin-arm64",
    os: "darwin",
    cpu: "arm64",
    libc: undefined,
    runner: "macos-latest",
    cross: false,
  },
  {
    triple: "x86_64-apple-darwin",
    package: "native-darwin-x64",
    key: "darwin-x64",
    os: "darwin",
    cpu: "x64",
    libc: undefined,
    runner: "macos-latest",
    cross: true,
  },
  {
    triple: "x86_64-pc-windows-msvc",
    package: "native-win32-x64-msvc",
    key: "win32-x64-msvc",
    os: "win32",
    cpu: "x64",
    libc: undefined,
    runner: "windows-latest",
    cross: false,
  },
];

export function nativeTargetByTriple(triple) {
  const entry = NATIVE_PLATFORM_TARGETS.find((t) => t.triple === triple);
  if (!entry)
    throw new Error(
      `unknown native target triple ${triple}; known: ${NATIVE_PLATFORM_TARGETS.map((t) => t.triple).join(", ")}`,
    );
  return entry;
}

/** Host triple for the current process, matching the staging rules the
 * gate scripts enforce (glibc-only Linux; no musl story yet). */
export function hostNativeTarget() {
  if (process.platform === "darwin")
    return nativeTargetByTriple(
      process.arch === "arm64" ? "aarch64-apple-darwin" : "x86_64-apple-darwin",
    );
  if (process.platform === "win32" && process.arch === "x64")
    return nativeTargetByTriple("x86_64-pc-windows-msvc");
  if (process.platform === "linux") {
    if (!process.report?.getReport().header.glibcVersionRuntime)
      throw new Error(
        `unsupported native staging host: ${process.platform}-${process.arch} (glibc required)`,
      );
    return nativeTargetByTriple(
      process.arch === "arm64"
        ? "aarch64-unknown-linux-gnu"
        : "x86_64-unknown-linux-gnu",
    );
  }
  throw new Error(
    `unsupported native staging host: ${process.platform}-${process.arch}`,
  );
}
