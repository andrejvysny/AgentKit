//! Platform primitives (ADR 0021 R9): the only crate allowed to hold
//! process-lifetime OS access and the `unsafe` that comes with it.
//!
//! Driven adapters (transport, LSP server lifecycle, private state files,
//! descriptor-anchored file tools) call these functions instead of linking
//! `libc` themselves. Unix
//! implements the primitives; every other platform compiles and refuses
//! explicitly (`SysError::Unsupported` / [`std::io::ErrorKind::Unsupported`]).

pub mod fs;
pub mod private_fs;
pub mod process;

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
