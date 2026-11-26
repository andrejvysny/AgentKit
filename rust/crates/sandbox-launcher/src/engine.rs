//! [`RuntimeApi`] over the Docker Engine API, which Docker and Podman's
//! compatible service both answer (`docs/evidence/sandbox-m2/
//! wp-2.1-runtime-probes.md`). Paths are versioned at 1.41, Podman's
//! compat level, which the probes exercised on both runtimes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use hyper::Method;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::error::bounded;
use crate::runtime::{
    ContainerSummary, CreatedContainer, Removal, RuntimeApi, RuntimeError, VolumeInfo,
};
use crate::transport::{Response, Transport, encode_query};

/// API version prefix of every call but `/system/df`.
const API: &str = "/v1.41";
/// Volume sizes. Unversioned so Docker honours `type=volume` (API 1.42+)
/// and does not size every image and container; Podman ignores `type`.
const DF_PATH: &str = "/system/df?type=volume";
/// Deadline of an ordinary call.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline of a create (the runtime may set up volumes and mounts).
const CREATE_TIMEOUT: Duration = Duration::from_secs(60);
/// Added to the grace period for a stop call's deadline.
const STOP_MARGIN: Duration = Duration::from_secs(30);
/// Deadline of `/system/df`, which walks every volume on the host.
const DF_TIMEOUT: Duration = Duration::from_secs(60);

/// Engine API client over a unix socket.
#[derive(Debug, Clone)]
pub(crate) struct EngineApi {
    transport: Transport,
}

impl EngineApi {
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self {
            transport: Transport::new(socket),
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Response, RuntimeError> {
        let body = body.map(|value| value.to_string().into_bytes());
        self.transport.call(method, path, body, timeout).await
    }
}

#[derive(Deserialize)]
struct ContainerJson {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "State", default)]
    state: String,
    #[serde(rename = "Labels", default)]
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct InspectJson {
    #[serde(rename = "Config", default)]
    config: Option<InspectConfigJson>,
}

#[derive(Deserialize)]
struct InspectConfigJson {
    #[serde(rename = "Env", default)]
    env: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct CreatedJson {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Warnings", default)]
    warnings: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct VolumeJson {
    #[serde(rename = "Driver", default)]
    driver: String,
    #[serde(rename = "Labels", default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(rename = "Options", default)]
    options: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct DiskUsageJson {
    #[serde(rename = "Volumes", default)]
    volumes: Option<Vec<VolumeUsageJson>>,
}

#[derive(Deserialize)]
struct VolumeUsageJson {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "UsageData", default)]
    usage: Option<UsageDataJson>,
}

#[derive(Deserialize)]
struct UsageDataJson {
    #[serde(rename = "Size", default)]
    size: Option<i64>,
}

#[async_trait]
impl RuntimeApi for EngineApi {
    async fn list_containers(&self, filter: &str) -> Result<Vec<ContainerSummary>, RuntimeError> {
        let filters = json!({ "label": [filter] }).to_string();
        let path = format!(
            "{API}/containers/json?all=true&filters={}",
            encode_query(&filters)
        );
        let response = self.call(Method::GET, &path, None, CALL_TIMEOUT).await?;
        expect(&response, &[200])?;
        let items: Vec<ContainerJson> = parse(&response)?;
        items.into_iter().map(summary).collect()
    }

    async fn container_exists(&self, name: &str) -> Result<bool, RuntimeError> {
        let path = format!("{API}/containers/{}/json", segment(name)?);
        let response = self.call(Method::GET, &path, None, CALL_TIMEOUT).await?;
        match response.status {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(status_error(&response)),
        }
    }

    async fn container_env(&self, id: &str) -> Result<Vec<String>, RuntimeError> {
        let path = format!("{API}/containers/{}/json", segment(id)?);
        let response = self.call(Method::GET, &path, None, CALL_TIMEOUT).await?;
        expect(&response, &[200])?;
        let inspected: InspectJson = parse(&response)?;
        Ok(inspected
            .config
            .and_then(|config| config.env)
            .unwrap_or_default())
    }

    async fn create_container(
        &self,
        name: &str,
        body: &Value,
    ) -> Result<CreatedContainer, RuntimeError> {
        let path = format!("{API}/containers/create?name={}", segment(name)?);
        let response = self
            .call(Method::POST, &path, Some(body), CREATE_TIMEOUT)
            .await?;
        expect(&response, &[201])?;
        let created: CreatedJson = parse(&response)?;
        Ok(CreatedContainer {
            id: checked_id(created.id)?,
            warnings: created.warnings.unwrap_or_default(),
        })
    }

