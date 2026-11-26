//! Launcher configuration: one JSON file, read at startup. Everything the
//! container template depends on comes from here, never from a request.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use agentkit_sandbox_proto::launcher::Tier;
use agentkit_sandbox_proto::valid_sha256;
use serde::{Deserialize, Deserializer};

use crate::error::ConfigError;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
/// Largest configuration file read.
const FILE_MAX_BYTES: u64 = 64 * 1024;
/// Most CPUs a tier may name, in thousandths.
const CPUS_MILLI_MAX: u32 = 1_024_000;
/// Smallest and largest memory limit of a tier.
const MEMORY_MIN: u64 = 64 * MIB;
const MEMORY_MAX: u64 = 4096 * GIB;
/// Largest process limit (Linux `pid_max` ceiling).
const PIDS_MAX: u32 = 4_194_304;
/// Smallest and largest tmpfs size.
const TMPFS_MIN: u64 = MIB;
const TMPFS_MAX: u64 = 64 * GIB;
/// Longest stop grace a request may get.
const STOP_GRACE_MAX_MS: u64 = 10 * 60 * 1000;
/// Most concurrent client connections.
const CONNECTIONS_MAX: usize = 1024;
/// Largest `max_containers`.
const CONTAINERS_MAX: usize = 4096;

/// The launcher's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Unix socket of the Docker Engine API or Podman's compatible service.
    pub runtime_socket: PathBuf,
    /// Unix socket the launcher listens on. Its directory must belong to
    /// the launcher and not be writable by group or others, and every
    /// directory above it must belong to root or the launcher and not be
    /// writable by group or others (checked at startup).
    pub listen_socket: PathBuf,
    /// Mode of the listen socket, an octal string: `"0600"` (default) or
    /// `"0660"` with [`Config::listen_gid`] naming the server's group.
    #[serde(default = "default_listen_mode", deserialize_with = "octal_mode")]
    pub listen_mode: u32,
    /// Group the listen socket is given, if any.
    #[serde(default)]
    pub listen_gid: Option<u32>,
    /// Image reference containers are created from, by index digest:
    /// `localhost/agentkit-sandbox@sha256:<index digest>`.
    pub image_reference: String,
    /// The digest that reference names (`sha256:<hex>`); `ensure` must name
    /// it, and every container carries it as a label.
    pub image_digest: String,
    /// Host directory under which each machine gets one directory, named
    /// `agentkit_sandbox_proto::machine_dir_name(ws, generation)`,
    /// bind-mounted read-only at `/run/agentkit`. Absolute, reached
    /// without links, owned by the launcher, no permission bits for others
    /// and no group write. When the server is another user (Docker: the
    /// launcher is root), its group must be `server_gid` and searchable
    /// (mode 0710, group = the server's gid); when the launcher runs as the
    /// server's user (rootless Podman), mode 0700. Every directory above it
    /// must belong to root or the launcher and not be writable by group or
    /// others. Checked at startup and at every container start.
    pub socket_host_dir: PathBuf,
    /// Owner of each per-machine directory (the server that binds the
    /// control and egress sockets in it).
    pub server_uid: u32,
    /// Group of each per-machine directory, and the group that must be
    /// able to search `socket_host_dir` when the server is not the
    /// launcher's own user.
    pub server_gid: u32,
    /// The launcher's own data directory (stopped-through records). It and
    /// every directory above it must belong to root or the launcher and not
    /// be writable by group or others.
    pub state_dir: PathBuf,
    /// Resource tiers a request may name, by value. Default: one tier,
    /// `default`, of 2 CPUs, 2 GiB and 512 processes.
    #[serde(default = "default_tiers")]
    pub tiers: BTreeMap<String, Tier>,
    /// Size of the `/tmp` tmpfs in bytes (default 256 MiB).
    #[serde(default = "default_tmp_bytes")]
    pub tmp_size_bytes: u64,
    /// Size of the `/var/tmp` tmpfs in bytes (default 64 MiB).
    #[serde(default = "default_var_tmp_bytes")]
    pub var_tmp_size_bytes: u64,
    /// Prefix of container and volume names (default `aks-`).
    #[serde(default = "default_prefix")]
    pub name_prefix: String,
    /// Longest stop grace a request gets, in milliseconds (default 30 s).
    #[serde(default = "default_stop_grace_max_ms")]
    pub stop_grace_max_ms: u64,
    /// Client connections served at once (default 32).
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Most containers the launcher keeps at once, across workspaces and
    /// in any state (default 8). An `ensure` that would create one more is
    /// refused (`runtime_failed`: retry later, nothing was created).
    #[serde(default = "default_max_containers")]
    pub max_containers: usize,
}

fn default_listen_mode() -> u32 {
    0o600
}

fn default_tiers() -> BTreeMap<String, Tier> {
    let tier = Tier {
        cpus_milli: 2000,
        memory_bytes: 2 * GIB,
        pids: 512,
    };
    BTreeMap::from([("default".to_string(), tier)])
}

fn default_tmp_bytes() -> u64 {
    256 * MIB
}

fn default_var_tmp_bytes() -> u64 {
    64 * MIB
}

