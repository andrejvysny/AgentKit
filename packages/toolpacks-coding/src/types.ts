import type { WorkspaceCoordinator } from "./coordination.js";
import type { CodingLimits } from "./limits.js";

export interface CodingWorkspace {
  /** Absolute path of the workspace root. */
  root: string;
}

export type CodingMode =
  | "read_only"
  | "ask"
  | "auto_edit"
  | "full_auto"
  /**
   * Reads only. Writes and commands are refused FINAL (no per-call approval):
   * the only escalation is a `submit_plan` the human approves, which the host
   * records as a durable grant before anything may change.
   */
  | "plan";
export type CodingRole = "implementer" | "reviewer" | "supervisor";

/**
 * The host's answer to "what is this session allowed to do RIGHT NOW".
 * Optional: without one the guard uses the mode it was constructed with, which
 * is what a single-mode CLI does.
 *
 * The host owns it deliberately — a permission mode the model could set is not
 * a permission mode.
 */
export interface CodingPermissionAuthority {
  /** The session's effective mode; undefined means "use the configured mode". */
  modeOf?(ctx: {
    chatId?: string;
    runId?: string;
  }): CodingMode | undefined | Promise<CodingMode | undefined>;
  /** The role a session runs under; undefined means "the configured role". */
  roleOf?(ctx: {
    chatId?: string;
    runId?: string;
  }): CodingRole | undefined | Promise<CodingRole | undefined>;
  /** True when THIS plan was approved by a human for this session. */
  planApproved?(ctx: {
    chatId?: string;
    planDigest: string;
  }): boolean | Promise<boolean>;
}

export interface CodingToolPackOptions {
  workspace: CodingWorkspace;
  /** Default "required": shell tools refuse to run without the sandbox. */
  sandbox?: "required" | "off";
  /** Default false: the sandbox denies network. */
  network?: boolean;
  /** Exact command strings `run_tests` may run, e.g. ["bun test"]. */
  testCommands?: readonly string[];
  /**
   * Default false: the sandbox denies every write under `<root>/.git`, so a
   * command cannot plant config, hooks or refs that later run unsandboxed.
   * When true, git state is writable but `.git/hooks` and `.git/config` stay denied.
   */
  allowGitWrites?: boolean;
  /** Default false: credential-like paths are refused for read and write. */
  allowSensitive?: boolean;
  limits?: Partial<CodingLimits>;
  clock?: () => number;
  /** Test seam: pretend to run on another platform. Default process.platform. */
  platform?: NodeJS.Platform;
  /** Test seam: path of the sandbox executable. Default /usr/bin/sandbox-exec. */
  sandboxExecPath?: string;
  /**
   * Host authority for the per-session mode/role/plan grant. When wired, the
   * pack also contributes `submit_plan`, the only way out of plan mode.
   */
  permission?: CodingPermissionAuthority;
  /**
   * Serialises file mutations and write-capable commands. Default: one
   * module-global in-process coordinator shared by every pack in the process.
   * Pass a lock-dir coordinator to also coordinate across processes.
   */
  coordinator?: WorkspaceCoordinator;
  /**
   * Longest a file mutation or command waits for a lease held by someone else
   * before failing with `workspace_busy`. Default 20_000; must be below the
   * `toolTimeoutMs` limit so the wait ends before the tool is timed out.
   */
  coordinationWaitMs?: number;
}
