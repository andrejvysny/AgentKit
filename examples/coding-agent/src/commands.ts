/**
 * The session-lifecycle commands, shared by the CLI flags (`--list-sessions`,
 * `--usage`, `--export`) and the REPL (`/turns`, `/usage`, `/export`, ...), so
 * a flag and its slash command cannot drift apart.
 */
import { writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { REASONING_EFFORTS, type SessionReasoningEffort } from "@agentkit/host";
import type { CodingAgentApp, SessionListing } from "./app.js";

export const MODES: readonly string[] = [
  "ask",
  "auto_edit",
  "full_auto",
  "plan",
  "read_only",
];

export function isEffort(value: string): value is SessionReasoningEffort {
  return (REASONING_EFFORTS as readonly string[]).includes(value);
}

export function formatSession(s: SessionListing): string {
  return [
    s.sessionId,
    `profile=${s.profileId}`,
    `model=${s.model ?? "-"}`,
    `effort=${s.reasoningEffort ?? "-"}`,
    `turns=${s.turns}`,
    `last=${s.lastTurnStatus ?? "-"}`,
  ].join("  ");
}

export async function printSessions(app: CodingAgentApp): Promise<void> {
  const sessions = await app.listSessions();
  if (sessions.length === 0) console.log("(no sessions)");
  for (const s of sessions) console.log(formatSession(s));
}

export async function printTurns(app: CodingAgentApp): Promise<void> {
  const turns = await app.turns();
  if (turns.length === 0) console.log("(no turns)");
  for (const t of turns)
    console.log(
      `${t.turnId}  ${t.status.padEnd(16)} attempts=${t.attempts}  ${
        t.model ?? "-"
      }${t.error ? `  error=${t.error}` : ""}`,
    );
}

export async function printUsage(app: CodingAgentApp): Promise<void> {
  const usage = await app.usage();
  for (const t of usage.turns)
    console.log(
      `${t.turnId}  requests=${t.requests} prompt=${t.promptTokens} completion=${t.completionTokens} total=${t.totalTokens}`,
    );
  const { totals } = usage;
  const last =
    usage.lastPromptTokens === undefined
      ? ""
      : `  last-prompt=${usage.lastPromptTokens}`;
  console.log(
    `total  requests=${totals.requests} prompt=${totals.promptTokens} completion=${totals.completionTokens} total=${totals.totalTokens}  context-window=${usage.contextWindowTokens} (${usage.contextWindowSource})${last}`,
  );
}

export async function exportTo(
  app: CodingAgentApp,
  file: string,
): Promise<string> {
  const target = resolve(file);
  writeFileSync(
    target,
    `${JSON.stringify(await app.exportSession(), null, 2)}\n`,
  );
  return target;
}

export interface ReplContext {
  /** Stream a turn to completion, prompting for approvals as usual. */
  follow: (turnId: string) => Promise<void>;
}

/**
 * Handle a `/command` line. Returns false for `/quit` so the REPL can leave;
 * unknown commands are reported, never sent to the model.
 */
export async function handleCommand(
  app: CodingAgentApp,
  line: string,
  ctx: ReplContext,
): Promise<boolean> {
  const [command = "", ...args] = line.split(/\s+/);
  switch (command) {
    case "/quit":
    case "/exit":
      return false;
    case "/status": {
      const state = await app.state();
      console.log(
        JSON.stringify(
          {
            mode: await app.executionMode(),
            model: state.descriptor.model ?? null,
            reasoningEffort: state.descriptor.reasoningEffort ?? null,
            activeTurn: state.activeTurn ?? null,
            pending: state.pending.length,
            waiting: state.waiting?.inputId ?? null,
          },
          null,
          2,
        ),
      );
      break;
    }
    case "/transcript":
      for (const m of await app.transcript())
        console.log(`[${m.role}] ${m.content}`);
      break;
    case "/mode": {
      const mode = args[0] ?? "";
      if (!MODES.includes(mode)) {
        console.log(`unknown mode; choose one of ${MODES.join(", ")}`);
        break;
      }
      await app.setExecutionMode(mode);
      console.log(`mode is now ${mode}`);
      break;
    }
    case "/cancel": {
      const active = (await app.state()).activeTurn;
      if (active === undefined) console.log("nothing to cancel");
      else {
        await app.cancel(active.turnId);
        console.log(`cancelled ${active.turnId}`);
      }
      break;
    }
    case "/interrupt": {
      const active = (await app.state()).activeTurn;
      if (active === undefined) console.log("nothing to interrupt");
      else {
        await app.interrupt(active.turnId);
        console.log(`interrupted ${active.turnId}; /resume continues it`);
      }
      break;
    }
    case "/resume": {
      const turnId = await app.interruptedTurn();
      if (turnId === undefined) console.log("no interrupted turn");
      else {
        await app.resumeTurn(turnId);
        await ctx.follow(turnId);
      }
      break;
    }
    case "/turns":
      await printTurns(app);
      break;
    case "/usage":
      await printUsage(app);
      break;
    case "/export": {
      if (args[0] === undefined) console.log("usage: /export <file>");
      else console.log(`exported to ${await exportTo(app, args[0])}`);
      break;
    }
    case "/model": {
      const [model, effort] = args;
      if (model === undefined || (effort !== undefined && !isEffort(effort))) {
        console.log(`usage: /model <id> [${REASONING_EFFORTS.join("|")}]`);
        break;
      }
      await app.setModel(model, effort as SessionReasoningEffort | undefined);
      console.log(`model is now ${model}; applies to the NEXT turn`);
      break;
    }
    case "/effort": {
      const level = args[0] ?? "";
      if (level !== "off" && !isEffort(level)) {
        console.log(`usage: /effort <${REASONING_EFFORTS.join("|")}|off>`);
        break;
      }
      const model = (await app.state()).descriptor.model;
      if (model === undefined) {
        console.log("session has no model recorded; use /model <id> <level>");
        break;
      }
      await app.setModel(model, level === "off" ? null : level);
      console.log(`effort is now ${level}; applies to the NEXT turn`);
      break;
    }
    default:
      console.log("unknown command");
  }
  return true;
}