fn default_prefix() -> String {
    "aks-".to_string()
}

fn default_stop_grace_max_ms() -> u64 {
    30_000
}

fn default_max_connections() -> usize {
    32
}

fn default_max_containers() -> usize {
    8
}

fn octal_mode<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let text = String::deserialize(deserializer)?;
    u32::from_str_radix(&text, 8).map_err(|_| {
        serde::de::Error::custom("listen_mode must be an octal string such as \"0600\"")
    })
}

impl Config {
    /// Read and check the configuration file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let file = std::fs::File::open(path).map_err(ConfigError::Read)?;
        let mut text = String::new();
        file.take(FILE_MAX_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(ConfigError::Read)?;
        if text.len() as u64 > FILE_MAX_BYTES {
            return Err(invalid("the file is larger than 64 KiB"));
        }
        let config: Config = serde_json::from_str(&text).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    /// Refuse values that would weaken or break the template.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_paths()?;
        self.validate_image()?;
        self.validate_tiers()?;
        let mode_ok = matches!(self.listen_mode, 0o600 | 0o660);
        let checks = [
            (mode_ok, "listen_mode must be 0600 or 0660"),
            (
                (TMPFS_MIN..=TMPFS_MAX).contains(&self.tmp_size_bytes)
                    && (TMPFS_MIN..=TMPFS_MAX).contains(&self.var_tmp_size_bytes),
                "tmpfs sizes must be between 1 MiB and 64 GiB",
            ),
            (
                valid_prefix(&self.name_prefix),
                "name_prefix must be 2 to 16 characters of a-z, 0-9 and -, start with a letter and end with -",
            ),
            (
                self.stop_grace_max_ms <= STOP_GRACE_MAX_MS,
                "stop_grace_max_ms must be at most 10 minutes",
            ),
            (
                (1..=CONNECTIONS_MAX).contains(&self.max_connections),
                "max_connections must be between 1 and 1024",
            ),
            (
                (1..=CONTAINERS_MAX).contains(&self.max_containers),
                "max_containers must be between 1 and 4096",
            ),
        ];
        match checks.iter().find(|(ok, _)| !ok) {
            Some((_, problem)) => Err(invalid(problem)),
            None => Ok(()),
        }
    }

    fn validate_paths(&self) -> Result<(), ConfigError> {
        let paths = [
            ("runtime_socket", &self.runtime_socket),
            ("listen_socket", &self.listen_socket),
            ("socket_host_dir", &self.socket_host_dir),
            ("state_dir", &self.state_dir),
        ];
        for (name, path) in paths {
            if !plain_absolute(path) {
                return Err(invalid(&format!(
                    "{name} must be a normalized absolute path without . or .. components"
                )));
            }
        }
        if self.socket_host_dir.to_str().is_none() || self.socket_host_dir == Path::new("/") {
            return Err(invalid("socket_host_dir must be UTF-8 and not /"));
        }
        Ok(())
    }

    fn validate_image(&self) -> Result<(), ConfigError> {
        let digest_ok = self
            .image_digest
            .strip_prefix("sha256:")
            .is_some_and(valid_sha256);
        if !digest_ok {
            return Err(invalid("image_digest must be sha256:<64 lowercase hex>"));
        }
        let repository = self
            .image_reference
            .strip_suffix(&self.image_digest)
            .and_then(|rest| rest.strip_suffix('@'));
        let repository_ok = repository.is_some_and(|repo| {
            (1..=255).contains(&repo.len())
                && repo
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-/:".contains(&b))
        });
        if !repository_ok {
            return Err(invalid(
                "image_reference must be <repository>@<image_digest>, for example localhost/agentkit-sandbox@sha256:…",
            ));
        }
        Ok(())
    }

    fn validate_tiers(&self) -> Result<(), ConfigError> {
        if self.tiers.is_empty() {
            return Err(invalid("at least one tier is required"));
        }
        for (name, tier) in &self.tiers {
            let ok = !name.is_empty()
                && (1..=CPUS_MILLI_MAX).contains(&tier.cpus_milli)
                && (MEMORY_MIN..=MEMORY_MAX).contains(&tier.memory_bytes)
                && (1..=PIDS_MAX).contains(&tier.pids);
            if !ok {
                return Err(invalid(&format!(
                    "tier {name:?} needs cpus_milli 1..=1024000, memory_bytes 64 MiB..=4 TiB and pids 1..=4194304"
                )));
            }
        }
        Ok(())
    }

    /// True when `tier` equals one of the configured tiers.
    pub(crate) fn allows_tier(&self, tier: &Tier) -> bool {
        self.tiers.values().any(|allowed| allowed == tier)
    }
}

/// Absolute, only plain components, and spelled the way it normalizes
/// (no `.`, no doubled or trailing `/`), since it becomes a mount source.
fn plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
}

fn valid_prefix(prefix: &str) -> bool {
    (2..=16).contains(&prefix.len())
        && prefix.starts_with(|c: char| c.is_ascii_lowercase())
        && prefix.ends_with('-')
        && prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn invalid(problem: &str) -> ConfigError {
    ConfigError::Invalid(problem.to_string())
}

#[cfg(test)]
mod tests;
