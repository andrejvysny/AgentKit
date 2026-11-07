//! At-rest encryption for the credential values in `connector_connections`.
//!
//! AGENTKIT-72: `transport.env`/`transport.headers` values used to be stored
//! as plaintext strings inside the record JSON. Every credential-shaped value
//! is now sealed with AES-256-GCM before the row is written; a new write never
//! stores plaintext.
//!
//! # Key custody
//!
//! 1. **Required external key (production posture).** The application supplies
//!    a secret from its own custody (`SqliteStore::with_connector_credential_key`;
//!    the OS keychain behind the `agentkit-connections` SecretStore port is the
//!    reference mechanism) and sets `SqliteStore::with_connector_key_required(true)`.
//!    The secret is stretched with HKDF-SHA256. With the posture required and no
//!    key injected, any seal/open that would need the store-local key fails with
//!    `StoreError::InvalidInput("connector_credential_key_required: ...")` and the
//!    store-local key row is never created. A copied database file alone then
//!    yields no connector credentials.
//! 2. **Store-local key (development fallback only).** Without an injected key
//!    (and without the required posture) a random 32-byte data key is generated
//!    on first use and kept in the same database (`settings`:
//!    `agentkit.connector-crypto` / `v1`). The key shares the database file's
//!    fate, so a copied file yields the credentials. This is NOT suitable for a
//!    production security sign-off.
//!
//! # Envelope v2
//!
//! New writes use `agentkit.enc.v2:{key_id}:{hex(nonce||ct||tag)}`. `key_id` is
//! the first 16 hex chars of SHA-256 over the derived 32-byte AEAD key, so an
//! envelope names the key that sealed it without revealing it. The AEAD AAD is
//! `"{connection_id}\u{1f}{field_path}"` (e.g. `c1\u{1f}transport.env.API_TOKEN`),
//! binding a value to its connection row and field: moving a sealed value to
//! another row or field fails authentication. v1 envelopes
//! (`agentkit.enc.v1:`, no key id, no AAD) still open under the key they were
//! sealed with. Opening a v2 envelope whose `key_id` differs from the resolved
//! key fails closed with `RecoveryUnreadable` naming `key_id_mismatch`; no
//! other key is tried silently.
//!
//! # Read handling of older rows (fail-closed)
//!
//! A sealed value that does not open under the resolved key is an error, never a
//! fallback to ciphertext. A legacy plaintext value still reads and is lazily
//! re-sealed (v2) on the next `get_connection` / `list_connections`.
//!
//! # Migration sweep
//!
//! `SqliteStore::reseal_connector_credentials` runs one transaction over every
//! `connector_connections` row: legacy plaintext becomes v2 under the resolved
//! key; v1 envelopes are upgraded to v2; when an external key is injected,
//! values sealed under the store-local key are opened with it and resealed
//! under the external key, and the store-local key row is then deleted. Any
//! value that opens under no available key aborts the whole sweep with nothing
//! changed. The sweep is idempotent. It sets `PRAGMA secure_delete=ON` for the
//! transaction and runs `PRAGMA wal_checkpoint(TRUNCATE)` afterwards (a busy
//! checkpoint does not fail the sweep).
//!
//! # Limits
//!
//! `secure_delete` and the checkpoint reduce, but do not guarantee, removal of
//! old plaintext or the old local key from free pages, WAL history, filesystem
//! snapshots and backups. This is NOT cryptographic erasure: treat credentials
//! that ever sat in a database without an external key as exposed to anyone who
//! held a copy of it, and rotate them at the provider.
//!
//! FOLLOW-UP: rotating the external key is not implemented; a different key is
//! only refused (`key_id_mismatch`).

use crate::{Result, StoreError};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::SecureRandom;
use rusqlite::Connection;
use std::sync::Arc;

