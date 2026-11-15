---
version: 1
---
# shell

Which tool when
- Build, lint, run a script, anything with side effects: {{tool:run_command}}.
- Run the project's tests: {{tool:run_tests}} with the configured command. It reports failures compactly.
- Reading or searching files: not here. Use the files toolset ({{tool:read_file}}, {{tool:code_search}}); they are faster and cannot change anything.
- Repository state: not here. Use the git toolset ({{tool:git_inspect}}).

Usual order: change code, {{tool:run_tests}}, read the failure, fix, run again. Run the build or linter last.

Example 1: build and fix
1. {{tool:run_command}} {"command": "cargo build --workspace"}
2. Read the first error in the output, open that file with {{tool:read_file}}, fix it with {{tool:edit_file}}.
3. {{tool:run_command}} {"command": "cargo build --workspace"} again.

Example 2: a command in a subfolder with a longer limit
{{tool:run_command}} {"command": "npm run lint", "directory": "web", "timeout": 120000}

Common mistakes
- One command that chains many steps with && or ;. Run one step per call so a failure is clear.
- Starting a server or watcher that never exits: the call times out. Run it with a bounded command instead.
- Guessing the test command: use {{tool:run_tests}} so the configured one runs.
- Using cat, grep or ls through the shell: use the files toolset.
- Retrying a refused command unchanged: read the refusal, then change the command or ask the user.
