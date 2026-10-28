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
command is `unknown`. `high_risk` is checked first (deletes, privilege, network,
publish/deploy/migrate names, history-rewriting git, force flags, ...). Literal
executable paths, single/double quotes, escaped words and concatenated literal
quotes are normalized for this risk scan: `/bin/rm`, `"rm"`, `r''m` and `r\m`
need the same fresh decision as `rm`. Assignments and `env`/`command` wrappers
cannot hide these literal words. `env -S`/`--split-string`, shell inline execution
(`sh`/`bash`/`zsh`/`dash`/`ksh`/`fish` `-c`, including bundled flags), and supported
interpreter evaluation forms require a fresh decision even for benign code.
These include Python/PyPy `-c`, Node/Bun/Ruby/Perl/Lua `-e`, Node `-p`, PHP `-r`,
Deno `eval`, PowerShell command/encoded-command, and awk programs.

This is a conservative literal scan, not a complete shell parser or a proof of
arbitrary code's effects. Variable-generated executable names, aliases, custom
executables, script files and transitive test/build code can have effects this
scan cannot identify. An ordinary `unknown` command retains the selected mode's
behavior: `full_auto` runs it, `ask`/`auto_edit` ask, and final role/mode ceilings
still win. Sandbox/capability restrictions remain independent. The inert policy
tests qualify classifier and real host guard composition without spawning any
process; they do not qualify the blocked native Full-auto GUI acceptance branch.

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
plan mode, uncertain-outcome and high-risk refusals can never be covered. Every
covered high-risk execution needs a fresh allow-once decision, never a remembered
grant.

The version-2 key also binds a bounded execution fingerprint. Unchanged covered
context reuses the grant; a changed fingerprint no longer matches and requests
ordinary approval again because the command's covered execution context changed.
Existing version-1 grants therefore require approval once under this policy.
The fingerprint covers:

- Executable resolution through `PATH`, canonical path, device/inode, permissions,
  size and nanosecond modification/change times. `npx`/`bunx` also resolve the
  selected local CLI in ancestor `node_modules/.bin`, then `PATH`; unresolved
  executables cannot receive standing grants. Executable identity uses filesystem
  metadata, not an unbounded binary-content hash.
- Recognized runner/dependency metadata from `cwd` through the workspace root:
  package manifests/lockfiles, Bun/npm/pnpm/Yarn configuration, Python/uv/pytest,
  Cargo/toolchain configuration, Go modules/workspaces, Makefiles, TypeScript,
  Vitest/Vite/Jest, ESLint, Prettier and Biome configuration. Missing recognized
  files are bound too, so adding one invalidates the grant.
- Literal direct script-entrypoint files named by the selected package script
  and its pre/post hooks; explicit `--config`, `--project`, `--manifest-path`,
  `--modfile`, TypeScript `-p` and pytest `-c` files. Source files discovered or
  imported by those entrypoints are not hashed. Ordinary project source/test
  edits remain covered unless they change one of these bound inputs.
- A fixed set of relevant environment variables, after the runner's secret
  filtering: `PATH`, home/shell/startup paths, locale, Node/Bun/Python options,
  virtual environment, Cargo/Rust/Go paths and flags, Make flags, dynamic-library
  resolution and npm configuration paths/registry. Startup/user configuration
  explicitly named by `BASH_ENV`, `ENV` or `npm_config_userconfig` is also bound.
  Hosts with different execution environments must supply `environment`; custom
  behavior-changing context belongs in `identity`/`context`.

Bounds: 16 ancestor directories, 128 present files, 8 MiB per file, 32 MiB total,
128 KiB of covered environment and 128 `PATH` entries. Unreadable, malformed,
unresolved or oversized covered context offers no standing grant. This bounded
fingerprint does not prove transitive behavior, protect the check-to-execution
interval, or cover every runner's external/global configuration. Sandboxing and
workspace coordination remain required independent controls.

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
