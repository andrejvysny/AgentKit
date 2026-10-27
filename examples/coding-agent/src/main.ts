#!/usr/bin/env bun
/**
 * A coding agent in ~200 lines of wiring, because the runtime does the work:
 * `bun run src/main.ts --repo /abs/path/to/repo --task "fix the failing test"`.
 *
 * Flags
 *   --repo <path>        required; the only directory the agent may touch
 *   --task <text>        one-shot instruction; without it, an interactive REPL
 *   --profile <id>       coding | coding/plan | coding/reviewer | coding/supervisor
 *                        | research | automation | chat   (default: coding)
 *   --mode <mode>        ask | auto_edit | full_auto | plan | read_only
 *   --model <id> --base-url <url> --api-key-env <VAR>
 *   --session <id>       stable id; reusing it continues the same session
 *   --db <path>          default ./.agentkit/coding-agent.db
 *   --yes                approve every request (still inside the role ceiling)
 *   --resume             continue a turn a previous process left interrupted
 *   --effort <level>     reasoning effort: minimal | low | medium | high
 *   --transcript         print the session transcript and exit
 *   --list-sessions      list the database's sessions and exit
 *   --usage              print the session's token usage and exit
 *   --export <file>      write the session as JSON and exit
 *   --list-profiles
 *
 * Quitting (/quit, Ctrl-C, SIGTERM) parks a running turn `interrupted`;
 * nothing restarts it until --resume or /resume.
 */
import { resolve } from "node:path";
import { createInterface } from "node:readline/promises";
import { stdin as input, stdout as output } from "node:process";
import type { AiRunEvent } from "@agentkit/contracts";
import type { CodingMode } from "@agentkit/toolpacks-coding";
import {
  REASONING_EFFORTS,
  defaultProfileRegistry,
  type SessionReasoningEffort,
} from "@agentkit/host";
import { CodingAgentApp, type PendingApproval } from "./app.js";
import {
  MODES,
  exportTo,
  handleCommand,
  isEffort,
  printSessions,
  printUsage,
} from "./commands.js";

interface Cli {
  repo: string;
  db: string;
  session: string;
  profile: string;
  mode?: CodingMode;
  model: string;
  baseUrl: string;
  apiKeyEnv?: string;
  task?: string;
  yes: boolean;
  effort?: SessionReasoningEffort;
  resume: boolean;
  transcript: boolean;
  listProfiles: boolean;
  listSessions: boolean;
  usage: boolean;
  exportFile?: string;
}

function usage(message?: string): never {
  if (message) console.error(`error: ${message}`);
  console.error(
    [
      "usage: bun run src/main.ts --repo <path> [--task <text>] [options]",
      "",
      "  --repo <path>        repository to work in (required, absolute)",
      "  --task <text>        one instruction; otherwise an interactive session",
      "  --profile <id>       agent profile (default: coding)",
      "  --mode <mode>        " + MODES.join(" | "),
      "  --model <id>         model id at --base-url",
      "  --base-url <url>     OpenAI-compatible endpoint (default: http://127.0.0.1:8000/v1)",
      "  --api-key-env <VAR>  environment variable holding the API key",
      "  --session <id>       session id to continue (default: coding-agent)",
      "  --db <path>          state file (default: ./.agentkit/coding-agent.db)",
      "  --yes                approve every tool request",
      "  --resume             continue a turn a previous process left interrupted",
      `  --effort <level>     ${REASONING_EFFORTS.join(" | ")}`,
      "  --transcript         print the transcript and exit",
      "  --list-sessions      list sessions in the database and exit",
      "  --usage              print token usage and exit",
      "  --export <file>      write the session as JSON and exit",
      "  --list-profiles",
    ].join("\n"),
  );
  process.exit(message ? 2 : 0);
}

