//! Append-only JSONL worker journal with tamper/tearing-detectable digest
//! chaining (§5.4, §7 crash points).
//!
//! Each line is one compact JSON [`JournalLine`]; every line embeds the
//! SHA-256 of the previous line, so both tearing (crash mid-write) and
//! tampering (edited history) are detectable at replay. `append` fsyncs at
//! every effect boundary; `StageChunk` records are batched (they are
//! progress markers, not effects). Replay truncates a torn tail on the last
//! line only; any bad line that is not at EOF is fatal
//! (`WorkerError::JournalCorrupt`).

use crate::error::{WorkerError, WorkerResult};
use agentkit_sandbox_contracts::api::StageDirection;
use agentkit_sandbox_contracts::api::{ProcessIdentity, TerminationEvidence};
use agentkit_sandbox_contracts::ids::{
    ArtifactDigest, ExecutionId, InstanceGeneration, RequestDigest, SandboxId, StageId, TenantId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Monotonic journal sequence (`Seq`).
pub type Seq = u64;

/// One durable journal record kind (§5.4 minimum record set + staging +
/// reservations).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JournalRecord {
    VmCreated {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        nonce_digest: ArtifactDigest,
        vsock_cid: u32,
        disk_path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        disk_digest: Option<ArtifactDigest>,
        /// Full launch identity so replay rebuilds a truthful registry
        /// without re-deriving from the wire.
        image_digest: agentkit_sandbox_contracts::ids::ImageDigest,
        rootfs_digest: ArtifactDigest,
        kernel_digest: agentkit_sandbox_contracts::ids::ImageDigest,
        policy_digest: agentkit_sandbox_contracts::ids::PolicyDigest,
        policy_generation: agentkit_sandbox_contracts::ids::PolicyGeneration,
        allow_guest_root: bool,
        persistent: bool,
    },
    VmLaunched {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        #[serde(skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        identity: Option<ProcessIdentity>,
    },
    VmStopped {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        evidence: TerminationEvidence,
    },
    VmDestroyed {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        evidence: TerminationEvidence,
    },
    DiskCreated {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        disk_path: String,
        disk_digest: ArtifactDigest,
    },
    DiskDeleted {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        disk_path: String,
    },
    /// Single-writer epoch admission (S09): durable so a worker restart
    /// cannot resurrect a superseded writer's grants.
    WriterAcquired {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        epoch: u64,
        holder_task: agentkit_sandbox_contracts::ids::TaskId,
        expires_at_ms: u64,
    },
    ExecAdmitted {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        execution: ExecutionId,
        grant_digest: RequestDigest,
    },
    ExecDispatched {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        execution: ExecutionId,
    },
    ExecAcked {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        execution: ExecutionId,
    },
    ExecSpawnedReported {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        execution: ExecutionId,
        guest_pid: u32,
    },
    ExecExitedReported {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        execution: ExecutionId,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
        timed_out: bool,
    },
    ExecTerminationObserved {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        execution: ExecutionId,
        evidence: String,
    },
    ExecCleanupVerified {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        execution: ExecutionId,
    },
    ExecUncertain {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        execution: ExecutionId,
        reason: String,
    },
    ExecQuarantined {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        execution: ExecutionId,
        reason: String,
    },
    StageBegin {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        stage_id: StageId,
        direction: StageDirection,
        total_bytes: u64,
        manifest_digest: ArtifactDigest,
        /// Manifest file list (import/export truth for replay).
        files: Vec<String>,
    },
    StageChunk {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        stage_id: StageId,
        sequence: u32,
    },
    StageCommit {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        stage_id: StageId,
        manifest_digest: ArtifactDigest,
    },
    StageAbort {
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        stage_id: StageId,
    },
    ReservationAcquire {
        tenant: TenantId,
        #[serde(skip_serializing_if = "Option::is_none")]
        sandbox: Option<SandboxId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        generation: Option<InstanceGeneration>,
        vcpus: u32,
        memory_mib: u32,
        disk_mib: u64,
    },
    ReservationRelease {
        tenant: TenantId,
        #[serde(skip_serializing_if = "Option::is_none")]
        sandbox: Option<SandboxId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        generation: Option<InstanceGeneration>,
        vcpus: u32,
        memory_mib: u32,
        disk_mib: u64,
    },
}

