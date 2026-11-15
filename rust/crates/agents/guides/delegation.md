---
version: 1
---
# delegation

Hand a self-contained task to a child agent that works in parallel and reports back. Worth it for wide searches or independent edits; not for a one-step task.

Which tool when
- Start a child: {{tool:subagent_spawn}} with a role and a complete prompt (the child sees nothing else).
- See your children: {{tool:subagent_list}}. One child's state: {{tool:subagent_status}}.
- Wait for results: {{tool:subagent_await}} with child_ids and a timeout.
- Redirect a running child: {{tool:subagent_message}}. Stop it: {{tool:subagent_cancel}}.
- Continue a finished or stopped child: {{tool:subagent_resume}}.

Usual order: spawn the children, keep working or {{tool:subagent_await}}, read the reports, combine them yourself.

Example 1: parallel exploration
1. {{tool:subagent_spawn}} {"role": "explorer", "prompt": "Find where retries are configured and report file paths and defaults."}
2. {{tool:subagent_spawn}} {"role": "explorer", "prompt": "List the tests that cover retries."}
3. {{tool:subagent_await}} {"child_ids": ["child-1", "child-2"], "timeout": 120000}

Example 2: an editing child
{{tool:subagent_spawn}} {"role": "implementer", "prompt": "Add a retry limit to src/client.rs and run the tests.", "exclusive_workspace": true}

Common mistakes
- A prompt that says "continue" or points at context the child does not have: write the whole task.
- Two editing children on the same files: use exclusive_workspace, or split by file.
- Spawning then never awaiting: results are not read until you await or check status.
- Delegating what one {{tool:read_file}} would answer.