/// v1 marker: `PREFIX || hex(nonce||ct||tag)`, no key id, no AAD (read-only).
pub(crate) const ENVELOPE_PREFIX_V1: &str = "agentkit.enc.v1:";
/// v2 marker: `PREFIX || key_id || ":" || hex(nonce||ct||tag)`, AAD-bound.
pub(crate) const ENVELOPE_PREFIX_V2: &str = "agentkit.enc.v2:";
/// Marker of every envelope this module writes.
#[cfg(test)]
pub(crate) const ENVELOPE_PREFIX: &str = ENVELOPE_PREFIX_V2;
/// Settings row holding the store-local data key (resolution order 2).
pub(crate) const CREDENTIAL_KEY_NAMESPACE: &str = "agentkit.connector-crypto";
pub(crate) const CREDENTIAL_KEY_NAME: &str = "v1";

const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;

const HKDF_SALT: &[u8] = b"agentkit/connector-connections/v1";
const HKDF_INFO: &[u8] = b"credential-record-aead-key";

/// The sealed-value codec for one store instance.
pub(crate) struct ConnectorCrypto {
    key: LessSafeKey,
    key_id: String,
}

/// How a store resolves its record key: the injected application secret and
/// the required-key posture, cloned into each store job.
#[derive(Clone, Default)]
pub(crate) struct CryptoSource {
    pub(crate) injected: Option<Arc<Vec<u8>>>,
    pub(crate) required: bool,
}

impl CryptoSource {
    pub(crate) fn resolve(&self, db: &Connection) -> Result<ConnectorCrypto> {
        ConnectorCrypto::resolve(
            db,
            self.injected.as_deref().map(Vec::as_slice),
            self.required,
        )
    }
}

/// AAD binding a sealed value to its connection row and credential field.
fn aad_bytes(connection_id: &str, field_path: &str) -> Vec<u8> {
    format!("{connection_id}\u{1f}{field_path}").into_bytes()
}

/// The `key_id` of a v2 envelope, or `None` for v1 / plaintext / malformed.
pub(crate) fn envelope_key_id(value: &str) -> Option<&str> {
    value
        .strip_prefix(ENVELOPE_PREFIX_V2)?
        .split_once(':')
        .map(|(key_id, _)| key_id)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble"));
        text.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("nibble"));
    }
    text
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let high = char::from(bytes[index]).to_digit(16)?;
        let low = char::from(bytes[index + 1]).to_digit(16)?;
        out.push((high * 16 + low) as u8);
        index += 2;
    }
    Some(out)
}

fn unreadable(reason: &str) -> StoreError {
    StoreError::RecoveryUnreadable(format!("connector credential is unreadable: {reason}"))
}