impl JournalRecord {
    /// Records whose durability defines an effect boundary: these fsync.
    /// `StageChunk` is progress, not an effect, and is batched.
    #[must_use]
    pub fn is_effect_boundary(&self) -> bool {
        !matches!(self, Self::StageChunk { .. })
    }

    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        match self {
            Self::VmCreated { tenant, .. }
            | Self::VmLaunched { tenant, .. }
            | Self::VmStopped { tenant, .. }
            | Self::VmDestroyed { tenant, .. }
            | Self::DiskCreated { tenant, .. }
            | Self::DiskDeleted { tenant, .. }
            | Self::WriterAcquired { tenant, .. }
            | Self::ExecAdmitted { tenant, .. }
            | Self::ExecDispatched { tenant, .. }
            | Self::ExecAcked { tenant, .. }
            | Self::ExecSpawnedReported { tenant, .. }
            | Self::ExecExitedReported { tenant, .. }
            | Self::ExecTerminationObserved { tenant, .. }
            | Self::ExecCleanupVerified { tenant, .. }
            | Self::ExecUncertain { tenant, .. }
            | Self::ExecQuarantined { tenant, .. }
            | Self::StageBegin { tenant, .. }
            | Self::StageChunk { tenant, .. }
            | Self::StageCommit { tenant, .. }
            | Self::StageAbort { tenant, .. }
            | Self::ReservationAcquire { tenant, .. }
            | Self::ReservationRelease { tenant, .. } => tenant,
        }
    }
}

/// One journal line: sequence, wall clock, previous-line digest, record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalLine {
    pub seq: Seq,
    pub ts_ms: u64,
    /// SHA-256 of the previous *raw line* (absent only on the first line).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    pub record: JournalRecord,
}

impl JournalLine {
    /// Digest string of this line's serialized bytes (`sha256:<hex>`).
    fn line_digest(raw: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(raw);
        format!("sha256:{}", hex(&h.finalize()))
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Durable journal handle over `<storage-root>/journal/worker-<worker_id>.jsonl`.
pub struct Journal {
    path: PathBuf,
    file: File,
    next_seq: Seq,
    last_line_digest: Option<String>,
    /// Bytes written but not yet fsynced (batched StageChunk records).
    unflushed: u64,
}

impl Journal {
    /// Open (or create) the journal in `dir`, replaying to position the tail.
    /// A torn tail is truncated here (only the last line may be torn);
    /// corruption earlier fails closed.
    pub fn open(dir: &Path, worker_id: &str) -> WorkerResult<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| WorkerError::Storage(format!("journal dir: {e}")))?;
        let safe_id: String = worker_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = dir.join(format!("worker-{safe_id}.jsonl"));
        // Validate the whole chain and truncate a torn tail if present.
        Self::replay_file(&path)?;
        // Position the tail from the exact surviving bytes (never from
        // re-serialization: the chain digest is over raw line bytes).
        let (next_seq, last_digest) = Self::tail_state(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| WorkerError::Storage(format!("journal open: {e}")))?;
        Ok(Self {
            path,
            file,
            next_seq,
            last_line_digest: last_digest,
            unflushed: 0,
        })
    }

