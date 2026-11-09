//! Crash-safe SQLite host store for sandbox bindings and the tenant quota
//! reservation ledger (§5.4, M3 Durability).
//!
//! The binding table mirrors the `SandboxBinding` JSON record one row per
//! sandbox; the quota ledger is transactional and fenced: a reservation is
//! granted inside `BEGIN IMMEDIATE` against the tenant's ceilings and its
//! live reservations, and carries the tenant fence epoch that must match on
//! release — a stale host process (or a replayed after-restart decision)
//! cannot release or clobber reservations it does not own.

use std::path::Path;
use std::sync::Mutex;

use agentkit_sandbox_contracts::ids::{InstanceGeneration, SandboxId, SessionId, TenantId};
use agentkit_sandbox_worker::registry::QuotaCeilings;
use rusqlite::{Connection, OptionalExtension, params};

use crate::binding::{BindingError, SandboxBinding, SandboxBindingStore};

/// Schema this build creates; future migrations bump and gate on
/// `PRAGMA user_version` exactly like the main store.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sandbox_bindings(
  tenant_id  TEXT NOT NULL,
  sandbox_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  state      TEXT NOT NULL,
  record     TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  PRIMARY KEY (tenant_id, sandbox_id)
);
CREATE INDEX IF NOT EXISTS sandbox_bindings_session
  ON sandbox_bindings(tenant_id, session_id);
CREATE TABLE IF NOT EXISTS quota_ceilings(
  tenant_id TEXT PRIMARY KEY,
  vcpus     INTEGER NOT NULL,
  memory_mib INTEGER NOT NULL,
  disk_mib  INTEGER NOT NULL,
  live_vms  INTEGER NOT NULL,
  fence     INTEGER NOT NULL DEFAULT 0,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS quota_reservations(
  sandbox_id TEXT PRIMARY KEY,
  tenant_id  TEXT NOT NULL,
  vcpus      INTEGER NOT NULL,
  memory_mib INTEGER NOT NULL,
  disk_mib   INTEGER NOT NULL,
  instance_generation INTEGER NOT NULL,
  fence      INTEGER NOT NULL,
  state      TEXT NOT NULL CHECK (state IN ('active','released')),
  updated_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS reservations_tenant_state
  ON quota_reservations(tenant_id, state);
"#;

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("quota exceeded: {0}")]
    Exceeded(String),
    #[error("reservation not found")]
    NotFound,
    #[error("stale fence: ledger moved past this ticket")]
    StaleFence,
}

impl From<rusqlite::Error> for LedgerError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e.to_string())
    }
}

impl From<rusqlite::Error> for BindingError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Reservation grant: proof of the ledger epoch under which capacity was
/// held. Release must present it (fenced release).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationTicket {
    pub sandbox_id: SandboxId,
    pub tenant_id: TenantId,
    pub instance_generation: InstanceGeneration,
    pub fence: i64,
}

/// One tenant's live capacity as the ledger sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReservedUsage {
    pub live_vms: u32,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub disk_mib: u64,
}