function parse(argv: string[]): Cli {
  const cli: Cli = {
    repo: "",
    db: "./.agentkit/coding-agent.db",
    session: "coding-agent",
    profile: "coding",
    model: process.env["AGENTKIT_MODEL"] ?? "qwen3-coder",
    baseUrl: process.env["AGENTKIT_BASE_URL"] ?? "http://127.0.0.1:8000/v1",
    yes: false,
    resume: false,
    transcript: false,
    listProfiles: false,
    listSessions: false,
    usage: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i] as string;
    const next = (): string => {
      const value = argv[i + 1];
      if (value === undefined) usage(`${arg} needs a value`);
      i += 1;
      return value;
    };
    switch (arg) {
      case "--repo":
        cli.repo = next();
        break;
      case "--db":
        cli.db = next();
        break;
      case "--session":
        cli.session = next();
        break;
      case "--profile":
        cli.profile = next();
        break;
      case "--mode": {
        const mode = next();
        if (!MODES.includes(mode)) usage(`unknown --mode ${mode}`);
        cli.mode = mode as CodingMode;
        break;
      }
      case "--model":
        cli.model = next();
        break;
      case "--base-url":
        cli.baseUrl = next();
        break;
      case "--api-key-env":
        cli.apiKeyEnv = next();
        break;
      case "--task":
        cli.task = next();
        break;
      case "--yes":
        cli.yes = true;
        break;
      case "--resume":
        cli.resume = true;
        break;
      case "--transcript":
        cli.transcript = true;
        break;
      case "--list-profiles":
        cli.listProfiles = true;
        break;
      case "--list-sessions":
        cli.listSessions = true;
        break;
      case "--usage":
        cli.usage = true;
        break;
      case "--export":
        cli.exportFile = next();
        break;
      case "--effort": {
        const effort = next();
        if (!isEffort(effort))
          usage(`--effort must be one of ${REASONING_EFFORTS.join(", ")}`);
        cli.effort = effort;
        break;
      }
      case "--help":
      case "-h":
        usage();
        break;
      default:
        usage(`unknown argument ${arg}`);
    }
  }
  if (!cli.listProfiles && cli.repo === "") usage("--repo is required");
  if (cli.repo !== "") cli.repo = resolve(cli.repo);
  cli.db = resolve(cli.db);
  return cli;
}

function describeEvent(event: AiRunEvent): string | undefined {
  switch (event.type) {
    case "run.tool.requested":
      return `→ ${event.data.toolName} ${abbreviate(event.data.argumentsJson)}`;
    case "run.tool.succeeded":
      return `✓ ${event.data.toolName}${
        event.data.truncated ? " (truncated)" : ""
      }`;
    case "run.tool.failed":
      return `✗ ${event.data.toolName}: ${event.data.errorMessage}`;
    case "run.warning":
      return `! ${event.data.code}: ${event.data.message}`;
    case "run.failed":
      return `✗ run failed: ${event.data.errorMessage}`;
    default:
      return undefined;
  }
}

function abbreviate(json: string): string {
  const flat = json.replace(/\s+/g, " ");
  return flat.length <= 120 ? flat : `${flat.slice(0, 117)}...`;
}

async function decide(
  cli: Cli,
  rl: ReturnType<typeof createInterface>,
  approval: PendingApproval,
): Promise<"approve" | "deny"> {
  if (cli.yes) return "approve";
  console.log("\n--- approval requested -------------------------------");
  console.log(
    `tool:  ${approval.toolName}${
      approval.argsSha256 === undefined
        ? ""
        : `  (args sha256 ${approval.argsSha256.slice(0, 12)})`
    }`,
  );
  if (approval.plan !== undefined) {
    console.log("PLAN APPROVAL — approving grants the host's execution mode:");
    console.log(approval.plan.plan ?? "(no plan text)");
    if (approval.plan.requestedMode !== undefined)
      console.log(`(the plan asked for: ${approval.plan.requestedMode})`);
  } else {
    console.log(`call:  ${approval.prompt}`);
  }
  console.log("-----------------------------------------------------------");
  for (;;) {
    const answer = (await rl.question("approve this call? [y/n/e(scap)e] "))
      .trim()
      .toLowerCase();
    if (answer === "y" || answer === "yes") return "approve";
    if (answer === "n" || answer === "no") return "deny";
    if (answer === "e" || answer === "escape") {
      console.log(
        "leaving the turn parked; re-run with --resume to answer it.",
      );
      process.exit(0);
    }
  }
}

/**
 * Ctrl-C and SIGTERM close the app, which parks a running turn `interrupted`
 * instead of losing it. A second signal means "now": exit without waiting.
 */
