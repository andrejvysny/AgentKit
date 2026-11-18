# Child final answer skips internal tool-round messages

**Problem.** After a child made a tool call, the subagent result envelope carried `summary: ""`. The store holds the turn's answer message, then the tool round's internal assistant message (content `""`, metadata `internal: true`) with a higher order key under the same run. `final_answer` took the highest order key, so it returned the empty internal message.

**Live evidence (2026-11-18).** Qwen (`qwen3.8-flash-next` via LiteLLM), smoke run `acceptance-runs/2026-11-18T164927Z-eval`: 10 of 10 child envelopes had an empty summary. OpenAI confirmation runs: luna 9/48, terra 1/21, sol 4/33.

**Fix.** `final_answer` (`rust/crates/agents/src/subagents/service.rs`) ignores records with `metadata.internal == true`.

**Test.** `crates/agents/tests/subagents.rs::internal_tool_round_message_does_not_replace_the_answer`: failed before the fix (`left: Some("")`, `right: Some("the child answer")`), passes after. `cargo test --locked -p agentkit-agents`: all pass (subagents binary 91 passed). fmt and clippy (`-p agentkit-agents --all-targets -D warnings`) clean.

**Gates** (`node scripts/gates.mjs --only=fmt,clippy,test,arch`, 2026-11-18): fmt, clippy and arch pass. The test step failed on `agentkit-claude --lib` only: 4 transport/discovery tests (7 when re-run alone) time out spawning a fake `claude` subprocess (`frame within budget: Elapsed(())`, `probe_timeout ... exceeded the 5000 ms probe bound`). `agentkit-claude` does not depend on `agentkit-agents` (`cargo tree`), so this change cannot affect them; they are environmental timing failures under shared-machine load (load average 4–7).
