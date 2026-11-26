//! Reads: `read_file`, `list`, `status`, `read_for_export`. They take no
//! lease and write no row (spec "Reads"); they may run beside a command
//! and see a half-written file. A never-used binding creates nothing: a
//! read answers not found, a listing is empty and the status says the
//! workspace does not exist yet (F-16). A fenced workspace answers
//! `sandbox_fenced` to every read that needs its machine; its status still
//! answers, with the fenced state in it.

use crate::error::SandboxError;
use crate::ids::{AgentId, Sha256};
use crate::limits::{
    EXPORT_FILE_MAX_BYTES, LIST_DEPTH_MAX, LIST_DEPTH_MIN, LIST_MAX_ENTRIES, PENDING_REQUESTS_MAX,
    READ_FILE_MAX_BYTES,
};
use crate::machine::{EntryKind, FileChunk, ListEntries, Listing, MachineHandle, ReadFile};
use crate::path::RelPath;
use crate::workspace::{Workspace, WorkspaceState};

use super::WorkspaceService;
use super::paths;
use super::prepare::state_error;
use super::types::{Binding, ExportFile, FileRef, RunningOperation, WorkspaceStatus};

/// Bytes one export read asks for.
const EXPORT_CHUNK_BYTES: u64 = 8 * 1024 * 1024;

impl WorkspaceService {
    /// Read up to `limit_bytes` (at most the model output's byte cap) from
    /// `offset` of a file of at most `READ_FILE_MAX_BYTES`. A binary file
    /// answers its size and type, no bytes.
    pub async fn read_file(
        &self,
        binding: &Binding,
        agent: &AgentId,
        file: &FileRef,
        offset: u64,
        limit_bytes: u64,
    ) -> Result<FileChunk, SandboxError> {
        let target = paths::resolve(binding.folder.as_ref(), file)?;
        let Some((workspace, handle)) = self.read_machine(binding, agent).await? else {
            return Err(SandboxError::NotFound {
                what: paths::display(&target),
            });
        };
        let request = ReadFile {
            root: target.root,
            path: target.path,
            offset,
            limit_bytes: limit_bytes.min(self.config.model_output.max_bytes),
            max_file_bytes: READ_FILE_MAX_BYTES,
            raw: false,
        };
        let chunk = self.read_call(&handle, &request).await?;
        self.touched(&workspace).await;
        Ok(chunk)
    }

    /// A list call whose answer is cut to the request, whatever the backend
    /// returned.
    async fn list_call(
        &self,
        handle: &MachineHandle,
        request: &ListEntries,
    ) -> Result<Listing, SandboxError> {
        let call = self.backend.list(handle, request);
        let mut listing = self.bounded("list", call).await?;
        let max = usize::try_from(request.max_entries).unwrap_or(usize::MAX);
        if listing.entries.len() > max {
            listing.entries.truncate(max);
            listing.truncated = true;
        }
        Ok(listing)
    }

    /// List a directory `depth` levels down (clamped to 1..=4), at most
    /// `LIST_MAX_ENTRIES` entries, links never followed. Entry paths are
    /// relative to the root, the binding's folder included.
    pub async fn list(
        &self,
        binding: &Binding,
        agent: &AgentId,
        dir: &FileRef,
        depth: u8,
    ) -> Result<Listing, SandboxError> {
        let target = paths::resolve(binding.folder.as_ref(), dir)?;
        let Some((workspace, handle)) = self.read_machine(binding, agent).await? else {
            return Ok(Listing::default());
        };
        let request = ListEntries {
            root: target.root,
            path: target.path,
            depth: depth.clamp(LIST_DEPTH_MIN, LIST_DEPTH_MAX),
            max_entries: LIST_MAX_ENTRIES,
        };
        let listing = self.list_call(&handle, &request).await?;
        self.touched(&workspace).await;
        Ok(listing)
    }

