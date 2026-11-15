---
version: 1
---
# plan

Which tool when
- {{tool:submit_plan}} ends plan mode: it hands a written plan to the user and asks for the permission mode needed to carry it out.
- Outside plan mode you do not need it. Just do the work.

Usual order in plan mode: explore with read-only tools (files, git, code_intel), write the plan, call {{tool:submit_plan}} once, wait for the answer.

What a good plan holds
- The changes, file by file, in the order you will make them.
- How each step is verified (the test or build command).
- The permission mode you ask for, and why it is the smallest that works.

Example 1: a small change
{{tool:submit_plan}} {"plan": "1. Raise MAX_RETRIES in src/client.rs.\n2. Cover it in tests/client.rs.\n3. Verify with cargo test.", "mode": "auto_edit"}

Example 2: a plan the user must approve first
{{tool:submit_plan}} {"plan": "Rename the config key in three files; update the docs; no behaviour change.", "mode": "ask"}

Common mistakes
- Calling it before you have read the code: the plan then names files that do not exist.
- A plan with no verification step.
- Asking for a wider mode than the plan needs.
- Trying edits while plan mode is on: they are refused. Submit the plan first.
