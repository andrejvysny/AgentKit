import { describe, expect, test } from "bun:test";
import { classifyShellCommand } from "../src/command-policy.js";

export const COVERED_HIGH_RISK = [
  "/bin/rm inert",
  '"rm" inert',
  "'rm' inert",
  '"/bin/rm" inert',
  "r\\m inert",
  "r''m inert",
  "env SAFE=1 /bin/rm inert",
  "SAFE=1 '/bin/rm' inert",
  "env -- /bin/rm inert",
  "env -S '/bin/rm inert'",
  'env --split-string="rm inert"',
  "command /bin/rm inert",
  "bash -lc 'printf inert'",
  "/bin/sh -c 'printf inert'",
  "env SAFE=1 /bin/bash -c 'printf inert'",
  "python3 -c 'print(1)'",
  "/usr/bin/python3 -Ic'print(1)'",
  "python3 -cprint(1)",
  "node --eval='console.log(1)'",
  "node -p '1'",
  "bun -e '1'",
  "deno eval '1'",
  "perl -e 'print 1'",
  "ruby -e 'puts 1'",
  "php -r 'echo 1;'",
  "lua -e 'print(1)'",
  "pwsh -EncodedCommand inert",
  "awk '{print 1}'",
  'echo "$(/bin/rm inert)"',
  'echo "$("rm" inert)"',
  "'/usr/bin/git' push",
  "/usr/bin/find . -delete",
  '"npm" publish',
  "node -",
  "node --input-type=module -",
  "node --input-type module",
  "printf inert | node",
  "node < ./input.js",
  "node --import=data:text/javascript,console.log(1)",
  "node --import 'data:text/javascript,console.log(1)'",
  "node --loader=data:text/javascript,console.log(1) script.js",
  "bun --preload=data:text/javascript,console.log(1) test",
];

describe("literal and indirect high-risk command forms", () => {
  test.each(COVERED_HIGH_RISK)("requires fresh approval: %s", (command) => {
    expect(classifyShellCommand(command)).toBe("high_risk");
  });
  test.each([
    "node script.js",
    "python3 script.py",
    "env NODE_ENV=test bun test",
    "command bun test",
    "./custom-test-runner",
    "ls",
  ])("ordinary indirect/unknown behavior remains: %s", (command) => {
    expect(classifyShellCommand(command)).toBe("unknown");
  });
});
