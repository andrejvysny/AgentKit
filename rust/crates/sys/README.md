# agentkit-sys

OS process-group and private-file primitives (ADR 0021 R9). The only crate
in the workspace that links `libc` for signalling and the only one holding
platform `unsafe`.

- `process`: `spawn_in_new_group` / `spawn_in_new_group_tokio` put a child
  in its own process group; `ProcessGroup` signals, probes and adopts only
  caller-owned groups (privileged, self-pid and self-group targets are
  refused); `signal_process` signals one pid (privileged and self pids
  refused); `kill_probe` wraps `kill(target, 0)` (pid, group and own-group
  addressing); `peek_child_exit` reports a child's exit without reaping it
  (`waitid` WNOWAIT|WNOHANG); `own_process_group`, `own_parent_pid` and
  `effective_uid` answer identity questions; `pid_alive` probes a single
  pid.
- `dir`: one-component directory-descriptor steps for walking a tree
  without following links (`open_dir_at_nofollow`, `create_dir_at`,
  `stat_at_nofollow`, `read_dir_names`, `create_new_rw_file_at`,
  `sync_dir`).
- `supervisor`: init-process primitives for the sandbox supervisor
  (ADR 0039): `prctl` non-dumpable and child-subreaper (Linux),
  `spawn_in_new_session` (`setsid` + `fchdir` before exec),
  `kill_all_processes` (`kill(-1)`, refused unless PID 1), `reap_any` /
  `reap_child` (`waitpid` WNOHANG), `descendants` (`/proc`, Linux) and
  `sync_filesystems`.
- `private_fs`: `create_private_dir` (0700) and `create_private_file` (0600)
  for state directories holding secret material.

Non-Unix platforms compile and refuse explicitly
(`SysError::Unsupported` / `io::ErrorKind::Unsupported`).