impl ConnectorCrypto {
    fn from_key(key: [u8; KEY_LEN]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, &key);
        let key_id = hex_encode(&digest.as_ref()[..8]);
        Self {
            key_id,
            key: LessSafeKey::new(
                UnboundKey::new(&AES_256_GCM, &key).expect("AES-256 accepts a 32-byte key"),
            ),
        }
    }

    /// Stretch an application-provided secret into the record AEAD key with
    /// HKDF-SHA256 (RFC 5869, extract-then-expand, one 32-byte output block)
    /// over `ring::hmac` — a standard construction on an audited primitive,
    /// no bespoke cipher anywhere.
    fn stretched(secret: &[u8]) -> Self {
        let extract_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, HKDF_SALT);
        let prk = ring::hmac::sign(&extract_key, secret);
        let expand_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, prk.as_ref());
        let mut message = HKDF_INFO.to_vec();
        message.push(1u8);
        let okm = ring::hmac::sign(&expand_key, &message);
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(okm.as_ref());
        Self::from_key(key)
    }

    /// Seal a credential value as a v2 envelope bound to `connection_id` and
    /// `field_path`; the result never contains the plaintext.
    pub(crate) fn seal(
        &self,
        plaintext: &str,
        connection_id: &str,
        field_path: &str,
    ) -> Result<String> {
        let mut bytes = plaintext.as_bytes().to_vec();
        let mut random = [0u8; NONCE_LEN];
        ring::rand::SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| unreadable("nonce generation failed"))?;
        let nonce = Nonce::assume_unique_for_key(random);
        let aad = aad_bytes(connection_id, field_path);
        self.key
            .seal_in_place_separate_tag(nonce, Aad::from(aad.as_slice()), &mut bytes)
            .map(|tag| {
                let mut sealed = random.to_vec();
                sealed.extend_from_slice(&bytes);
                sealed.extend_from_slice(tag.as_ref());
                format!(
                    "{ENVELOPE_PREFIX_V2}{}:{}",
                    self.key_id,
                    hex_encode(&sealed)
                )
            })
            .map_err(|_| unreadable("sealing failed"))
    }

    /// Test-only: write a legacy v1 envelope (no key id, no AAD).
    #[cfg(test)]
    pub(crate) fn seal_v1(&self, plaintext: &str) -> String {
        let mut bytes = plaintext.as_bytes().to_vec();
        let mut random = [0u8; NONCE_LEN];
        ring::rand::SystemRandom::new()
            .fill(&mut random)
            .expect("nonce");
        let tag = self
            .key
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(random),
                Aad::empty(),
                &mut bytes,
            )
            .expect("seal");
        let mut sealed = random.to_vec();
        sealed.extend_from_slice(&bytes);
        sealed.extend_from_slice(tag.as_ref());
        format!("{ENVELOPE_PREFIX_V1}{}", hex_encode(&sealed))
    }

    /// True when a stored value carries a v1 or v2 envelope marker.
    pub(crate) fn is_sealed(value: &str) -> bool {
        value.starts_with(ENVELOPE_PREFIX_V1) || value.starts_with(ENVELOPE_PREFIX_V2)
    }

    /// Open a sealed value. A foreign, mis-bound or corrupt envelope is
    /// fail-closed: the error surfaces, the caller never sees ciphertext as
    /// plaintext. A v2 envelope sealed under another key is `key_id_mismatch`.
    pub(crate) fn open(
        &self,
        envelope: &str,
        connection_id: &str,
        field_path: &str,
    ) -> Result<String> {
        let (payload, aad) = if let Some(rest) = envelope.strip_prefix(ENVELOPE_PREFIX_V2) {
            let Some((key_id, payload)) = rest.split_once(':') else {
                return Err(unreadable("envelope is malformed"));
            };
            if key_id != self.key_id {
                return Err(unreadable(&format!(
                    "key_id_mismatch (sealed under {key_id}, resolved key is {})",
                    self.key_id
                )));
            }
            (payload, aad_bytes(connection_id, field_path))
        } else if let Some(payload) = envelope.strip_prefix(ENVELOPE_PREFIX_V1) {
            (payload, Vec::new())
        } else {
            return Err(unreadable("missing envelope marker"));
        };
        let Some(mut bytes) = hex_decode(payload) else {
            return Err(unreadable("envelope is not hex"));
        };
        if bytes.len() < NONCE_LEN + TAG_LEN {
            return Err(unreadable("envelope is truncated"));
        }
        let (nonce_bytes, ciphertext_with_tag) = bytes.split_at_mut(NONCE_LEN);
        let nonce = Nonce::assume_unique_for_key(
            <[u8; NONCE_LEN]>::try_from(nonce_bytes).expect("sliced to the nonce length"),
        );
        let plaintext = self
            .key
            .open_in_place(nonce, Aad::from(aad.as_slice()), ciphertext_with_tag)
            .map_err(|_| {
                unreadable("authentication failed (wrong key, wrong binding or corruption)")
            })?;
        String::from_utf8(plaintext.to_vec()).map_err(|_| unreadable("plaintext is not UTF-8"))
    }

    /// Resolve the store's record key: the injected application key when
    /// present, else the store-local key row (generated on first use). With
    /// `required` and no injected key this refuses before touching the
    /// database, so the store-local key row is never created.
    pub(crate) fn resolve(
        db: &Connection,
        injected: Option<&[u8]>,
        required: bool,
    ) -> Result<Self> {
        if let Some(secret) = injected {
            return Ok(Self::stretched(secret));
        }
        if required {
            return Err(StoreError::InvalidInput(
                "connector_credential_key_required: an external connector credential key must be \
                 supplied (SqliteStore::with_connector_credential_key); the store-local key is a \
                 development fallback"
                    .into(),
            ));
        }
        if let Some(crypto) = Self::from_stored(db)? {
            return Ok(crypto);
        }
        // Generate, insert-if-absent, then read the winner (two first-use
        // stores must converge on one key, not each keep their own).
        let mut candidate = [0u8; KEY_LEN];
        ring::rand::SystemRandom::new()
            .fill(&mut candidate)
            .map_err(|_| unreadable("key generation failed"))?;
        db.execute(
            "INSERT INTO settings(namespace,key,value) VALUES(?,?,?) \
             ON CONFLICT(namespace,key) DO NOTHING",
            rusqlite::params![
                CREDENTIAL_KEY_NAMESPACE,
                CREDENTIAL_KEY_NAME,
                format!("\"{}\"", hex_encode(&candidate))
            ],
        )?;
        Self::from_stored(db)?
            .ok_or_else(|| unreadable("store-local key row vanished during creation"))
    }

    pub(crate) fn from_stored(db: &Connection) -> Result<Option<Self>> {
        // A read failure is an error, never "no key yet": treating it as
        // absence would mint a second key over existing ciphertext.
        let existing: Option<String> = rusqlite::OptionalExtension::optional(db.query_row(
            "SELECT value FROM settings WHERE namespace=? AND key=?",
            rusqlite::params![CREDENTIAL_KEY_NAMESPACE, CREDENTIAL_KEY_NAME],
            |row| row.get::<_, Option<String>>(0),
        ))?
        .flatten();
        let Some(text) = existing else {
            return Ok(None);
        };
        let Some(bytes) = hex_decode(text.trim_matches('"')).filter(|bytes| bytes.len() == KEY_LEN)
        else {
            return Err(unreadable("store-local key row is corrupt"));
        };
        Ok(Some(Self::from_key(
            <[u8; KEY_LEN]>::try_from(bytes.as_slice()).expect("checked length"),
        )))
    }
}

