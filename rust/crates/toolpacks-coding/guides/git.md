---
version: 1
---
# git

Read-only repository questions: {{tool:git_inspect}}. It has one form per question; where it is a single tool, pass command with the form name.

Which form when
- What changed in the working tree: status.
- The content of the changes: diff. Staged changes need staged true, a comparison with a commit needs rev, one file needs path.
- Recent history: log. Bound it with limit, narrow it with path.
- One commit with its changes: show, with rev.
- Anything that writes (add, commit, checkout, push): not available here. Say what you would run and let the user decide.

Usual order: status first, then diff for the files that matter, then log or show only when the history is the question.

Example 1: review your own change before reporting
1. status
2. diff with path src/client.rs

Example 2: find who last touched a file
log with path src/client.rs and limit 5, then show with rev of the commit you want.

Common mistakes
- Running git through the shell for read-only questions: use this toolset, its output is bounded and safe.
- Asking for a diff of the whole repository when you need one file: pass path.
- Reading a long log in full: set limit.
- Treating an empty diff as "nothing changed" without also checking staged true.