function installSignalHandlers(app: CodingAgentApp): void {
  let closing = false;
  const onSignal = (signal: NodeJS.Signals): void => {
    if (closing) process.exit(130);
    closing = true;
    console.error(
      `\n${signal}: interrupting the running turn (signal again to force exit)`,
    );
    app
      .close()
      .catch((error) => console.error(error))
      .finally(() => process.exit(signal === "SIGINT" ? 130 : 143));
  };
  process.on("SIGINT", onSignal);
  process.on("SIGTERM", onSignal);
}

async function main(): Promise<void> {
  const cli = parse(process.argv.slice(2));
  if (cli.listProfiles) {
    // No app, no database, no model: a profile list is a fact about the runtime.
    for (const profile of defaultProfileRegistry().list())
      console.log(
        `${profile.id.padEnd(20)} ${profile.kind.padEnd(10)} ${
          profile.role ?? "-"
        }  mode=${profile.permissionMode}  tools=${profile.tools.length}`,
      );
    return;
  }
  const app = await CodingAgentApp.open({
    repo: cli.repo,
    dbPath: cli.db,
    sessionId: cli.session,
    profileId: cli.profile,
    model: cli.model,
    baseUrl: cli.baseUrl,
    ...(cli.apiKeyEnv === undefined ? {} : { apiKeyEnv: cli.apiKeyEnv }),
    ...(cli.mode === undefined ? {} : { mode: cli.mode }),
    ...(cli.effort === undefined ? {} : { reasoningEffort: cli.effort }),
  });
  installSignalHandlers(app);

  if (cli.listSessions) {
    await printSessions(app);
    await app.close();
    return;
  }
  await app.ensureSession();
  if (cli.transcript || cli.usage || cli.exportFile !== undefined) {
    if (cli.transcript)
      for (const message of await app.transcript())
        console.log(
          `[${message.role}${message.toolName ? `:${message.toolName}` : ""}] ${message.content}`,
        );
    if (cli.usage) await printUsage(app);
    if (cli.exportFile !== undefined)
      console.log(`exported to ${await exportTo(app, cli.exportFile)}`);
    await app.close();
    return;
  }

  await app.startWorker();
  const rl = createInterface({ input, output });
  const follow = async (turnId: string): Promise<void> => {
    const status = await app.waitForTurn(turnId, {
      decide: (approval) => decide(cli, rl, approval),
      onEvent: (event) => {
        const line = describeEvent(event);
        if (line) console.log(line);
      },
    });
    const answers = (await app.transcript()).filter(
      (m) => m.role === "assistant",
    );
    const last = answers[answers.length - 1];
    console.log(`\n${last?.content ?? "(no answer)"}\n[turn ${status}]`);
  };
  const runOnce = async (text: string): Promise<void> => {
    const started = await app.send(text);
    if (started.kind !== "turn") {
      console.log(
        "(queued as a follow-up; it runs when the current turn ends)",
      );
      return;
    }
    await follow(started.turnId);
  };
  const resumeInterrupted = async (): Promise<void> => {
    // Crashed leases become `interrupted` first; a clean quit already left it so.
    await app.recover();
    const active = (await app.state()).activeTurn;
    if (active === undefined) return;
    if (active.status === "running") {
      console.log(
        `turn ${active.turnId} still holds a live lease (another process, or one that has not expired yet); retry shortly`,
      );
      return;
    }
    console.log(`resuming turn ${active.turnId} (${active.status})`);
    if (active.status === "interrupted") await app.resumeTurn(active.turnId);
    await follow(active.turnId);
  };

  try {
    if (cli.resume) await resumeInterrupted();
    if (cli.task !== undefined) {
      await runOnce(cli.task);
      return;
    }
    console.log(
      `coding agent · repo=${cli.repo} · profile=${cli.profile} · mode=${
        (await app.executionMode()) ?? "profile default"
      }`,
    );
    console.log(
      "commands: /mode <m> /status /transcript /cancel /interrupt /resume /turns /usage /export <file> /model <id> [effort] /effort <level|off> /quit, or type an instruction",
    );
    for (;;) {
      const line = (await rl.question("> ")).trim();
      if (line === "") continue;
      if (line.startsWith("/")) {
        if (!(await handleCommand(app, line, { follow }))) return;
        continue;
      }
      await runOnce(line);
    }
  } finally {
    rl.close();
    await app.close();
  }
}

if (import.meta.main) {
  main().catch((error) => {
    console.error(
      error instanceof Error ? (error.stack ?? error.message) : error,
    );
    process.exit(1);
  });
}