/// Outcome of `SqliteStore::reseal_connector_credentials`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResealReport {
    /// Connection rows rewritten by the sweep.
    pub rows: u64,
    /// Sealed values rewritten: v1 upgraded to v2, or moved from the
    /// store-local key to the external key.
    pub values_resealed: u64,
    /// Legacy plaintext values sealed for the first time.
    pub legacy_plaintext_sealed: u64,
    /// True when the store-local key settings row existed and was deleted
    /// (only ever with an injected external key).
    pub local_key_retired: bool,
}

impl crate::SqliteStore {
    /// Migration sweep over every `connector_connections` credential value
    /// (see the module header, "Migration sweep"). One transaction: legacy
    /// plaintext and v1 envelopes become v2 under the resolved key; with an
    /// external key injected, values sealed under the store-local key are
    /// resealed under it and the store-local key row is deleted. A value that
    /// opens under no available key aborts the sweep with nothing changed
    /// (`RecoveryUnreadable`). Idempotent: a second run reports zeros.
    /// `secure_delete` and a WAL checkpoint reduce, but do not guarantee,
    /// removal of old plaintext from the file; this is not cryptographic erasure.
    pub async fn reseal_connector_credentials(&self) -> Result<ResealReport> {
        let source = self.connector_crypto_source();
        self.call(move |db| reseal_all(db, &source)).await
    }
}

fn reseal_all(db: &mut Connection, source: &CryptoSource) -> Result<ResealReport> {
    let previous: i64 = db.pragma_query_value(None, "secure_delete", |row| row.get(0))?;
    db.pragma_update(None, "secure_delete", "ON")?;
    let outcome = sweep_in_transaction(db, source);
    let restored = db.pragma_update(None, "secure_delete", previous);
    let report = outcome?;
    restored?;
    // A busy or failed checkpoint (readers, ambient transaction) must not
    // fail an already committed sweep; the old frames then merely linger.
    let _ = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    Ok(report)
}

enum Resealed {
    Unchanged,
    Plaintext(String),
    Sealed(String),
}

