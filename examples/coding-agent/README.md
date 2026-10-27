# coding-agent — a standalone AgentKit coding agent

A coding agent you can actually run: an explicit repository, an
OpenAI-compatible endpoint, a persistent session on SQLite, the confined coding
tool pack, human approvals, and crash-resumable turns. No Pi, no OneMind, no
second runtime — `@agentkit/host` plus `@agentkit/toolpacks-coding`, wired in
about 800 lines across two files — the wiring in `src/app.ts` and the CLI shell in
`src/main.ts`.

It exists to prove the claim from the other side: everything a coding agent
needs is in AgentKit, so a host does not have to write an agent loop.

```bash
bun run src/main.ts \
  --repo /absolute/path/to/your/repo \
  --base-url http://127.0.0.1:8000/v1 \
  --model qwen3-coder \
  --api-key-env OPENAI_API_KEY \
  --task "the sum test fails; find why, fix it, run the tests"
```

Without `--task` you get a REPL on the same session:

```
> fix the flaky test
→ repo_map {}
✓ repo_map
→ read_file {"path":"test/sum.test.ts"}
✓ read_file
--- approval requested -------------------------------
tool:  edit_file
call:  edit_file src/sum.ts (+1 -1)
-----------------------------------------------------------
approve this call? [y/n/e(scap)e] y
✓ edit_file
→ run_tests {"command":"bun test"}
✓ run_tests

fixed: sum subtracted instead of adding.
[turn completed]
> /status
{ "mode": "ask", "activeTurn": null, "pending": 0, "waiting": null }
```

`--session <id>` is the whole of continuation: pass the same id tomorrow and the
agent still has yesterday's transcript. `--transcript` prints what the store
holds, including the tool results the model reads. `--resume` adopts a turn a
previous process left unfinished.

## What the host enforces, not what the prompt asks

| Mechanism | Where it lives | What a prompt cannot do |
| --- | --- | --- |
| Tool allowlist | profile → `stageRegistry` | make a removed tool visible or callable |
| Permission mode | session's durable execution state → profile → host floor | pick its own mode |
| Role ceiling | coding guard (`supervisor` read, `reviewer` read+exec, `implementer` read+write+exec) | lift a ceiling, even under `full_auto` |
| Plan mode | guard refuses writes FINAL; only an approved `submit_plan` grants execution | edit while planning, or choose the mode it is granted |
| Workspace | tool pack resolves every path under `--repo` | escape the repository |
| Uncertain mutations | side-effect receipts from the durable log | re-run a call whose outcome nobody recorded |

Approvals are durable records, not prompts in a console: an approval is bound to
one tool call and the hash of its arguments, is spent once, and survives a
restart. Denying is a recorded outcome the model reads on the next turn.

`--yes` approves every request. It does not widen anything: a supervisor still
cannot write, and a plan is still needed to leave plan mode.

## Profiles

`--list-profiles` prints the registry. The built-ins:

| profile | tools | mode | role | use |
| --- | --- | --- | --- | --- |
| `chat` | none | read_only | supervisor | conversation; no side effects at all |
| `coding` | 11 coding tools | ask | implementer | the default above |
| `coding/plan` | 11 coding tools | plan | implementer | read, propose, wait |
| `coding/supervisor` | 6 reads | read_only | supervisor | read and judge, run nothing |
| `coding/reviewer` | 7 (reads + `run_tests`) | ask | reviewer | read and run tests, never edit |
| `research` | 7 (reads + `run_command`) | ask | reviewer | browsing, summarising, no writes |
| `automation` | 9 | full_auto | implementer | unattended jobs; confinement and ceiling are the human's substitute |

A profile is host-side data. Its tool list is enforced at registry staging (the
model is never shown a tool it may not use, and a request for one is answered
`tool_missing` with a `profile_tools_pruned` warning on the log), its limits may
only lower the host's ceilings, and its instructions are the only advisory part.

## Model

Any OpenAI-compatible endpoint: vLLM, LM Studio, Ollama's `/v1`, a corporate
gateway. `--base-url`, `--model`, and `--api-key-env` naming the environment
variable that holds the key — the key goes from the environment to the provider
client and never into the database.

```bash
# vLLM
bun run src/main.ts --repo ~/code/app --base-url http://localhost:8000/v1 --model qwen3-coder --task "..."
# LM Studio
bun run src/main.ts --repo ~/code/app --base-url http://localhost:1234/v1 --model llama-3.1-8b --task "..."
# Ollama
bun run src/main.ts --repo ~/code/app --base-url http://localhost:11434/v1 --model qwen2.5-coder:7b --task "..."
```

## State

`.agentkit/coding-agent.db` (SQLite, override with `--db`) holds conversations,
messages, tool results, tasks, attempts, leases, the durable event log, human
requests and the session's execution state. Deleting it starts the agent's
memory fresh; nothing else in the process is stateful.

## Tests

```bash
bun test tests        # offline: a scripted provider drives the real runtime
```

The acceptance suite covers the whole promise, against a real temporary
repository and the real tools — no network, no model:

- explores, edits and runs the project's own `bun test` in one session;
- asks a human before mutating, and a denial is honoured (file unchanged, the
  denial recorded where the next turn reads it);
- a later approval of the same call does land — a denial binds a call, not a
  session;
- plan mode blocks writes until a human approves the plan, the plan's requested
  mode is shown and not obeyed, and the granted mode can never be plan itself;
- continuing the session in a new process replays the earlier turns;
- **crash recovery**: a child process is SIGKILLed while a real command is in
  flight; a new process adopts the turn, reports the interrupted call as
  `tool_result_uncertain` instead of pretending it never ran, refuses to
  re-issue the identical command without a human, and runs it exactly once more
  after approval.

## What is deliberately not here

- **No agent loop.** `@agentkit/core`'s run loop owns iteration, tool steps and
  cancellation. A second loop is the thing this example must not contain.
- **No scheduler.** Tasks, leases and recovery belong to `TaskStore` and the
  task runner; `CodingAgentApp.recover()` is an explicit adoption, so restarting
  the CLI never by itself resumes a turn.
- **No MCP, no attachments, no web UI.** The transport example covers those.
- **No OneMind imports.** That integration is a later phase; this directory is
  the proof the runtime stands alone.