    /// Recompute (next_seq, prev-line digest) from the exact bytes that
    /// survived torn-tail truncation.
    fn tail_state(path: &Path) -> WorkerResult<(Seq, Option<String>)> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return Ok((1, None)),
        };
        let lines: Vec<&[u8]> = split_lines_keep_shape(&bytes);
        let terminated = bytes.last().is_none_or(|b| *b == b'\n');
        let kept = valid_prefix_len(&lines, terminated)?;
        if kept == 0 {
            return Ok((1, None));
        }
        let raw = lines[kept - 1];
        let line: JournalLine =
            serde_json::from_slice(raw).map_err(|e| WorkerError::JournalCorrupt(e.to_string()))?;
        Ok((line.seq + 1, Some(JournalLine::line_digest(raw))))
    }

    /// Append one record; fsyncs unless it is a batched `StageChunk`.
    pub fn append(&mut self, record: &JournalRecord, ts_ms: u64) -> WorkerResult<Seq> {
        let line = JournalLine {
            seq: self.next_seq,
            ts_ms,
            prev: self.last_line_digest.clone(),
            record: record.clone(),
        };
        let mut raw = serde_json::to_vec(&line)
            .map_err(|e| WorkerError::Internal(format!("journal serialize: {e}")))?;
        raw.push(b'\n');
        self.file
            .write_all(&raw)
            .map_err(|e| WorkerError::Storage(format!("journal write: {e}")))?;
        if record.is_effect_boundary() {
            self.file
                .sync_all()
                .map_err(|e| WorkerError::Storage(format!("journal fsync: {e}")))?;
            self.unflushed = 0;
        } else {
            self.unflushed += 1;
        }
        self.last_line_digest = Some(JournalLine::line_digest(&raw[..raw.len() - 1]));
        let seq = self.next_seq;
        self.next_seq += 1;
        Ok(seq)
    }

    /// Force fsync of batched records (stage commit / shutdown drain).
    pub fn sync(&mut self) -> WorkerResult<()> {
        self.file
            .sync_all()
            .map_err(|e| WorkerError::Storage(format!("journal sync: {e}")))?;
        self.unflushed = 0;
        Ok(())
    }

    /// Rebuild the durable history: verified lines only. A torn *last* line
    /// (no trailing newline / unparsable tail) is truncated from the file;
    /// any other bad line or digest-chain break is fatal.
    pub fn replay(path: &Path) -> WorkerResult<Vec<JournalRecord>> {
        Ok(Self::replay_file(path)?
            .into_iter()
            .map(|l| l.record)
            .collect())
    }

    fn replay_file(path: &Path) -> WorkerResult<Vec<JournalLine>> {
        let mut file = match OpenOptions::new().read(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(WorkerError::Storage(format!("journal read: {e}"))),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| WorkerError::Storage(format!("journal read: {e}")))?;
        let lines = split_lines_keep_shape(&bytes);
        let terminated = bytes.last().is_none_or(|b| *b == b'\n');
        let kept = valid_prefix_len(&lines, terminated)?;
        if kept != lines.len() {
            // Torn tail: truncate the file to the last good line boundary.
            let good_bytes: usize = lines[..kept].iter().map(|l| l.len() + 1).sum();
            let f = OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|e| WorkerError::Storage(format!("journal truncate: {e}")))?;
            f.set_len(good_bytes as u64)
                .map_err(|e| WorkerError::Storage(format!("journal truncate: {e}")))?;
            f.sync_all()
                .map_err(|e| WorkerError::Storage(format!("journal truncate fsync: {e}")))?;
        }
        let mut out = Vec::with_capacity(kept);
        let mut prev_digest: Option<String> = None;
        let mut prev_seq: Option<Seq> = None;
        for raw in &lines[..kept] {
            let line: JournalLine = serde_json::from_slice(raw)
                .map_err(|e| WorkerError::JournalCorrupt(format!("bad line: {e}")))?;
            if line.prev != prev_digest {
                return Err(WorkerError::JournalCorrupt(format!(
                    "digest chain break at seq {}",
                    line.seq
                )));
            }
            if let Some(p) = prev_seq {
                if line.seq != p + 1 {
                    return Err(WorkerError::JournalCorrupt(format!(
                        "sequence break at seq {} (expected {})",
                        line.seq,
                        p + 1
                    )));
                }
            } else if line.seq != 1 {
                return Err(WorkerError::JournalCorrupt(format!(
                    "journal must start at seq 1, found {}",
                    line.seq
                )));
            }
            prev_digest = Some(JournalLine::line_digest(raw));
            prev_seq = Some(line.seq);
            out.push(line);
        }
        Ok(out)
    }

    #[must_use]
    pub fn next_seq(&self) -> Seq {
        self.next_seq
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// fsync the directory entry too (new-file durability).
    pub fn sync_dir(&self) -> WorkerResult<()> {
        if let Some(dir) = self.path.parent() {
            let d = File::open(dir).map_err(|e| WorkerError::Storage(format!("dir fsync: {e}")))?;
            d.sync_all()
                .map_err(|e| WorkerError::Storage(format!("dir fsync: {e}")))?;
        }
        Ok(())
    }
}

