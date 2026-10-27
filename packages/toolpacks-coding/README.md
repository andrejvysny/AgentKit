# @agentkit/toolpacks-coding

**An optional coding tool pack for `@agentkit/host`: confined filesystem
read/search/edit tools, read-only git inspection, and sandboxed command and
test execution, plus a `CodingPermissionGuard`.** Node APIs only, no third-party
runtime dependencies, macOS-first (the shell sandbox is `/usr/bin/sandbox-exec`).

```ts
import {
  createCodingToolPack,
  createCodingPermissionGuard,
} from "@agentkit/toolpacks-coding";

const pack = createCodingToolPack({
  workspace: { root: "/abs/path/to/repo" },
  testCommands: ["bun test"], // exact strings run_tests may run
});
const guard = createCodingPermissionGuard({ mode: "auto_edit" });
// pass `pack` as a ToolSetContributor and `guard` as a ToolGuard to the host
```

| Tool | effect / capability |
|---|---|
| `read_file`, `list_dir`, `find_files`, `code_search`, `repo_map` | read / `fs.read` |
| `git_inspect` (status, diff, log, show) | read / `git.read` |
| `edit_file`, `write_file` | write / `fs.write` |
| `run_command`, `run_tests` | write / `process.exec` |

`read_file` takes `path` or `paths` (up to 8) and returns each file's `sha256`
and `totalLines` in its result data; pass the hash as `edit_file`'s
`expectedSha256`. `edit_file` keeps each line's original terminator (CRLF, LF,
CR) and refuses non-UTF-8 files (`encoding_unsupported`). `list_dir` hides
gitignored entries unless `includeIgnored`. `code_search` pages with `offset`.

## Shell command policy and standing approvals

`classifyShellCommand(command)` answers `"eligible"`, `"high_risk"` or
`"unknown"`. `eligible` is a positive allowlist (build, test, lint, typecheck and
read-only git invocations) over a character set with no shell syntax; anything
with quotes, `$`, redirects, pipes, globs, a leading `VAR=value`, or an unlisted
command is `unknown`. `high_risk` is checked first, on the raw text (deletes,
privilege, network, publish/deploy/migrate names, history-rewriting git, force
flags, ...).

```ts
import {
  createCodingPermissionGuard,
  createCodingStandingApprovals,
  createHighRiskCommandGuard,
} from "@agentkit/toolpacks-coding";

const guards = [
  createCodingPermissionGuard({ mode: "ask", authority }),
  createHighRiskCommandGuard(), // a fresh approval, even in full_auto
];
const standingApprovals = createCodingStandingApprovals({
  // Where this chat's commands run; `identity` names the environment the grant
  // is valid for (change it and old grants stop matching).
  resolve: async (chatId) => ({ root: workspaceOf(chatId), identity: "sandbox-v1" }),
});
// new TurnRunner({ ..., toolGuards: guards, standingApprovals })
```

`createCodingStandingApprovals` only keys `run_command` / `run_tests` calls whose
every approvable refusal is the plain `approval_required:` and whose command is
`eligible`; the key binds chat, tool, identity, canonical root, canonical `cwd`
(must be a directory inside the root) and the exact command text. Role ceilings,
plan mode, uncertain-outcome and high-risk refusals can never be covered. A
grant approves that command string, not what the files it runs contain later.

Every path is confined to the workspace root (symlink-aware); credential-like
paths are refused unless `allowSensitive` is set; the file tools never write
`.git`, and the shell sandbox denies writes under `<root>/.git` unless
`allowGitWrites: true` (then `.git/hooks` and `.git/config` stay denied).
`git_inspect` neutralizes every filter/textconv/external-diff driver, pager and
ssh/askpass helper the repository config defines (includes honored), and refuses
with `git_config_unreadable` if it cannot enumerate them. Shell env drops
secret-named variables, `SSH_AUTH_SOCK`, `GPG_AGENT_INFO` and values that are
URLs with embedded credentials.
Shell tools refuse to run without the sandbox (`shell_isolation_unavailable`)
unless `sandbox: "off"` is explicit. Design and limits: ADR 0018
(`docs/adr/0018-coding-tool-pack.md`).

## Concurrency

Mutations refuse lost updates; reads are never blocked.

- **Versions.** `read_file` records the sha256 it reports per chat
  (`ctx.chatId`), and whether the read showed the WHOLE file. `edit_file`
  compares the recorded version to the file under the lock and fails with
  `file_changed` when another session or an external editor changed it (a
  partial read is enough: edits are anchored on `oldText`). An overwriting
  `write_file` needs a whole-file read (or its own earlier write); a partial or
  missing read is `file_not_read`. `expectedSha256` overrides the recorded
  version on both; on `write_file` it also means "overwrite that file", so a
  missing target is `file_changed`, not a create. Both re-hash the target
  immediately before the atomic rename and refuse (`file_changed`) when the
  path now resolves to a different canonical file than the one locked. A new
  file is create-if-absent (`link()`; losing the race is `file_changed`).
  Without a `chatId` there is no recorded version, so only create-if-absent and
  the under-lock recheck apply. The ledger is bounded (LRU); an evicted entry
  means "unknown".
- **Serialisation.** Mutations of one canonical file (symlink aliases
  collapse; `realpath` returns on-disk case on macOS) run one at a time across
  every pack in the process, through one module-global in-process coordinator
  (`coordinator` option to override).
- **Shell.** Every `run_command` / `run_tests` holds an exclusive workspace
  lease from before spawn until the process tree is torn down; if survivors
  remain it is held up to 2 s more and a warning is added when they are still
  running. It waits for in-flight file mutations and blocks new ones (FIFO,
  writer preference). No command is exempt: even `git log` can run config-
  defined programs. Waiting honours the call's abort signal.
  `CoordinatorObserver.onWaiting` reports who waits on whom.
- **Wait cap.** A wait longer than `coordinationWaitMs` (default
  `min(20000, 0.66 * toolTimeoutMs)`; must be below `toolTimeoutMs`) fails with
  `workspace_busy` naming the holders. The core tool timeout also aborts the
  signal, so a timed-out call can never write afterwards.
- **Across processes.** `createLockDirWorkspaceCoordinator({ dir })` adds lock
  files under `dir`, on top of the module-global in-process coordinator. A
  lock is stale when its pid is dead or its `ps` start time changed. Lock
  files hold the tool name and a hash of the label, never command text.
  Limits: same host and pid namespace only; without `ps` only pid liveness is
  checked.
- **Limitation.** An uncoordinated external editor takes none of these locks;
  the content checks detect its changes but cannot stop a write that lands
  between the last check and the rename.
