use crate::{McpClientError, oauth::ManagedOAuthOptions};
use rmcp::transport::auth::{
    AuthorizationMetadata, OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRedirectPolicy,
    OAuthHttpRequest,
};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use url::Url;

pub(crate) struct CheckedHttp {
    inner: Arc<dyn OAuthHttpClient>,
    allowed: BTreeSet<String>,
    resource: String,
    refused: AtomicBool,
}
impl CheckedHttp {
    pub(crate) fn new(
        options: &ManagedOAuthOptions,
        inner: Arc<dyn OAuthHttpClient>,
    ) -> Result<Self, McpClientError> {
        let allowed = options
            .allowed_origins
            .iter()
            .map(|origin| {
                let url = Url::parse(origin).map_err(|_| McpClientError::Configuration)?;
                if url.scheme() != "https"
                    || url.origin().ascii_serialization() != *origin
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return Err(McpClientError::Configuration);
                }
                Ok(origin.clone())
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let policy = Self {
            inner,
            allowed,
            resource: options.resource_url.clone(),
            refused: AtomicBool::new(false),
        };
        policy.check(&options.resource_url)?;
        Ok(policy)
    }
    pub(crate) fn check(&self, input: &str) -> Result<(), McpClientError> {
        let url = Url::parse(input).map_err(|_| McpClientError::Configuration)?;
        if self.refused.load(Ordering::Acquire)
            || url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !self.allowed.contains(&url.origin().ascii_serialization())
        {
            self.refused.store(true, Ordering::Release);
            return Err(McpClientError::Configuration);
        }
        Ok(())
    }
    pub(crate) fn validate_metadata(
        &self,
        metadata: &AuthorizationMetadata,
    ) -> Result<(), McpClientError> {
        for endpoint in [
            Some(metadata.authorization_endpoint.as_str()),
            Some(metadata.token_endpoint.as_str()),
            metadata.registration_endpoint.as_deref(),
            metadata.issuer.as_deref(),
            metadata.jwks_uri.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            self.check(endpoint)?;
        }
        Ok(())
    }
}
impl OAuthHttpClient for CheckedHttp {
    fn execute(&self, mut request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            self.check(&request.request.uri().to_string())?;
            request.redirect_policy = OAuthHttpRedirectPolicy::Stop;
            let response = self.inner.execute(request).await?;
            if response.status().is_redirection() || response.body().len() > 4_194_304 {
                self.refused.store(true, Ordering::Release);
                return Err(McpClientError::Configuration.into());
            }
            if let Ok(value) = serde_json::from_slice::<Value>(response.body())
                && let Some(resource) = value.get("resource").and_then(Value::as_str)
                && Url::parse(resource).ok().as_ref().map(Url::as_str)
                    != Some(self.resource.as_str())
            {
                self.refused.store(true, Ordering::Release);
                return Err(McpClientError::Configuration.into());
            }
            Ok(response)
        })
    }
}

pub(crate) struct Network(reqwest::Client);
impl Network {
    pub(crate) fn new() -> Result<Self, McpClientError> {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map(Self)
            .map_err(|_| McpClientError::Connect)
    }
}
impl OAuthHttpClient for Network {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let (parts, body) = request.request.into_parts();
            let mut response = self
                .0
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(body)
                .send()
                .await
                .map_err(|_| McpClientError::Connect)?;
            let mut builder = http::Response::builder().status(response.status());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| McpClientError::Connect)?
            {
                if body.len().saturating_add(chunk.len()) > 4_194_304 {
                    return Err(McpClientError::Configuration.into());
                }
                body.extend_from_slice(&chunk);
            }
            builder
                .body(body)
                .map_err(|_| McpClientError::Connect.into())
        })
    }
}
