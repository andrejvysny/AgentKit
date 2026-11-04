/**
 * The wrapper turns an unsupported host into one precise error before the
 * generated loader's "Cannot find module" noise. `nativePlatformKey` is the
 * pure half (internal to `src/loader.ts`, not part of the package API).
 */
import { describe, expect, test } from "bun:test";
import { NATIVE_PLATFORM_TARGETS } from "../../../rust/scripts/native-platform-targets.mjs";
import { assertSupportedPlatform, nativePlatformKey } from "../src/loader.js";

describe("nativePlatformKey", () => {
  test("derives every shipped key from platform/arch/libc", () => {
    expect(nativePlatformKey("linux", "x64", "2.39")).toBe("linux-x64-gnu");
    expect(nativePlatformKey("linux", "arm64", "2.39")).toBe("linux-arm64-gnu");
    expect(nativePlatformKey("darwin", "arm64", null)).toBe("darwin-arm64");
    expect(nativePlatformKey("darwin", "x64", null)).toBe("darwin-x64");
    expect(nativePlatformKey("win32", "x64", null)).toBe("win32-x64-msvc");
    expect(NATIVE_PLATFORM_TARGETS.map((t) => t.key).sort()).toEqual(
      [
        nativePlatformKey("linux", "x64", "2"),
        nativePlatformKey("linux", "arm64", "2"),
        nativePlatformKey("darwin", "arm64", null),
        nativePlatformKey("darwin", "x64", null),
        nativePlatformKey("win32", "x64", null),
      ].sort(),
    );
  });

  test("classifies musl, unknown libc as gnu, and unsupported arches", () => {
    expect(nativePlatformKey("linux", "x64", undefined)).toBe("linux-x64-musl");
    expect(nativePlatformKey("linux", "x64", null)).toBe("linux-x64-gnu");
    expect(nativePlatformKey("win32", "arm64", null)).toBe("win32-arm64-msvc");
    expect(nativePlatformKey("freebsd", "x64", null)).toBe("freebsd-x64");
  });
});

describe("assertSupportedPlatform", () => {
  function withProcess(overrides: Record<string, unknown>, run: () => void) {
    const originals = Object.fromEntries(
      Object.keys(overrides).map((k) => [
        k,
        Object.getOwnPropertyDescriptor(process, k),
      ]),
    );
    for (const [k, v] of Object.entries(overrides))
      Object.defineProperty(process, k, { value: v, configurable: true });
    try {
      run();
    } finally {
      for (const [k, d] of Object.entries(originals))
        if (d) Object.defineProperty(process, k, d);
    }
  }

  test("accepts the host the suite runs on", () => {
    // The suite itself needs the native addon, so this host is supported.
    expect(() => assertSupportedPlatform()).not.toThrow();
  });

  test("refuses musl and win32-arm64 with the supported list", () => {
    withProcess(
      {
        platform: "linux",
        arch: "x64",
        report: { getReport: () => ({ header: {} }) },
      },
      () =>
        expect(() => assertSupportedPlatform()).toThrow(
          "@agentkit/native: unsupported platform linux-x64-musl; supported: linux-x64-gnu, linux-arm64-gnu, darwin-arm64, darwin-x64, win32-x64-msvc",
        ),
    );
    withProcess({ platform: "win32", arch: "arm64" }, () =>
      expect(() => assertSupportedPlatform()).toThrow(
        "unsupported platform win32-arm64",
      ),
    );
  });
});
