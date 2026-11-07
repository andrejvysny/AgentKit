//! At-rest encryption for the credential values in `connector_connections`.
//!
//! AGENTKIT-72: `transport.env`/`transport.headers` values used to be stored
//! as plaintext strings inside the record JSON (masked `***` only on read
//! paths, never at rest). Every credential-shaped value is now sealed with
//! AES-256-GCM before the row is written; a new write never stores plaintext.
//!
//! Cryptographic dependency: `ring`, which is already in the workspace
//! `Cargo.lock` (the rustls stack the `http` feature pulls). No new crate
//! enters the build graph; only the store's dependency edge is added.
//!
//! Key custody, in resolution order:
//!   1. An application-provided key (`SqliteStore::with_connector_credential_key`),
//!      stretched with HKDF-SHA256 under a domain-separated salt/info pair.
//!      This is the production posture: the key lives outside the database,
//!      supplied by the app from its own custody (the OS keychain vault the
//!      `agentkit-connections` SecretStore port fronts is the reference
//!      mechanism).
//!   2. Otherwise a store-local data key: 32 uniformly random bytes generated
//!      on first use and kept in the `settings` table
//!      (`agentkit.connector-crypto` / `v1`). This keeps credential bytes out
//!      of the `connector_connections` column and out of naive dumps/backups
//!      of that table, but the key then shares the database file's fate —
//!      defence in depth, not a substitute for (1).
//!
//! Read handling of pre-fix rows (fail-closed policy):
//!   * A sealed value that does not decrypt under the resolved key is an
//!     error (`RecoveryUnreadable`), never a silent fallback to ciphertext
//!     or to some other row's plaintext.
//!   * A legacy plaintext value (written before this fix) still reads, and
//!     the read path lazily RE-ENCRYPTS it in place: the next `get_connection`
//!     / `list_connections` seals it durably in the same call. Reopening an
//!     old database therefore migrates it credential by credential, and no
//!     plaintext write path remains (all writes seal).

use crate::{Result, StoreError};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::SecureRandom;
use rusqlite::Connection;

/// Envelope marker: a stored credential value is `PREFIX || hex(nonce||ct||tag)`.
pub(crate) const ENVELOPE_PREFIX: &str = "agentkit.enc.v1:";
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
        Self {
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

    /// Seal a credential value; the result never contains the plaintext.
    pub(crate) fn seal(&self, plaintext: &str) -> Result<String> {
        let mut bytes = plaintext.as_bytes().to_vec();
        let mut random = [0u8; NONCE_LEN];
        ring::rand::SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| unreadable("nonce generation failed"))?;
        let nonce = Nonce::assume_unique_for_key(random);
        self.key
            .seal_in_place_separate_tag(nonce, Aad::empty(), &mut bytes)
            .map(|tag| {
                let mut sealed = random.to_vec();
                sealed.extend_from_slice(&bytes);
                sealed.extend_from_slice(tag.as_ref());
                format!("{ENVELOPE_PREFIX}{}", hex_encode(&sealed))
            })
            .map_err(|_| unreadable("sealing failed"))
    }

    /// True when a stored value carries the envelope marker.
    pub(crate) fn is_sealed(value: &str) -> bool {
        value.starts_with(ENVELOPE_PREFIX)
    }

    /// Open a sealed value. A foreign or corrupt envelope is fail-closed:
    /// the error surfaces, the caller never sees ciphertext as plaintext.
    pub(crate) fn open(&self, envelope: &str) -> Result<String> {
        let Some(payload) = envelope.strip_prefix(ENVELOPE_PREFIX) else {
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
            .open_in_place(nonce, Aad::empty(), ciphertext_with_tag)
            .map_err(|_| unreadable("authentication failed (wrong key or corruption)"))?;
        String::from_utf8(plaintext.to_vec()).map_err(|_| unreadable("plaintext is not UTF-8"))
    }

    /// Resolve the store's record key: the injected application key when
    /// present, else the store-local key row (generated on first use).
    pub(crate) fn resolve(db: &Connection, injected: Option<&[u8]>) -> Result<Self> {
        if let Some(secret) = injected {
            return Ok(Self::stretched(secret));
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

    fn from_stored(db: &Connection) -> Result<Option<Self>> {
        let existing: Option<String> = db
            .query_row(
                "SELECT value FROM settings WHERE namespace=? AND key=?",
                rusqlite::params![CREDENTIAL_KEY_NAMESPACE, CREDENTIAL_KEY_NAME],
                |row| row.get(0),
            )
            .unwrap_or(None);
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
