//! Platform primitives (ADR 0021 R9): the only crate allowed to hold
//! process-lifetime OS access and the `unsafe` that comes with it.
//!
//! Driven adapters (transport, LSP server lifecycle, private state files,
//! descriptor-anchored file tools) call these functions instead of linking
//! `libc` themselves. Unix
//! implements the primitives; every other platform compiles and refuses
//! explicitly (`SysError::Unsupported` / [`std::io::ErrorKind::Unsupported`]).

pub mod dir;
pub mod fs;
pub mod private_fs;
pub mod process;
pub mod procinfo;
pub mod supervisor;

pub use dir::{
    DirNames, EntryStat, EntryType, create_dir_at, create_new_rw_file_at, open_dir_at_nofollow,
    read_dir_names, stat_at_nofollow, sync_dir,
};
pub use fs::{
    create_new_file_at, dir_real_path, link_at, lock_exclusive, open_dir_nofollow,
    open_file_at_nofollow, open_parent_dir, rename_at, try_lock_exclusive, unlink_at,
    unlock_exclusive,
};
pub use private_fs::{create_private_dir, create_private_file};
pub use process::{
    ChildExit, KillProbe, ProcessGroup, Signal, SignalOutcome, SysError, effective_uid, kill_probe,
    own_parent_pid, own_process_group, peek_child_exit, pid_alive, real_uid, signal_process,
    spawn_in_new_group, spawn_in_new_group_tokio,
};
pub use procinfo::{ProcessLookup, lookup_process, pids_holding};
pub use supervisor::{
    Reaped, descendants, is_init, kill_all_processes, raise_open_file_limit, reap_any, reap_child,
    set_child_subreaper, set_non_dumpable, spawn_in_new_session, sync_filesystems,
};
