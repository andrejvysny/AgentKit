//! Platform primitives (ADR 0021 R9): the only crate allowed to hold
//! process-lifetime OS access and the `unsafe` that comes with it.
//!
//! Driven adapters (transport, LSP server lifecycle, private state files)
//! call these functions instead of linking `libc` themselves. Unix
//! implements the primitives; every other platform compiles and refuses
//! explicitly (`SysError::Unsupported` / [`std::io::ErrorKind::Unsupported`]).

pub mod private_fs;
pub mod process;

pub use private_fs::{create_private_dir, create_private_file};
pub use process::{
    ProcessGroup, Signal, SignalOutcome, SysError, own_process_group, pid_alive,
    spawn_in_new_group, spawn_in_new_group_tokio,
};
