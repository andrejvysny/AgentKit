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
 *   --resume             adopt a turn a previous process left unfinished
 *   --transcript         print the session transcript and exit
 *   --list-profiles
 */
import { resolve } from "node:path";
import { createInterface } from "node:readline/promises";
import { stdin as input, stdout as output } from "node:process";
import type { AiRunEvent } from "@agentkit/contracts";
import type { CodingMode } from "@agentkit/toolpacks-coding";
import { defaultProfileRegistry } from "@agentkit/host";
import { CodingAgentApp, type PendingApproval } from "./app.js";

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
  resume: boolean;
  transcript: boolean;
  listProfiles: boolean;
}

const MODES: readonly string[] = [
  "ask",
  "auto_edit",
  "full_auto",
  "plan",
  "read_only",
];

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
      "  --resume             adopt work a previous process left unfinished",
      "  --transcript         print the transcript and exit",
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
      case "--help":
      case "-h":
        usage();
        break;
      default:
        usage(`unknown argument ${arg}`);
    }
  }
  if (!cli.listProfiles && !cli.transcript && cli.repo === "")
    usage("--repo is required");
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
  console.log(`tool:  ${approval.toolName}`);
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
  });

  await app.ensureSession();
  if (cli.transcript) {
    for (const message of await app.transcript())
      console.log(
        `[${message.role}${message.toolName ? `:${message.toolName}` : ""}] ${message.content}`,
      );
    await app.close();
    return;
  }

  await app.startWorker();
  if (cli.resume) {
    const active = (await app.state()).activeTurn;
    if (active !== undefined) {
      console.log(
        `adopting unfinished turn ${active.turnId} (${active.status})`,
      );
      await app.recover();
    }
  }

  const rl = createInterface({ input, output });
  const runOnce = async (text: string): Promise<void> => {
    const started = await app.send(text);
    if (started.kind !== "turn") {
      console.log(
        "(queued as a follow-up; it runs when the current turn ends)",
      );
      return;
    }
    const status = await app.waitForTurn(started.turnId, {
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

  try {
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
      "commands: /mode <m> /status /transcript /cancel /quit, or type an instruction",
    );
    for (;;) {
      const line = (await rl.question("> ")).trim();
      if (line === "") continue;
      if (line === "/quit" || line === "/exit") return;
      if (line === "/status") {
        const state = await app.state();
        console.log(
          JSON.stringify(
            {
              mode: await app.executionMode(),
              activeTurn: state.activeTurn ?? null,
              pending: state.pending.length,
              waiting: state.waiting?.inputId ?? null,
            },
            null,
            2,
          ),
        );
        continue;
      }
      if (line === "/transcript") {
        for (const m of await app.transcript())
          console.log(`[${m.role}] ${m.content}`);
        continue;
      }
      if (line.startsWith("/mode ")) {
        const mode = line.slice(6).trim();
        if (!MODES.includes(mode)) {
          console.log(`unknown mode; choose one of ${MODES.join(", ")}`);
          continue;
        }
        await app.setExecutionMode(mode);
        console.log(`mode is now ${mode}`);
        continue;
      }
      if (line === "/cancel") {
        const active = (await app.state()).activeTurn;
        if (active === undefined) console.log("nothing to cancel");
        else {
          await app.cancel(active.turnId);
          console.log(`cancelled ${active.turnId}`);
        }
        continue;
      }
      if (line.startsWith("/")) {
        console.log("unknown command");
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
