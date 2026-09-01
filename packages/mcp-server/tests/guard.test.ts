import { describe, expect, it } from "bun:test";
import {
  checkRebindingGuard,
  isHostAllowed,
  isOriginAllowed,
  splitHostPort,
  DEFAULT_ALLOWED_HOSTS,
} from "../src/index.js";

describe("splitHostPort", () => {
  it("splits hostname and port, lowercasing the hostname", () => {
    expect(splitHostPort("LocalHost:8787")).toEqual({
      hostname: "localhost",
      port: "8787",
    });
    expect(splitHostPort("127.0.0.1")).toEqual({ hostname: "127.0.0.1" });
  });

  it("keeps IPv6 brackets", () => {
    expect(splitHostPort("[::1]:9000")).toEqual({
      hostname: "[::1]",
      port: "9000",
    });
    expect(splitHostPort("[::1]")).toEqual({ hostname: "[::1]" });
  });

  it("refuses what it cannot honestly parse", () => {
    expect(splitHostPort("")).toBeNull();
    expect(splitHostPort("[::1")).toBeNull();
    expect(splitHostPort("::1")).toBeNull(); // bare IPv6, illegal in Host
    expect(splitHostPort("host:")).toBeNull();
    expect(splitHostPort(":8080")).toBeNull();
  });
});

describe("isHostAllowed", () => {
  it("matches any port when the allowed entry names none", () => {
    expect(isHostAllowed("localhost:8787", ["localhost"])).toBe(true);
    expect(isHostAllowed("localhost", ["localhost"])).toBe(true);
  });

  it("requires an exact port when the allowed entry names one", () => {
    expect(isHostAllowed("localhost:8787", ["localhost:8787"])).toBe(true);
    expect(isHostAllowed("localhost:9999", ["localhost:8787"])).toBe(false);
  });

  it("refuses a host outside the list, and a missing header", () => {
    expect(isHostAllowed("evil.com", [...DEFAULT_ALLOWED_HOSTS])).toBe(false);
    expect(isHostAllowed("evil.com:8787", [...DEFAULT_ALLOWED_HOSTS])).toBe(
      false,
    );
    expect(isHostAllowed(null, [...DEFAULT_ALLOWED_HOSTS])).toBe(false);
  });

  it("covers loopback on any port by default", () => {
    for (const host of ["localhost:1", "127.0.0.1:65535", "[::1]:8787"]) {
      expect(isHostAllowed(host, [...DEFAULT_ALLOWED_HOSTS])).toBe(true);
    }
  });
});

describe("isOriginAllowed", () => {
  it("compares parsed origins, so a trailing slash is not a lockout", () => {
    expect(
      isOriginAllowed("http://localhost:5173", ["http://localhost:5173/"]),
    ).toBe(true);
  });

  it("refuses a different scheme, host or port", () => {
    const allowed = ["http://localhost:5173"];
    expect(isOriginAllowed("https://localhost:5173", allowed)).toBe(false);
    expect(isOriginAllowed("http://localhost:5174", allowed)).toBe(false);
    expect(isOriginAllowed("http://evil.com", allowed)).toBe(false);
  });

  it("refuses unparseable and opaque origins", () => {
    expect(isOriginAllowed("null", ["http://localhost"])).toBe(false);
    expect(isOriginAllowed("not a url", ["http://localhost"])).toBe(false);
  });

  it("does not honour a wildcard entry", () => {
    expect(isOriginAllowed("http://evil.com", ["*"])).toBe(false);
  });
});

describe("checkRebindingGuard", () => {
  const hosts = [...DEFAULT_ALLOWED_HOSTS];

  it("passes a loopback host with no Origin", () => {
    const headers = new Headers({ host: "127.0.0.1:8787" });
    expect(checkRebindingGuard(headers, { allowedHosts: hosts })).toBeNull();
  });

  it("refuses a rebound host even when the Origin looks fine", () => {
    const headers = new Headers({
      host: "evil.com",
      origin: "http://localhost:5173",
    });
    const refusal = checkRebindingGuard(headers, {
      allowedHosts: hosts,
      allowedOrigins: ["http://localhost:5173"],
    });
    expect(refusal).toMatch(/Host header/);
  });

  it("ignores Origin entirely when allowedOrigins is not configured", () => {
    const headers = new Headers({
      host: "localhost:8787",
      origin: "http://evil.com",
    });
    expect(checkRebindingGuard(headers, { allowedHosts: hosts })).toBeNull();
  });

  it("checks Origin only when present, once configured", () => {
    const options = { allowedHosts: hosts, allowedOrigins: ["http://ok.test"] };
    expect(
      checkRebindingGuard(new Headers({ host: "localhost" }), options),
    ).toBeNull();
    expect(
      checkRebindingGuard(
        new Headers({ host: "localhost", origin: "http://ok.test" }),
        options,
      ),
    ).toBeNull();
    expect(
      checkRebindingGuard(
        new Headers({ host: "localhost", origin: "http://evil.com" }),
        options,
      ),
    ).toMatch(/Origin/);
  });
});
