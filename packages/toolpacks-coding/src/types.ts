import type { CodingLimits } from "./limits.js";

export interface CodingWorkspace {
  /** Absolute path of the workspace root. */
  root: string;
}

export type CodingMode = "read_only" | "ask" | "auto_edit" | "full_auto";
export type CodingRole = "implementer" | "reviewer" | "supervisor";

export interface CodingToolPackOptions {
  workspace: CodingWorkspace;
  /** Default "required": shell tools refuse to run without the sandbox. */
  sandbox?: "required" | "off";
  /** Default false: the sandbox denies network. */
  network?: boolean;
  /** Exact command strings `run_tests` may run, e.g. ["bun test"]. */
  testCommands?: readonly string[];
  /** Default false: credential-like paths are refused for read and write. */
  allowSensitive?: boolean;
  limits?: Partial<CodingLimits>;
  clock?: () => number;
  /** Test seam: pretend to run on another platform. Default process.platform. */
  platform?: NodeJS.Platform;
  /** Test seam: path of the sandbox executable. Default /usr/bin/sandbox-exec. */
  sandboxExecPath?: string;
}