/// Decide the new form of one stored value (see `reseal_all`).
fn reseal_value(
    text: &str,
    connection_id: &str,
    path: &str,
    target: &ConnectorCrypto,
    local: Option<&ConnectorCrypto>,
) -> Result<Resealed> {
    if !ConnectorCrypto::is_sealed(text) {
        return Ok(Resealed::Plaintext(target.seal(
            text,
            connection_id,
            path,
        )?));
    }
    let candidates = [Some(target), local];
    let plaintext = if let Some(key_id) = envelope_key_id(text) {
        // v2 names its key: pick that one, never try the others.
        let Some(matched) = candidates
            .into_iter()
            .flatten()
            .find(|c| c.key_id == key_id)
        else {
            return Err(unreadable(&format!(
                "key_id_mismatch (sealed under {key_id}, no available key matches)"
            )));
        };
        let plaintext = matched.open(text, connection_id, path)?;
        if std::ptr::eq(matched, target) {
            return Ok(Resealed::Unchanged);
        }
        plaintext
    } else {
        // v1 carries no key id: the first available key that authenticates.
        candidates
            .into_iter()
            .flatten()
            .find_map(|c| c.open(text, connection_id, path).ok())
            .ok_or_else(|| unreadable("v1 envelope opens under no available key"))?
    };
    Ok(Resealed::Sealed(target.seal(
        &plaintext,
        connection_id,
        path,
    )?))
}