    async fn start_container(&self, id: &str) -> Result<(), RuntimeError> {
        let path = format!("{API}/containers/{}/start", segment(id)?);
        let response = self.call(Method::POST, &path, None, CALL_TIMEOUT).await?;
        expect(&response, &[204, 304])
    }

    async fn stop_container(&self, id: &str, grace_secs: u64) -> Result<(), RuntimeError> {
        let path = format!("{API}/containers/{}/stop?t={grace_secs}", segment(id)?);
        let timeout = Duration::from_secs(grace_secs).saturating_add(STOP_MARGIN);
        let response = self.call(Method::POST, &path, None, timeout).await?;
        expect(&response, &[204, 304, 404])
    }

    async fn remove_container(&self, id: &str) -> Result<(), RuntimeError> {
        let path = format!("{API}/containers/{}?force=true", segment(id)?);
        let response = self.call(Method::DELETE, &path, None, CALL_TIMEOUT).await?;
        expect(&response, &[204, 404])
    }

    async fn inspect_volume(&self, name: &str) -> Result<Option<VolumeInfo>, RuntimeError> {
        let path = format!("{API}/volumes/{}", segment(name)?);
        let response = self.call(Method::GET, &path, None, CALL_TIMEOUT).await?;
        if response.status == 404 {
            return Ok(None);
        }
        expect(&response, &[200])?;
        let volume: VolumeJson = parse(&response)?;
        Ok(Some(VolumeInfo {
            driver: volume.driver,
            labels: volume.labels.unwrap_or_default(),
            options: volume.options.unwrap_or_default(),
        }))
    }

    async fn create_volume(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<(), RuntimeError> {
        let body = json!({ "Name": segment(name)?, "Driver": "local", "Labels": labels });
        let path = format!("{API}/volumes/create");
        let response = self
            .call(Method::POST, &path, Some(&body), CALL_TIMEOUT)
            .await?;
        expect(&response, &[200, 201])
    }

    async fn remove_volume(&self, name: &str) -> Result<Removal, RuntimeError> {
        let path = format!("{API}/volumes/{}", segment(name)?);
        let response = self.call(Method::DELETE, &path, None, CALL_TIMEOUT).await?;
        match response.status {
            204 => Ok(Removal::Removed),
            404 => Ok(Removal::Absent),
            409 => Ok(Removal::InUse),
            _ => Err(status_error(&response)),
        }
    }

    async fn volume_sizes(&self) -> Result<BTreeMap<String, Option<u64>>, RuntimeError> {
        let response = self.call(Method::GET, DF_PATH, None, DF_TIMEOUT).await?;
        expect(&response, &[200])?;
        let usage: DiskUsageJson = parse(&response)?;
        let sizes = usage.volumes.unwrap_or_default().into_iter().map(|volume| {
            let size = volume.usage.and_then(|usage| usage.size);
            (volume.name, size.and_then(|size| u64::try_from(size).ok()))
        });
        Ok(sizes.collect())
    }
}

fn summary(item: ContainerJson) -> Result<ContainerSummary, RuntimeError> {
    Ok(ContainerSummary {
        id: checked_id(item.id)?,
        state: item.state,
        labels: item.labels.unwrap_or_default(),
    })
}

/// A container id from the runtime, refused unless it is plain hex-like
/// text that is safe in a URL path.
fn checked_id(id: String) -> Result<String, RuntimeError> {
    if (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Ok(id)
    } else {
        Err(RuntimeError::Malformed(
            "container id is not alphanumeric".into(),
        ))
    }
}

/// A name or id used as one URL path segment: only characters the launcher
/// itself puts in names. Anything else is a bug, refused before sending.
fn segment(text: &str) -> Result<&str, RuntimeError> {
    let ok = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    if ok {
        Ok(text)
    } else {
        Err(RuntimeError::Malformed(
            "name is not a safe path segment".into(),
        ))
    }
}

fn expect(response: &Response, statuses: &[u16]) -> Result<(), RuntimeError> {
    if statuses.contains(&response.status) {
        Ok(())
    } else {
        Err(status_error(response))
    }
}

/// The runtime's `{"message": …}`, bounded; never the raw body.
fn status_error(response: &Response) -> RuntimeError {
    #[derive(Deserialize)]
    struct Message {
        message: String,
    }
    let message = serde_json::from_slice::<Message>(&response.body)
        .map(|body| bounded(&body.message))
        .unwrap_or_else(|_| format!("{} bytes of non-JSON body", response.body.len()));
    RuntimeError::Status {
        status: response.status,
        message,
    }
}

fn parse<T: DeserializeOwned>(response: &Response) -> Result<T, RuntimeError> {
    serde_json::from_slice(&response.body)
        .map_err(|error| RuntimeError::Malformed(bounded(&error.to_string())))
}

#[cfg(test)]
mod tests;
