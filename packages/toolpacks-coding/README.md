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
