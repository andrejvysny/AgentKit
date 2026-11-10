//! A private, disposable test vault for synthetic fixture credentials only.
use agentkit_mcp::{McpConfigError, McpConfigSecrets};
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub struct FixtureVault {
    path: PathBuf,
    values: Mutex<BTreeMap<String, String>>,
}

impl FixtureVault {
    pub fn open(database: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let path = database.with_extension("mcp-vault.json");
        let values = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            values: Mutex::new(values),
        })
    }

    fn update(&self, reference: &str, value: Option<&str>) -> Result<(), McpConfigError> {
        let mut values = self
            .values
            .lock()
            .map_err(|_| McpConfigError::SecretStore)?;
        let mut next = values.clone();
        match value {
            Some(value) => {
                next.insert(reference.to_owned(), value.to_owned());
            }
            None => {
                next.remove(reference);
            }
        }
        // tempfile creates the file with owner-only access. Persist only after
        // a full synced write so restart cannot observe a partial vault file.
        let parent = self.path.parent().ok_or(McpConfigError::SecretStore)?;
        let mut file =
            tempfile::NamedTempFile::new_in(parent).map_err(|_| McpConfigError::SecretStore)?;
        serde_json::to_writer(&mut file, &next).map_err(|_| McpConfigError::SecretStore)?;
        file.flush().map_err(|_| McpConfigError::SecretStore)?;
        file.as_file()
            .sync_all()
            .map_err(|_| McpConfigError::SecretStore)?;
        file.persist(&self.path)
            .map_err(|_| McpConfigError::SecretStore)?;
        *values = next;
        Ok(())
    }
}

#[async_trait]
impl McpConfigSecrets for FixtureVault {
    async fn get(&self, reference: &str) -> Result<Option<String>, McpConfigError> {
        Ok(self
            .values
            .lock()
            .map_err(|_| McpConfigError::SecretStore)?
            .get(reference)
            .cloned())
    }
    async fn set(&self, reference: &str, value: &str) -> Result<(), McpConfigError> {
        self.update(reference, Some(value))
    }
    async fn delete(&self, reference: &str) -> Result<(), McpConfigError> {
        self.update(reference, None)
    }
}
