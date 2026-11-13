# agentkit-sys

OS process-group and private-file primitives (ADR 0021 R9). The only crate
in the workspace that links `libc` for signalling and the only one holding
platform `unsafe`.

- `process`: `spawn_in_new_group` / `spawn_in_new_group_tokio` put a child
  in its own process group; `ProcessGroup` signals, probes and adopts only
  caller-owned groups (privileged, self-pid and self-group targets are
  refused); `pid_alive` probes a single pid.
- `private_fs`: `create_private_dir` (0700) and `create_private_file`
  (0600) for state directories holding secret material.

Non-Unix platforms compile and refuse explicitly
(`SysError::Unsupported` / `io::ErrorKind::Unsupported`).