fn sweep_in_transaction(db: &mut Connection, source: &CryptoSource) -> Result<ResealReport> {
    let transaction = crate::transaction::atomic(db)?;
    let rows: Vec<(String, String)> = {
        let mut statement =
            transaction.prepare("SELECT id, record FROM connector_connections ORDER BY id")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    // The store-local key is only ever read here, never created; a corrupt
    // row simply cannot open anything (the sweep then aborts on such values).
    let local = if source.injected.is_some() {
        ConnectorCrypto::from_stored(&transaction).ok().flatten()
    } else {
        None
    };
    let mut target: Option<ConnectorCrypto> = None;
    let mut report = ResealReport {
        rows: 0,
        values_resealed: 0,
        legacy_plaintext_sealed: 0,
        local_key_retired: false,
    };
    for (id, text) in rows {
        let mut record = crate::managed_content::read_stored(&text)?;
        let Some(config) = record
            .get_mut("config")
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        let (mut resealed, mut plaintext) = (0u64, 0u64);
        crate::managed_content::for_each_credential(config, &mut |path, entry| {
            if target.is_none() {
                target = Some(source.resolve(&transaction)?);
            }
            let current = entry.as_str().expect("string values only").to_owned();
            let target = target.as_ref().expect("resolved above");
            match reseal_value(&current, &id, path, target, local.as_ref())? {
                Resealed::Unchanged => {}
                Resealed::Plaintext(value) => {
                    *entry = serde_json::Value::String(value);
                    plaintext += 1;
                }
                Resealed::Sealed(value) => {
                    *entry = serde_json::Value::String(value);
                    resealed += 1;
                }
            }
            Ok(())
        })?;
        if resealed + plaintext > 0 {
            transaction.execute(
                "UPDATE connector_connections SET record = ?2 WHERE id = ?1",
                rusqlite::params![
                    id,
                    crate::managed_content::persisted(&transaction, &record)?
                ],
            )?;
            report.rows += 1;
            report.values_resealed += resealed;
            report.legacy_plaintext_sealed += plaintext;
        }
    }
    if source.injected.is_some() {
        let deleted = transaction.execute(
            "DELETE FROM settings WHERE namespace=? AND key=?",
            rusqlite::params![CREDENTIAL_KEY_NAMESPACE, CREDENTIAL_KEY_NAME],
        )?;
        report.local_key_retired = deleted > 0;
    }
    transaction.commit()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SqliteStore, StoreError};
    use serde_json::{Value, json};
    use std::path::Path;

    const KEY: &[u8] = b"external application secret one";
    const OTHER_KEY: &[u8] = b"external application secret two";

    fn record(id: &str, token: &str, header: &str) -> Value {
        json!({
            "id": id, "connectorId": "github", "accountId": "acc", "revision": 1,
            "config": { "alias": id,
                "transport": { "kind": "http", "url": "https://connector.example.invalid/mcp",
                    "headers": { "Authorization": header },
                    "env": { "TOKEN": token } } },
        })
    }

    fn column(raw: &Connection, id: &str) -> String {
        raw.query_row(
            "SELECT record FROM connector_connections WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .expect("row")
    }

    fn set_column(raw: &Connection, id: &str, text: &str) {
        raw.execute(
            "UPDATE connector_connections SET record = ?2 WHERE id = ?1",
            rusqlite::params![id, text],
        )
        .expect("update");
    }

    fn local_key_rows(raw: &Connection) -> i64 {
        raw.query_row(
            "SELECT COUNT(*) FROM settings WHERE namespace=?",
            [CREDENTIAL_KEY_NAMESPACE],
            |row| row.get(0),
        )
        .expect("count")
    }

    /// True when any file of the database (main or -wal) holds `needle`.
    fn files_contain(db: &Path, needle: &str) -> bool {
        let wal = format!("{}-wal", db.display());
        [db.to_path_buf(), wal.into()]
            .iter()
            .any(|file| std::fs::read(file).is_ok_and(|bytes| contains(&bytes, needle.as_bytes())))
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn with_env(text: &str, value: &str) -> String {
        let mut json: Value = serde_json::from_str(text).expect("json");
        json["config"]["transport"]["env"]["TOKEN"] = json!(value);
        serde_json::to_string(&json).expect("json")
    }

    #[test]
    fn v2_envelope_names_key_and_binds_connection_and_field() {
        let crypto = ConnectorCrypto::stretched(KEY);
        let sealed = crypto
            .seal("canary-aad", "c1", "transport.env.A")
            .expect("seal");
        let rest = sealed.strip_prefix(ENVELOPE_PREFIX_V2).expect("v2");
        let (key_id, _) = rest.split_once(':').expect("key id");
        assert_eq!(key_id.len(), 16);
        assert!(key_id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(key_id, crypto.key_id);
        assert_eq!(
            crypto.open(&sealed, "c1", "transport.env.A").expect("open"),
            "canary-aad"
        );
        for (id, path) in [("c2", "transport.env.A"), ("c1", "transport.env.B")] {
            assert!(
                matches!(
                    crypto.open(&sealed, id, path),
                    Err(StoreError::RecoveryUnreadable(_))
                ),
                "{id} {path}"
            );
        }
        let other = ConnectorCrypto::stretched(OTHER_KEY);
        let error = other
            .open(&sealed, "c1", "transport.env.A")
            .expect_err("mismatch");
        assert!(error.to_string().contains("key_id_mismatch"), "{error}");
        let v1 = crypto.seal_v1("legacy");
        assert_eq!(crypto.open(&v1, "any", "path").expect("v1 opens"), "legacy");
    }

    #[tokio::test]
    async fn moved_sealed_value_fails_closed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("aad.db");
        let store = SqliteStore::open(&path, 4)
            .expect("open")
            .with_connector_credential_key(KEY);
        store
            .save_connection(record("c1", "canary-one", "Bearer one"), None)
            .await
            .expect("c1");
        store
            .save_connection(record("c2", "canary-two", "Bearer two"), None)
            .await
            .expect("c2");
        let raw = Connection::open(&path).expect("raw");
        // Row move: c1's sealed env value pasted into c2.
        let c1: Value = serde_json::from_str(&column(&raw, "c1")).expect("json");
        let sealed = c1["config"]["transport"]["env"]["TOKEN"]
            .as_str()
            .expect("sealed");
        set_column(&raw, "c2", &with_env(&column(&raw, "c2"), sealed));
        let moved = store
            .get_connection("c2".into())
            .await
            .expect_err("row move");
        assert!(
            matches!(moved, StoreError::RecoveryUnreadable(_)),
            "{moved}"
        );
        // Field move: c1's header envelope pasted into c1's env slot.
        let header = c1["config"]["transport"]["headers"]["Authorization"]
            .as_str()
            .expect("sealed");
        set_column(&raw, "c1", &with_env(&column(&raw, "c1"), header));
        let moved = store
            .get_connection("c1".into())
            .await
            .expect_err("field move");
        assert!(
            matches!(moved, StoreError::RecoveryUnreadable(_)),
            "{moved}"
        );
    }

    #[tokio::test]
    async fn required_without_key_refuses_and_creates_no_settings_row() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("required.db");
        // A credential-free row creates no key material, then a hand-forged
        // plaintext credential stands in for a pre-fix row.
        let plain = SqliteStore::open(&path, 4).expect("plain");
        plain
            .save_connection(
                json!({"id":"c9","connectorId":"g","accountId":"a","revision":1,
                    "config":{"alias":"c9","transport":{"kind":"stdio","command":"x"}}}),
                None,
            )
            .await
            .expect("credential-free save");
        drop(plain);
        let raw = Connection::open(&path).expect("raw");
        assert_eq!(local_key_rows(&raw), 0);
        let mut forged: Value = serde_json::from_str(&column(&raw, "c9")).expect("json");
        forged["config"]["transport"]["env"] = json!({"TOKEN": "canary-plain"});
        set_column(&raw, "c9", &forged.to_string());
        let store = SqliteStore::open(&path, 4)
            .expect("open")
            .with_connector_key_required(true);
        let needle = "connector_credential_key_required";
        let error = store
            .save_connection(record("c1", "canary-req", "Bearer req"), None)
            .await
            .expect_err("save needs the local key");
        assert!(
            matches!(&error, StoreError::InvalidInput(m) if m.starts_with(needle)),
            "{error}"
        );
        let read = store.get_connection("c9".into()).await.expect_err("read");
        assert!(read.to_string().contains(needle), "{read}");
        let listed = store.list_connections().await.expect_err("list");
        assert!(listed.to_string().contains(needle), "{listed}");
        let sweep = store
            .reseal_connector_credentials()
            .await
            .expect_err("sweep");
        assert!(sweep.to_string().contains(needle), "{sweep}");
        assert_eq!(local_key_rows(&raw), 0, "no store-local key row may appear");
    }

    #[tokio::test]
    async fn copied_database_without_the_external_key_yields_nothing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("origin.db");
        let store = SqliteStore::open(&path, 4)
            .expect("open")
            .with_connector_credential_key(KEY)
            .with_connector_key_required(true);
        store
            .save_connection(
                record("c1", "canary-copy-token", "Bearer canary-copy-header"),
                None,
            )
            .await
            .expect("save");
        drop(store);
        let copy = directory.path().join("copy.db");
        std::fs::copy(&path, &copy).expect("copy");
        assert!(!files_contain(&copy, "canary-copy-token"));
        assert!(!files_contain(&copy, "canary-copy-header"));
        let required = SqliteStore::open(&copy, 4)
            .expect("open copy")
            .with_connector_key_required(true);
        let error = required
            .get_connection("c1".into())
            .await
            .expect_err("no key");
        assert!(
            matches!(&error, StoreError::InvalidInput(m) if m.contains("connector_credential_key_required")),
            "{error}"
        );
        drop(required);
        let fallback = SqliteStore::open(&copy, 4).expect("open copy");
        let error = fallback
            .get_connection("c1".into())
            .await
            .expect_err("local key differs");
        assert!(error.to_string().contains("key_id_mismatch"), "{error}");
    }

    /// An old-format database: c1 sealed v1 under the store-local key, c2
    /// legacy plaintext, c3 sealed v2 under the store-local key.
    async fn old_format_db(path: &Path) {
        let store = SqliteStore::open(path, 4).expect("open");
        for id in ["c1", "c2", "c3"] {
            store
                .save_connection(record(id, "x", "y"), None)
                .await
                .expect("save");
        }
        drop(store);
        let raw = Connection::open(path).expect("raw");
        let local = ConnectorCrypto::resolve(&raw, None, false).expect("local key");
        let forge = |id: &str, token: &str, header: &str| {
            let mut json: Value = serde_json::from_str(&column(&raw, id)).expect("json");
            json["config"]["transport"]["env"]["TOKEN"] = json!(token);
            json["config"]["transport"]["headers"]["Authorization"] = json!(header);
            set_column(&raw, id, &serde_json::to_string(&json).expect("json"));
        };
        forge(
            "c1",
            &local.seal_v1("canary-v1-token"),
            &local.seal_v1("canary-v1-header"),
        );
        forge("c2", "canary-plain-token", "canary-plain-header");
        forge(
            "c3",
            &local
                .seal("canary-v2-token", "c3", "transport.env.TOKEN")
                .expect("seal"),
            &local
                .seal("canary-v2-header", "c3", "transport.headers.Authorization")
                .expect("seal"),
        );
    }

    #[tokio::test]
    async fn sweep_migrates_old_rows_retires_local_key_and_is_idempotent() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("old.db");
        old_format_db(&path).await;
        assert!(files_contain(&path, "canary-plain-token"), "precondition");
        let store = SqliteStore::open(&path, 4)
            .expect("open")
            .with_connector_credential_key(KEY)
            .with_connector_key_required(true);
        let report = store.reseal_connector_credentials().await.expect("sweep");
        assert_eq!(
            report,
            ResealReport {
                rows: 3,
                values_resealed: 4,
                legacy_plaintext_sealed: 2,
                local_key_retired: true
            }
        );
        let again = store
            .reseal_connector_credentials()
            .await
            .expect("second sweep");
        assert_eq!(
            again,
            ResealReport {
                rows: 0,
                values_resealed: 0,
                legacy_plaintext_sealed: 0,
                local_key_retired: false
            }
        );
        for id in ["c1", "c2", "c3"] {
            assert!(
                store
                    .get_connection(id.into())
                    .await
                    .expect("readable")
                    .is_some()
            );
        }
        drop(store);
        let raw = Connection::open(&path).expect("raw");
        assert_eq!(local_key_rows(&raw), 0);
        let external = ConnectorCrypto::resolve(&raw, Some(KEY), true).expect("external");
        let c2: Value = serde_json::from_str(&column(&raw, "c2")).expect("json");
        let sealed = c2["config"]["transport"]["env"]["TOKEN"]
            .as_str()
            .expect("sealed");
        assert_eq!(
            external
                .open(sealed, "c2", "transport.env.TOKEN")
                .expect("open"),
            "canary-plain-token"
        );
        let c1: Value = serde_json::from_str(&column(&raw, "c1")).expect("json");
        let sealed = c1["config"]["transport"]["headers"]["Authorization"]
            .as_str()
            .expect("sealed");
        assert_eq!(
            external
                .open(sealed, "c1", "transport.headers.Authorization")
                .expect("open"),
            "canary-v1-header"
        );
        drop(raw);
        for canary in [
            "canary-plain-token",
            "canary-plain-header",
            "canary-v1-token",
            "canary-v2-header",
        ] {
            assert!(
                !files_contain(&path, canary),
                "{canary} survives in db/-wal"
            );
        }
    }

    #[tokio::test]
    async fn wrong_external_key_fails_closed_and_sweep_changes_nothing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("wrong.db");
        old_format_db(&path).await;
        // c4 sealed under the genuine external key; the store below holds another.
        let genuine = SqliteStore::open(&path, 4)
            .expect("open")
            .with_connector_credential_key(KEY);
        genuine
            .save_connection(record("c4", "canary-ext-token", "canary-ext-header"), None)
            .await
            .expect("c4");
        drop(genuine);
        let raw = Connection::open(&path).expect("raw");
        let before: Vec<String> = ["c1", "c2", "c3", "c4"]
            .iter()
            .map(|id| column(&raw, id))
            .collect();
        let store = SqliteStore::open(&path, 4)
            .expect("open")
            .with_connector_credential_key(OTHER_KEY);
        let read = store.get_connection("c4".into()).await.expect_err("read");
        assert!(matches!(read, StoreError::RecoveryUnreadable(_)), "{read}");
        let sweep = store
            .reseal_connector_credentials()
            .await
            .expect_err("sweep aborts");
        assert!(
            matches!(sweep, StoreError::RecoveryUnreadable(_)),
            "{sweep}"
        );
        let after: Vec<String> = ["c1", "c2", "c3", "c4"]
            .iter()
            .map(|id| column(&raw, id))
            .collect();
        assert_eq!(before, after);
        assert_eq!(
            local_key_rows(&raw),
            1,
            "local key must survive an aborted sweep"
        );
    }
}