    /// What `sandbox_get_status` shows. Needs no machine and starts none;
    /// a never-used binding answers `workspace: None` and creates nothing.
    pub async fn status(
        &self,
        binding: &Binding,
        agent: &AgentId,
    ) -> Result<WorkspaceStatus, SandboxError> {
        let folder = FileRef::workspace(binding.folder.clone().unwrap_or_else(RelPath::root));
        let working_dir = paths::display(&folder);
        let Some(workspace) = self.lookup(binding).await? else {
            return Ok(WorkspaceStatus {
                workspace: None,
                working_dir,
                storage: None,
                grants: Vec::new(),
                pending_requests: Vec::new(),
                running: None,
            });
        };
        self.check_member(&workspace, agent).await?;
        let call = self.backend.storage_usage(&workspace.id);
        let storage = self.bounded("storage_usage", call).await.ok();
        let running = match self.open_operation(&workspace.id).await? {
            Some(operation) => Some(RunningOperation {
                holder: self
                    .repo
                    .lease(&workspace.id, self.now())
                    .await?
                    .map(|lease| lease.holder),
                operation,
            }),
            None => None,
        };
        let mut pending_requests = self.repo.list_pending_requests(&workspace.id).await?;
        pending_requests.truncate(PENDING_REQUESTS_MAX);
        Ok(WorkspaceStatus {
            grants: self.repo.list_active_grants(&workspace.id).await?,
            pending_requests,
            workspace: Some(workspace),
            working_dir,
            storage,
            running,
        })
    }

    /// Read a whole regular file for an export: at most
    /// `EXPORT_FILE_MAX_BYTES`, binary or not, with its digest. A link, a
    /// directory or anything else is `not_a_regular_file`; a file that
    /// changes while it is read is refused.
    pub async fn read_for_export(
        &self,
        binding: &Binding,
        agent: &AgentId,
        file: &FileRef,
    ) -> Result<ExportFile, SandboxError> {
        let target = paths::resolve(binding.folder.as_ref(), file)?;
        let shown = paths::display(&target);
        let Some((workspace, handle)) = self.read_machine(binding, agent).await? else {
            return Err(SandboxError::NotFound { what: shown });
        };
        let size = self.regular_file_size(&handle, &target).await?;
        if size > EXPORT_FILE_MAX_BYTES {
            return Err(SandboxError::FileTooLarge {
                size,
                limit: EXPORT_FILE_MAX_BYTES,
            });
        }
        let export = self.read_whole(&handle, &target).await?;
        self.touched(&workspace).await;
        Ok(export)
    }

    /// The size of `target` when the listing of its parent shows a regular
    /// file there. The listing is bounded like any other; when it is cut
    /// before the file, the file's kind cannot be checked and the export
    /// is refused rather than answered as missing.
    async fn regular_file_size(
        &self,
        handle: &MachineHandle,
        target: &FileRef,
    ) -> Result<u64, SandboxError> {
        let shown = paths::display(target);
        let not_regular = || SandboxError::NotARegularFile {
            path: shown.clone(),
        };
        let (parent, _) = paths::split_parent(&target.path).ok_or_else(not_regular)?;
        let request = ListEntries {
            root: target.root,
            path: parent,
            depth: LIST_DEPTH_MIN,
            max_entries: LIST_MAX_ENTRIES,
        };
        let listing = self.list_call(handle, &request).await?;
        let truncated = listing.truncated;
        let Some(entry) = listing
            .entries
            .into_iter()
            .find(|entry| entry.path == target.path)
        else {
            if truncated {
                return Err(SandboxError::Unavailable {
                    reason: format!(
                        "the folder of {shown} has more than {LIST_MAX_ENTRIES} entries, so the export cannot check that it is a regular file"
                    ),
                });
            }
            return Err(SandboxError::NotFound { what: shown });
        };
        match (entry.kind, entry.size) {
            (EntryKind::File, Some(size)) => Ok(size),
            _ => Err(not_regular()),
        }
    }

