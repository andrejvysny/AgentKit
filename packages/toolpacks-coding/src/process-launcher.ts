import { spawn, type ChildProcess } from "node:child_process";
import type { Readable, Writable } from "node:stream";
import { EXEC_ID_ENV } from "./process-tree.js";

export const GUARDIAN_PROTOCOL = "agentkit-command-guardian-v1" as const;

/** Arguments are never evaluated as shell syntax. Job control stays disabled. */
const GUARDIAN = `
set +m
IFS= read -r admission <&3 || exit 0
[ "$admission" = "${GUARDIAN_PROTOCOL}" ] || exit 125
cd -- "$1" || { printf '125\\n' >&4; exit 125; }
shift
(
  while IFS= read -r ignored <&3; do :; done
  kill -STOP 0
) &
watcher=$!
/usr/bin/env -i -- "$@" 3<&- 4>&- &
command=$!
wait "$command"
result=$?
printf '%s\\n' "$result" >&4
wait "$watcher"
`;

export interface GuardianResult {
  /** POSIX wait status preserves 128+signal without guessing a signal name. */
  exitCode: number | null;
  signal: NodeJS.Signals | null;
  spawnError?: string;
}

export interface GuardianCommand {
  file: string;
  args: string[];
  cwd: string;
  env: NodeJS.ProcessEnv;
}

export interface GuardianLaunch {
  child: ChildProcess;
  result: Promise<GuardianResult>;
  release(): void;
  dispose(): void;
}

function commandStatus(
  child: ChildProcess,
  status: Readable,
): Promise<GuardianResult> {
  return new Promise((resolve) => {
    let pending = "";
    child.once("error", (error) =>
      resolve({ exitCode: null, signal: null, spawnError: error.message }),
    );
    status.on("data", (chunk: Buffer) => {
      pending += chunk.toString();
      const end = pending.indexOf("\n");
      if (end < 0) return;
      const code = Number(pending.slice(0, end));
      resolve(
        Number.isInteger(code) && code >= 0 && code <= 255
          ? { exitCode: code, signal: null }
          : {
              exitCode: null,
              signal: null,
              spawnError: "invalid guardian status",
            },
      );
    });
  });
}

/** Works from Node, Bun and compiled Bun without reexecuting the application. */
export function launchGuardian(
  scratch: string,
  execId: string,
  command: GuardianCommand,
): GuardianLaunch {
  const environment = Object.entries(command.env).flatMap(([name, value]) =>
    value === undefined ? [] : [`${name}=${value}`],
  );
  const child = spawn(
    "/bin/bash",
    [
      "--noprofile",
      "--norc",
      "-c",
      GUARDIAN,
      "agentkit-command-guardian",
      command.cwd,
      ...environment,
      command.file,
      ...command.args,
    ],
    {
      cwd: scratch,
      env: {
        PATH: "/usr/bin:/bin",
        HOME: scratch,
        TMPDIR: scratch,
        [EXEC_ID_ENV]: execId,
      },
      detached: true,
      stdio: ["ignore", "pipe", "pipe", "pipe", "pipe"],
    },
  );
  const control = child.stdio[3] as Writable;
  const status = child.stdio[4] as Readable;
  control.on("error", () => {});
  return {
    child,
    result: commandStatus(child, status),
    release() {
      control.write(`${GUARDIAN_PROTOCOL}\n`);
    },
    dispose() {
      control.destroy();
      status.destroy();
    },
  };
}