/// Split into complete lines plus a trailing partial fragment (`None` when
/// the file ends with a newline). The returned slices exclude newlines.
fn split_lines_keep_shape(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            out.push(&bytes[start..i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}

/// Number of leading lines that parse AND chain correctly. A bad line is
/// allowed only as the final fragment (torn tail); a bad line that is
/// followed by more bytes, or any chain break, is fatal corruption.
fn valid_prefix_len(lines: &[&[u8]], last_line_terminated: bool) -> WorkerResult<usize> {
    let mut prev_digest: Option<String> = None;
    let mut prev_seq: Option<Seq> = None;
    for (idx, raw) in lines.iter().enumerate() {
        let is_last = idx + 1 == lines.len();
        let parsed: Option<JournalLine> = serde_json::from_slice(raw).ok();
        let Some(line) = parsed else {
            if is_last && !last_line_terminated {
                // Torn tail candidate: an unterminated final fragment may be
                // a partial write. A terminated-but-unparsable line is a
                // whole-record corruption anywhere in the file: fatal.
                return Ok(idx);
            }
            return Err(WorkerError::JournalCorrupt(format!(
                "unparsable line at index {idx} not at EOF"
            )));
        };
        if line.prev != prev_digest {
            return Err(WorkerError::JournalCorrupt(format!(
                "digest chain break at seq {}",
                line.seq
            )));
        }
        if let Some(p) = prev_seq {
            if line.seq != p + 1 {
                return Err(WorkerError::JournalCorrupt(format!(
                    "sequence break at seq {}",
                    line.seq
                )));
            }
        } else if line.seq != 1 {
            return Err(WorkerError::JournalCorrupt(
                "journal does not start at seq 1".into(),
            ));
        }
        prev_digest = Some(JournalLine::line_digest(raw));
        prev_seq = Some(line.seq);
    }
    Ok(lines.len())
}

/// Seek helper kept for API symmetry with tests that inspect the raw file.
pub fn raw_bytes(path: &Path) -> WorkerResult<Vec<u8>> {
    let mut f = File::open(path).map_err(|e| WorkerError::Storage(format!("raw read: {e}")))?;
    let mut v = Vec::new();
    f.seek(SeekFrom::Start(0))
        .map_err(|e| WorkerError::Storage(format!("raw seek: {e}")))?;
    f.read_to_end(&mut v)
        .map_err(|e| WorkerError::Storage(format!("raw read: {e}")))?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_sandbox_contracts::ids::{InstanceGeneration, SandboxId, TenantId};

    fn rec(s: &str) -> JournalRecord {
        JournalRecord::VmCreated {
            tenant: TenantId::new("t1").unwrap(),
            sandbox: SandboxId::new(s).unwrap(),
            generation: InstanceGeneration(1),
            nonce_digest: ArtifactDigest::of_bytes(b"n"),
            vsock_cid: 3,
            disk_path: format!("/var/lib/ak/{s}/disk.raw"),
            disk_digest: None,
            image_digest: agentkit_sandbox_contracts::ids::ImageDigest::of_bytes(b"i"),
            rootfs_digest: ArtifactDigest::of_bytes(b"r"),
            kernel_digest: agentkit_sandbox_contracts::ids::ImageDigest::of_bytes(b"k"),
            policy_digest: agentkit_sandbox_contracts::ids::PolicyDigest::of_bytes(b"p"),
            policy_generation: agentkit_sandbox_contracts::ids::PolicyGeneration(1),
            allow_guest_root: false,
            persistent: false,
        }
    }

    #[test]
    fn append_and_replay_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("journal");
        {
            let mut j = Journal::open(&sub, "w1").unwrap();
            for i in 0..5 {
                j.append(&rec(&format!("sb{i}")), 1000 + i).unwrap();
            }
        }
        let recs = Journal::replay(&sub.join("worker-w1.jsonl")).unwrap();
        assert_eq!(recs.len(), 5);
    }

    #[test]
    fn torn_tail_is_truncated_but_midfile_corruption_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("journal");
        {
            let mut j = Journal::open(&sub, "w1").unwrap();
            j.append(&rec("sb1"), 1).unwrap();
            j.append(&rec("sb2"), 2).unwrap();
        }
        let path = sub.join("worker-w1.jsonl");
        // Torn tail: partial third line.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"TORN_TAIL_MARKER\"").unwrap();
        drop(f);
        let recs = Journal::replay(&path).unwrap();
        assert_eq!(recs.len(), 2);
        let tail = raw_bytes(&path).unwrap();
        assert!(!String::from_utf8_lossy(&tail).contains("TORN_TAIL_MARKER"));

        // Mid-file corruption: break the second line's bytes.
        let mut bytes = raw_bytes(&path).unwrap();
        let first_nl = bytes.iter().position(|b| *b == b'\n').unwrap();
        bytes[first_nl + 5] = b'!';
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Journal::replay(&path),
            Err(WorkerError::JournalCorrupt(_))
        ));
    }

    #[test]
    fn tampered_line_breaks_chain() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("journal");
        {
            let mut j = Journal::open(&sub, "w1").unwrap();
            j.append(&rec("sb1"), 1).unwrap();
            j.append(&rec("sb2"), 2).unwrap();
            j.append(&rec("sb3"), 3).unwrap();
        }
        let path = sub.join("worker-w1.jsonl");
        let mut bytes = raw_bytes(&path).unwrap();
        let s = String::from_utf8(bytes.clone()).unwrap();
        let edited = s.replace("sb2", "sbX");
        assert_ne!(s, edited);
        bytes = edited.into_bytes();
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Journal::replay(&path),
            Err(WorkerError::JournalCorrupt(_))
        ));
    }
}