    /// Raw chunks until the end; every chunk must describe the same file.
    async fn read_whole(
        &self,
        handle: &MachineHandle,
        target: &FileRef,
    ) -> Result<ExportFile, SandboxError> {
        let mut bytes = Vec::new();
        let mut first: Option<(u64, i64)> = None;
        loop {
            let request = ReadFile {
                root: target.root,
                path: target.path.clone(),
                offset: bytes.len() as u64,
                limit_bytes: EXPORT_CHUNK_BYTES,
                max_file_bytes: EXPORT_FILE_MAX_BYTES,
                raw: true,
            };
            let chunk = self.read_call(handle, &request).await?;
            let seen = (chunk.size, chunk.mtime_ms);
            if first.is_some_and(|first| first != seen) || bytes.len() as u64 > chunk.size {
                return Err(SandboxError::Backend {
                    reason: format!("{} changed while it was read", paths::display(target)),
                });
            }
            first = Some(seen);
            bytes.extend_from_slice(&chunk.bytes);
            if chunk.eof || chunk.bytes.is_empty() {
                break;
            }
        }
        let (size, mtime_ms) = first.unwrap_or_default();
        if bytes.len() as u64 != size {
            return Err(SandboxError::Backend {
                reason: format!("{} changed while it was read", paths::display(target)),
            });
        }
        Ok(ExportFile {
            sha256: Sha256::of(&bytes),
            bytes,
            size,
            mtime_ms,
        })
    }

    /// A read whose bytes are cut to the request, whatever the backend
    /// returned.
    async fn read_call(
        &self,
        handle: &MachineHandle,
        request: &ReadFile,
    ) -> Result<FileChunk, SandboxError> {
        let call = self.backend.read_file(handle, request);
        let mut chunk = self.bounded("read_file", call).await?;
        let max = usize::try_from(request.limit_bytes).unwrap_or(usize::MAX);
        if chunk.bytes.len() > max {
            chunk.bytes.truncate(max);
            chunk.eof = false;
        }
        Ok(chunk)
    }

    /// The machine a read uses, without a lease. `None` for a never-used
    /// binding. A fenced workspace answers `Fenced`. When the current
    /// generation needs a proven stop and an advance first, the read takes
    /// the lease for that step only if it is free, else answers `Busy`.
    ///
    /// The record is read again after the start: when a stop advanced the
    /// generation meanwhile, the machine just started belongs to a
    /// generation left behind, so it is forgotten and stopped, and the
    /// read starts over once.
    async fn read_machine(
        &self,
        binding: &Binding,
        agent: &AgentId,
    ) -> Result<Option<(Workspace, MachineHandle)>, SandboxError> {
        let _admitted = self.gate.admit()?;
        for _ in 0..2 {
            let Some(workspace) = self.lookup(binding).await? else {
                return Ok(None);
            };
            self.check_member(&workspace, agent).await?;
            if let WorkspaceState::Fenced { .. } | WorkspaceState::Deleted = workspace.state {
                return Err(state_error(&workspace.state, &workspace.id));
            }
            match self.ensure(&workspace).await {
                Ok(handle) => {
                    if self.load(&workspace.id).await?.generation == handle.generation {
                        return Ok(Some((workspace, handle)));
                    }
                    self.machines.forget_handle(&handle);
                    let _ = self
                        .stop_proof(&handle.workspace_id, handle.generation)
                        .await;
                }
                Err(SandboxError::StaleGeneration { .. }) => {
                    return self.read_restart(&workspace).await.map(Some);
                }
                Err(error) => return Err(error),
            }
        }
        Err(SandboxError::Unavailable {
            reason: "the sandbox machine is restarting".into(),
        })
    }

    /// The current generation needs a proven stop and an advance: do it
    /// under the lease if nobody holds it.
    async fn read_restart(
        &self,
        workspace: &Workspace,
    ) -> Result<(Workspace, MachineHandle), SandboxError> {
        let holder = self.service_holder("read restart");
        let Some(lease) = self.try_acquire(&workspace.id, &holder).await? else {
            let holder = self.repo.lease(&workspace.id, self.now()).await?;
            return Err(match holder {
                Some(info) => SandboxError::Busy {
                    holder: info.holder,
                },
                None => SandboxError::Unavailable {
                    reason: "the sandbox machine is restarting".into(),
                },
            });
        };
        let prepared = self.prepare(&lease).await;
        self.release(&lease).await;
        prepared
    }

    async fn touched(&self, workspace: &Workspace) {
        let now = self.now();
        self.machines.touch(&workspace.id, now);
        // `last_used_at` only orders the activity list; a failed touch
        // changes nothing a writer relies on.
        let _ = self.repo.touch_workspace(&workspace.id, now).await;
    }
}