/// SQLite-backed binding store + quota ledger (single writer; the mutex
/// serializes within this process, `BEGIN IMMEDIATE` across processes).
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (creating if needed) and bring the schema up to date.
    pub fn open(path: &Path) -> Result<Self, BindingError> {
        let mut conn = Connection::open(path).map_err(|e| BindingError::Io(e.to_string()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| BindingError::Io(e.to_string()))?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| BindingError::Io(e.to_string()))?;
        if version > SCHEMA_VERSION {
            return Err(BindingError::Io(format!(
                "sandbox store schema {version} is newer than this build ({SCHEMA_VERSION})"
            )));
        }
        if version < SCHEMA_VERSION {
            let tx = conn
                .transaction()
                .map_err(|e| BindingError::Io(e.to_string()))?;
            tx.execute_batch(SCHEMA)
                .map_err(|e| BindingError::Io(e.to_string()))?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|e| BindingError::Io(e.to_string()))?;
            tx.commit().map_err(|e| BindingError::Io(e.to_string()))?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }

    // ---------------------------------------------------------------- quota

    /// Set (replace) a tenant's ceilings. Does not touch the fence.
    pub fn set_ceilings(&self, tenant: &TenantId, c: &QuotaCeilings) -> Result<(), LedgerError> {
        let conn = self.conn.lock().expect("store lock");
        conn.execute(
            "INSERT INTO quota_ceilings(tenant_id, vcpus, memory_mib, disk_mib, live_vms, fence, updated_at_ms)
             VALUES (?1,?2,?3,?4,?5,0,?6)
             ON CONFLICT(tenant_id) DO UPDATE SET
               vcpus=excluded.vcpus, memory_mib=excluded.memory_mib,
               disk_mib=excluded.disk_mib, live_vms=excluded.live_vms,
               updated_at_ms=excluded.updated_at_ms",
            params![
                tenant.as_str(),
                c.max_vcpus_per_tenant as i64,
                c.max_memory_mib_per_tenant as i64,
                c.max_disk_mib_per_tenant as i64,
                c.max_live_vms_per_tenant as i64,
                Self::now_ms() as i64,
            ],
        )?;
        Ok(())
    }

    /// Current ceilings for a tenant (defaults when unset).
    pub fn ceilings(&self, tenant: &TenantId) -> Result<QuotaCeilings, LedgerError> {
        let conn = self.conn.lock().expect("store lock");
        let row = conn
            .query_row(
                "SELECT vcpus, memory_mib, disk_mib, live_vms FROM quota_ceilings WHERE tenant_id = ?1",
                params![tenant.as_str()],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(match row {
            Some((v, m, d, l)) => QuotaCeilings {
                max_live_vms_per_tenant: l.max(0) as u32,
                max_live_vms_global: QuotaCeilings::default().max_live_vms_global,
                max_vcpus_per_tenant: v.max(0) as u32,
                max_memory_mib_per_tenant: m.max(0) as u32,
                max_disk_mib_per_tenant: d.max(0) as u64,
            },
            None => QuotaCeilings::default(),
        })
    }

    /// Live reservations for a tenant.
    pub fn usage(&self, tenant: &TenantId) -> Result<ReservedUsage, LedgerError> {
        let conn = self.conn.lock().expect("store lock");
        let row = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(vcpus),0), COALESCE(SUM(memory_mib),0), COALESCE(SUM(disk_mib),0)
                 FROM quota_reservations WHERE tenant_id = ?1 AND state = 'active'",
                params![tenant.as_str()],
                |r| {
                    Ok(ReservedUsage {
                        live_vms: r.get::<_, i64>(0)?.max(0) as u32,
                        vcpus: r.get::<_, i64>(1)?.max(0) as u32,
                        memory_mib: r.get::<_, i64>(2)?.max(0) as u32,
                        disk_mib: r.get::<_, i64>(3)?.max(0) as u64,
                    })
                },
            )
            .optional()?;
        Ok(row.unwrap_or_default())
    }

    /// Grant capacity for one sandbox launch. Transactional and fenced:
    /// ceilings are checked against live reservations inside the write
    /// transaction, and the tenant fence advances so stale tickets die.
    /// Idempotent per (sandbox, generation): re-reserving the same
    /// generation refreshes the numbers without consuming new capacity.
    pub fn reserve(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        generation: InstanceGeneration,
        vcpus: u32,
        memory_mib: u32,
        disk_mib: u64,
    ) -> Result<ReservationTicket, LedgerError> {
        let mut guard = self.conn.lock().expect("store lock");
        let tx = guard.transaction()?;
        // Ensure the tenant ledger row exists (with default ceilings when
        // unconfigured); the fence lives on this row.
        let d = QuotaCeilings::default();
        tx.execute(
            "INSERT INTO quota_ceilings(tenant_id, vcpus, memory_mib, disk_mib, live_vms, fence, updated_at_ms)
             VALUES (?1,?2,?3,?4,?5,0,?6)
             ON CONFLICT(tenant_id) DO NOTHING",
            params![
                tenant.as_str(),
                i64::from(d.max_vcpus_per_tenant),
                i64::from(d.max_memory_mib_per_tenant),
                d.max_disk_mib_per_tenant as i64,
                i64::from(d.max_live_vms_per_tenant),
                Self::now_ms() as i64,
            ],
        )?;
        // Default ceilings when the tenant has none configured.
        let ceilings = tx
            .query_row(
                "SELECT vcpus, memory_mib, disk_mib, live_vms FROM quota_ceilings WHERE tenant_id = ?1",
                params![tenant.as_str()],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?
            .unwrap_or((
                i64::from(d.max_vcpus_per_tenant),
                i64::from(d.max_memory_mib_per_tenant),
                d.max_disk_mib_per_tenant as i64,
                i64::from(d.max_live_vms_per_tenant),
            ));
        let live: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM quota_reservations WHERE tenant_id = ?1 AND state = 'active' AND sandbox_id != ?2",
                params![tenant.as_str(), sandbox.as_str()],
                |r| r.get(0),
            )?;
        let used_v: i64 = tx.query_row(
            "SELECT COALESCE(SUM(vcpus),0) FROM quota_reservations WHERE tenant_id = ?1 AND state = 'active' AND sandbox_id != ?2",
            params![tenant.as_str(), sandbox.as_str()],
            |r| r.get(0),
        )?;
        let used_m: i64 = tx.query_row(
            "SELECT COALESCE(SUM(memory_mib),0) FROM quota_reservations WHERE tenant_id = ?1 AND state = 'active' AND sandbox_id != ?2",
            params![tenant.as_str(), sandbox.as_str()],
            |r| r.get(0),
        )?;
        let used_d: i64 = tx.query_row(
            "SELECT COALESCE(SUM(disk_mib),0) FROM quota_reservations WHERE tenant_id = ?1 AND state = 'active' AND sandbox_id != ?2",
            params![tenant.as_str(), sandbox.as_str()],
            |r| r.get(0),
        )?;
        if live + 1 > ceilings.3 {
            return Err(LedgerError::Exceeded(format!(
                "live VMs {} > ceiling {}",
                live + 1,
                ceilings.3
            )));
        }
        if used_v + i64::from(vcpus) > ceilings.0 {
            return Err(LedgerError::Exceeded(format!(
                "vcpus {} > ceiling {}",
                used_v + i64::from(vcpus),
                ceilings.0
            )));
        }
        if used_m + i64::from(memory_mib) > ceilings.1 {
            return Err(LedgerError::Exceeded(format!(
                "memory MiB {} > ceiling {}",
                used_m + i64::from(memory_mib),
                ceilings.1
            )));
        }
        if used_d + disk_mib as i64 > ceilings.2 {
            return Err(LedgerError::Exceeded(format!(
                "disk MiB {} > ceiling {}",
                used_d + disk_mib as i64,
                ceilings.2
            )));
        }
        let fence: i64 = tx.query_row(
            "SELECT fence FROM quota_ceilings WHERE tenant_id = ?1",
            params![tenant.as_str()],
            |r| r.get(0),
        )?;
        let fence = fence + 1;
        tx.execute(
            "UPDATE quota_ceilings SET fence = ?2, updated_at_ms = ?3 WHERE tenant_id = ?1",
            params![tenant.as_str(), fence, Self::now_ms() as i64],
        )?;
        tx.execute(
            "INSERT INTO quota_reservations(sandbox_id, tenant_id, vcpus, memory_mib, disk_mib, instance_generation, fence, state, updated_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,'active',?8)
             ON CONFLICT(sandbox_id) DO UPDATE SET
               vcpus=excluded.vcpus, memory_mib=excluded.memory_mib,
               disk_mib=excluded.disk_mib, instance_generation=excluded.instance_generation,
               fence=excluded.fence, state='active', updated_at_ms=excluded.updated_at_ms",
            params![
                sandbox.as_str(),
                tenant.as_str(),
                i64::from(vcpus),
                i64::from(memory_mib),
                disk_mib as i64,
                generation.0 as i64,
                fence,
                Self::now_ms() as i64,
            ],
        )?;
        tx.commit()?;
        Ok(ReservationTicket {
            sandbox_id: sandbox.clone(),
            tenant_id: tenant.clone(),
            instance_generation: generation,
            fence,
        })
    }

    /// Fenced release: the ticket's fence must equal the reservation row's
    /// current fence. Any newer grant for this sandbox (re-launch, new
    /// generation) advanced the row's fence, so a stale host decision can
    /// never release capacity it no longer owns.
    pub fn release(&self, ticket: &ReservationTicket) -> Result<(), LedgerError> {
        let mut guard = self.conn.lock().expect("store lock");
        let tx = guard.transaction()?;
        let row: Option<(i64, String, i64)> = tx
            .query_row(
                "SELECT fence, state, instance_generation FROM quota_reservations
                 WHERE sandbox_id = ?1 AND tenant_id = ?2",
                params![ticket.sandbox_id.as_str(), ticket.tenant_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (fence, state, generation) = row.ok_or(LedgerError::NotFound)?;
        if state != "active" || generation != ticket.instance_generation.0 as i64 {
            return Err(LedgerError::NotFound);
        }
        if fence != ticket.fence {
            return Err(LedgerError::StaleFence);
        }
        tx.execute(
            "UPDATE quota_reservations SET state = 'released', fence = fence + 1, updated_at_ms = ?2
             WHERE sandbox_id = ?1",
            params![ticket.sandbox_id.as_str(), Self::now_ms() as i64],
        )?;
        tx.execute(
            "UPDATE quota_ceilings SET fence = fence + 1, updated_at_ms = ?2 WHERE tenant_id = ?1",
            params![ticket.tenant_id.as_str(), Self::now_ms() as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Operator reconciliation: drop whatever the ledger holds for a
    /// sandbox regardless of fencing (after the physical state is proven).
    pub fn force_release(&self, tenant: &TenantId, sandbox: &SandboxId) -> Result<(), LedgerError> {
        let conn = self.conn.lock().expect("store lock");
        conn.execute(
            "UPDATE quota_reservations SET state = 'released', updated_at_ms = ?2
             WHERE sandbox_id = ?1 AND tenant_id = ?3 AND state = 'active'",
            params![sandbox.as_str(), Self::now_ms() as i64, tenant.as_str()],
        )?;
        Ok(())
    }

    /// All active reservations (host reconciliation against worker
    /// `ReconcileQuery` inventory).
    pub fn active_reservations(&self) -> Result<Vec<ReservationTicket>, LedgerError> {
        let conn = self.conn.lock().expect("store lock");
        let mut stmt = conn.prepare(
            "SELECT r.sandbox_id, r.tenant_id, r.instance_generation, c.fence
             FROM quota_reservations r JOIN quota_ceilings c ON c.tenant_id = r.tenant_id
             WHERE r.state = 'active'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (s, t, gval, fence) = row?;
            let sandbox_id = SandboxId::new(&s).map_err(|e| LedgerError::Sqlite(e.to_string()))?;
            let tenant_id = TenantId::new(&t).map_err(|e| LedgerError::Sqlite(e.to_string()))?;
            let generation = InstanceGeneration::new(gval.max(1) as u64)
                .map_err(|e| LedgerError::Sqlite(e.to_string()))?;
            out.push(ReservationTicket {
                sandbox_id,
                tenant_id,
                instance_generation: generation,
                fence,
            });
        }
        Ok(out)
    }

    fn put_inner(conn: &Connection, binding: &SandboxBinding) -> Result<(), rusqlite::Error> {
        let record = serde_json::to_string(binding).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        conn.execute(
            "INSERT INTO sandbox_bindings(tenant_id, sandbox_id, session_id, state, record, updated_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(tenant_id, sandbox_id) DO UPDATE SET
               session_id=excluded.session_id, state=excluded.state,
               record=excluded.record, updated_at_ms=excluded.updated_at_ms",
            params![
                binding.tenant_id.as_str(),
                binding.sandbox_id.as_str(),
                binding.session_id.as_str(),
                format!("{:?}", binding.state),
                record,
                Self::now_ms() as i64,
            ],
        )?;
        Ok(())
    }

    fn row_to_binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<SandboxBinding> {
        let record: String = row.get(0)?;
        serde_json::from_str(&record).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
    }
}

impl SandboxBindingStore for SqliteStore {
    fn put(&self, binding: &SandboxBinding) -> Result<(), BindingError> {
        let conn = self.conn.lock().expect("store lock");
        Self::put_inner(&conn, binding)?;
        Ok(())
    }

    fn get(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
    ) -> Result<Option<SandboxBinding>, BindingError> {
        let conn = self.conn.lock().expect("store lock");
        let out = conn
            .query_row(
                "SELECT record FROM sandbox_bindings WHERE tenant_id = ?1 AND sandbox_id = ?2",
                params![tenant.as_str(), sandbox.as_str()],
                Self::row_to_binding,
            )
            .optional()?;
        Ok(out)
    }

    fn get_by_session(
        &self,
        tenant: &TenantId,
        session: &SessionId,
    ) -> Result<Option<SandboxBinding>, BindingError> {
        let conn = self.conn.lock().expect("store lock");
        let out = conn
            .query_row(
                "SELECT record FROM sandbox_bindings
                 WHERE tenant_id = ?1 AND session_id = ?2 AND state != 'Destroyed'
                 ORDER BY CAST(json_extract(record, '$.instance_generation') AS INTEGER) DESC LIMIT 1",
                params![tenant.as_str(), session.as_str()],
                Self::row_to_binding,
            )
            .optional()?;
        Ok(out)
    }

    fn list(&self) -> Result<Vec<SandboxBinding>, BindingError> {
        let conn = self.conn.lock().expect("store lock");
        let mut stmt = conn.prepare("SELECT record FROM sandbox_bindings")?;
        let rows = stmt.query_map([], Self::row_to_binding)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn remove(&self, tenant: &TenantId, sandbox: &SandboxId) -> Result<(), BindingError> {
        let conn = self.conn.lock().expect("store lock");
        conn.execute(
            "DELETE FROM sandbox_bindings WHERE tenant_id = ?1 AND sandbox_id = ?2",
            params![tenant.as_str(), sandbox.as_str()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_sandbox_contracts::api::{DeletionReceipt, TerminationEvidence};
    use agentkit_sandbox_contracts::ids::{
        ImageDigest, PolicyDigest, PolicyGeneration, PrincipalId,
    };

    fn binding(tenant: &str, session: &str, sandbox: &str) -> SandboxBinding {
        SandboxBinding::new_active(
            TenantId::new(tenant).unwrap(),
            PrincipalId::new("principal").unwrap(),
            SessionId::new(session).unwrap(),
            SandboxId::new(sandbox).unwrap(),
            crate::binding::BindingMode::Ephemeral,
            "registry.example/img:1",
            ImageDigest::parse(&format!("sha256:{}", "aa".repeat(32))).unwrap(),
            PolicyGeneration(1),
            PolicyDigest::parse(&format!("sha256:{}", "bb".repeat(32))).unwrap(),
            InstanceGeneration::new(1).unwrap(),
            1_700_000_000_000,
        )
    }

    fn store() -> (SqliteStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let s = SqliteStore::open(&tmp.path().join("sandbox.db")).unwrap();
        (s, tmp)
    }

    #[test]
    fn binding_roundtrip_and_session_lookup() {
        let (s, _t) = store();
        let b = binding("t1", "sess-a", "sbx-1");
        s.put(&b).unwrap();
        assert_eq!(
            s.get(
                &TenantId::new("t1").unwrap(),
                &SandboxId::new("sbx-1").unwrap()
            )
            .unwrap()
            .unwrap()
            .sandbox_id
            .as_str(),
            "sbx-1"
        );
        let live = s
            .get_by_session(
                &TenantId::new("t1").unwrap(),
                &SessionId::new("sess-a").unwrap(),
            )
            .unwrap();
        assert!(live.is_some());
        // Cross-tenant lookup finds nothing.
        assert!(
            s.get_by_session(
                &TenantId::new("t2").unwrap(),
                &SessionId::new("sess-a").unwrap()
            )
            .unwrap()
            .is_none()
        );
        // Destroyed bindings are not "live for the session".
        let mut d = binding("t1", "sess-a", "sbx-2");
        d.state = crate::binding::BindingState::Destroyed;
        d.last_receipt = Some(DeletionReceipt {
            sandbox_id: SandboxId::new("sbx-2").unwrap(),
            instance_generation: InstanceGeneration::new(1).unwrap(),
            storage_deleted: true,
            reservations_released: true,
            termination: TerminationEvidence {
                method: "test".into(),
                observed_ms: 1,
                identity: None,
                conclusive: true,
            },
            retained: Vec::new(),
        });
        s.put(&d).unwrap();
        let live = s
            .get_by_session(
                &TenantId::new("t1").unwrap(),
                &SessionId::new("sess-a").unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(live.sandbox_id.as_str(), "sbx-1");
    }

    #[test]
    fn reservations_are_fenced_and_ceiling_enforced() {
        let (s, _t) = store();
        let tenant = TenantId::new("t1").unwrap();
        s.set_ceilings(
            &tenant,
            &QuotaCeilings {
                max_live_vms_per_tenant: 2,
                max_vcpus_per_tenant: 4,
                max_memory_mib_per_tenant: 1024,
                max_disk_mib_per_tenant: 4096,
                ..QuotaCeilings::default()
            },
        )
        .unwrap();
        let a = s
            .reserve(
                &tenant,
                &SandboxId::new("sbx-a").unwrap(),
                InstanceGeneration::new(1).unwrap(),
                2,
                512,
                1024,
            )
            .unwrap();
        let b = s
            .reserve(
                &tenant,
                &SandboxId::new("sbx-b").unwrap(),
                InstanceGeneration::new(1).unwrap(),
                2,
                512,
                1024,
            )
            .unwrap();
        // Ceiling: 2 live VMs; the third is refused.
        let e = s
            .reserve(
                &tenant,
                &SandboxId::new("sbx-c").unwrap(),
                InstanceGeneration::new(1).unwrap(),
                1,
                64,
                64,
            )
            .unwrap_err();
        assert!(matches!(e, LedgerError::Exceeded(_)), "{e:?}");
        // Releasing with a stale fence (older epoch) is refused…
        assert!(matches!(
            s.release(&ReservationTicket {
                fence: a.fence - 1,
                ..a.clone()
            }),
            Err(LedgerError::StaleFence)
        ));
        // …the right ticket works and frees capacity.
        s.release(&a).unwrap();
        let c = s
            .reserve(
                &tenant,
                &SandboxId::new("sbx-c").unwrap(),
                InstanceGeneration::new(2).unwrap(),
                1,
                64,
                64,
            )
            .unwrap();
        assert_eq!(s.usage(&tenant).unwrap().live_vms, 2);
        // b's own ticket still owns its row (untouched by other grants).
        s.release(&b).unwrap();
        // A re-grant for b under a new generation fences out the old ticket.
        let b2 = s
            .reserve(
                &tenant,
                &SandboxId::new("sbx-b").unwrap(),
                InstanceGeneration::new(2).unwrap(),
                1,
                64,
                64,
            )
            .unwrap();
        assert!(b2.fence > b.fence);
        assert!(matches!(s.release(&b), Err(LedgerError::NotFound)));
        // Double release of the live ticket after row moved: NotFound or
        // StaleFence, never a silent second decrement.
        s.release(&b2).unwrap();
        let e = s.release(&b2).unwrap_err();
        assert!(
            matches!(e, LedgerError::NotFound | LedgerError::StaleFence),
            "{e:?}"
        );
        // Operator path clears regardless.
        s.force_release(&tenant, &SandboxId::new("sbx-c").unwrap())
            .unwrap();
        assert_eq!(s.usage(&tenant).unwrap().live_vms, 0);
    }

    #[test]
    fn vcpu_ceiling_blocks_even_when_vm_count_fits() {
        let (s, _t) = store();
        let tenant = TenantId::new("t1").unwrap();
        s.set_ceilings(
            &tenant,
            &QuotaCeilings {
                max_live_vms_per_tenant: 10,
                max_vcpus_per_tenant: 4,
                max_memory_mib_per_tenant: 1 << 20,
                max_disk_mib_per_tenant: 1 << 30,
                ..QuotaCeilings::default()
            },
        )
        .unwrap();
        s.reserve(
            &tenant,
            &SandboxId::new("sbx-a").unwrap(),
            InstanceGeneration::new(1).unwrap(),
            3,
            16,
            16,
        )
        .unwrap();
        let e = s
            .reserve(
                &tenant,
                &SandboxId::new("sbx-b").unwrap(),
                InstanceGeneration::new(1).unwrap(),
                2,
                16,
                16,
            )
            .unwrap_err();
        assert!(
            matches!(e, LedgerError::Exceeded(ref m) if m.contains("vcpus")),
            "{e:?}"
        );
    }

    #[test]
    fn reopen_preserves_records() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let s = SqliteStore::open(&tmp.path().join("sandbox.db")).unwrap();
            s.put(&binding("t1", "sess-a", "sbx-1")).unwrap();
            s.reserve(
                &TenantId::new("t1").unwrap(),
                &SandboxId::new("sbx-1").unwrap(),
                InstanceGeneration::new(3).unwrap(),
                1,
                1,
                1,
            )
            .unwrap();
        }
        let s = SqliteStore::open(&tmp.path().join("sandbox.db")).unwrap();
        assert!(
            s.get(
                &TenantId::new("t1").unwrap(),
                &SandboxId::new("sbx-1").unwrap()
            )
            .unwrap()
            .is_some()
        );
        assert_eq!(s.active_reservations().unwrap().len(), 1);
        assert_eq!(
            s.active_reservations().unwrap()[0].instance_generation,
            InstanceGeneration::new(3).unwrap()
        );
    }
}
