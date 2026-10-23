use crate::{
    AppConnections, Capability, Connection, ConnectionError, ConnectionId, ResolvedSecrets, Secret,
    SecretKey, SharedSecretStore,
};
use async_trait::async_trait;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

/// Exact bytes and disclosed categories are bound to this single immutable plan.
pub struct PlanInput {
    pub capability: Capability,
    pub model: Option<String>,
    pub disclosed_data: BTreeSet<String>,
    pub payload: Vec<u8>,
}

#[derive(Clone)]
pub struct RequestPlan {
    authority: Arc<Mutex<Authority>>,
    id: u64,
    connection: Connection,
    destination: url::Url,
    input: Arc<PlanInput>,
}

impl RequestPlan {
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn destination(&self) -> &url::Url {
        &self.destination
    }
    pub fn capability(&self) -> &Capability {
        &self.input.capability
    }
    pub fn model(&self) -> Option<&str> {
        self.input.model.as_deref()
    }
    pub fn disclosed_data(&self) -> &BTreeSet<String> {
        &self.input.disclosed_data
    }
    pub fn payload(&self) -> &[u8] {
        &self.input.payload
    }
}

/// Tokens can only be issued by explicitly approving an exact plan.
#[derive(Clone)]
pub struct ConsentToken {
    authority: Arc<Mutex<Authority>>,
    id: u64,
}

#[derive(Default)]
struct Authority {
    next_id: u64,
    approvals: BTreeMap<u64, Approval>,
}

enum Approval {
    Exact(u64),
    Scope(Box<Scope>),
}

struct Scope {
    connection: Connection,
    destination: url::Url,
    capability: Capability,
    model: Option<String>,
    disclosed_data: BTreeSet<String>,
}

impl Approval {
    fn covers(&self, plan: &RequestPlan) -> bool {
        match self {
            Self::Exact(id) => *id == plan.id,
            Self::Scope(scope) => {
                scope.connection == plan.connection
                    && scope.destination == plan.destination
                    && scope.capability == plan.input.capability
                    && scope.model == plan.input.model
                    && scope.disclosed_data == plan.input.disclosed_data
            }
        }
    }
}

impl Authority {
    fn next(&mut self) -> Result<u64, ConnectionError> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(ConnectionError::Exhausted)?;
        Ok(self.next_id)
    }
}

pub struct NetworkResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub type DispatchFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ConnectionError>> + Send + 'a>>;

/// Stream consumers check this before publishing every event or chunk.
#[derive(Clone)]
pub struct RequestFence {
    native: NativeConnections,
    plan: RequestPlan,
    consent: ConsentToken,
}

impl RequestFence {
    pub fn check(&self) -> Result<(), ConnectionError> {
        self.native.validate_consent(&self.plan, &self.consent)
    }
}

/// Native hosts inject discovery/probe/provider transports. Implementations must
/// send only the approved payload to the exact endpoint and must reject redirects.
#[async_trait]
pub trait ConnectionNetwork: Send + Sync {
    /// Check adapter/protocol support without reading credentials or doing network I/O.
    fn check_available(&self, plan: &RequestPlan) -> Result<(), ConnectionError>;
    fn supports_extra_headers(&self) -> bool {
        false
    }
    async fn send(
        &self,
        plan: &RequestPlan,
        credential: Option<&Secret>,
    ) -> Result<NetworkResponse, ConnectionError>;

    async fn send_resolved(
        &self,
        plan: &RequestPlan,
        secrets: &ResolvedSecrets,
    ) -> Result<NetworkResponse, ConnectionError> {
        if !secrets.headers.is_empty() {
            return Err(ConnectionError::NetworkUnavailable);
        }
        self.send(plan, secrets.api_key.as_ref()).await
    }
}

pub struct UnavailableNetwork;

#[async_trait]
impl ConnectionNetwork for UnavailableNetwork {
    fn check_available(&self, _plan: &RequestPlan) -> Result<(), ConnectionError> {
        Err(ConnectionError::NetworkUnavailable)
    }
    async fn send(
        &self,
        _plan: &RequestPlan,
        _credential: Option<&Secret>,
    ) -> Result<NetworkResponse, ConnectionError> {
        Err(ConnectionError::NetworkUnavailable)
    }
}

/// No implicit account/model/destination selection or credential fallback occurs.
#[derive(Clone)]
pub struct NativeConnections {
    pub(crate) repository: AppConnections,
    pub(crate) secrets: SharedSecretStore,
    network: Arc<dyn ConnectionNetwork>,
    authority: Arc<Mutex<Authority>>,
}

impl NativeConnections {
    pub fn new(
        repository: AppConnections,
        secrets: SharedSecretStore,
        network: Arc<dyn ConnectionNetwork>,
    ) -> Self {
        Self {
            repository,
            secrets,
            network,
            authority: Arc::default(),
        }
    }

    pub fn plan(
        &self,
        id: &ConnectionId,
        input: PlanInput,
    ) -> Result<RequestPlan, ConnectionError> {
        let endpoint = self.repository.get(id)?.config.endpoint;
        self.plan_to(id, input, endpoint)
    }

    /// Routes are explicit, constrained to the configured origin and base path.
    pub fn plan_to(
        &self,
        id: &ConnectionId,
        input: PlanInput,
        destination: url::Url,
    ) -> Result<RequestPlan, ConnectionError> {
        let connection = self.repository.get(id)?;
        validate_input(&connection, &input)?;
        validate_destination(&connection.config.endpoint, &destination)?;
        let id = self
            .authority
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?
            .next()?;
        Ok(RequestPlan {
            authority: self.authority.clone(),
            id,
            connection,
            destination,
            input: Arc::new(input),
        })
    }

    pub fn approve(&self, plan: &RequestPlan) -> Result<ConsentToken, ConnectionError> {
        self.issue_consent(plan, Approval::Exact(plan.id))
    }

    /// Explicit reusable consent covers exact categories, model, account, destination,
    /// capability and generation. It does not approve undisclosed data categories.
    pub fn approve_scope(&self, plan: &RequestPlan) -> Result<ConsentToken, ConnectionError> {
        let scope = Scope {
            connection: plan.connection.clone(),
            destination: plan.destination.clone(),
            capability: plan.input.capability.clone(),
            model: plan.input.model.clone(),
            disclosed_data: plan.input.disclosed_data.clone(),
        };
        self.issue_consent(plan, Approval::Scope(Box::new(scope)))
    }

    fn issue_consent(
        &self,
        plan: &RequestPlan,
        approval: Approval,
    ) -> Result<ConsentToken, ConnectionError> {
        self.validate_plan(plan)?;
        let mut authority = self
            .authority
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?;
        let id = authority.next()?;
        authority.approvals.insert(id, approval);
        Ok(ConsentToken {
            authority: self.authority.clone(),
            id,
        })
    }

    pub fn revoke(&self, consent: &ConsentToken) -> Result<(), ConnectionError> {
        if !Arc::ptr_eq(&self.authority, &consent.authority) {
            return Err(ConnectionError::ConsentDenied);
        }
        self.authority
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?
            .approvals
            .remove(&consent.id);
        Ok(())
    }

    pub fn credential_key(&self, id: &ConnectionId) -> Result<SecretKey, ConnectionError> {
        Ok(SecretKey::for_connection(&self.repository.get(id)?))
    }

    pub fn credential_key_at(
        &self,
        id: &ConnectionId,
        expected_generation: u64,
    ) -> Result<SecretKey, ConnectionError> {
        let connection = self.repository.get(id)?;
        if connection.generation != expected_generation {
            return Err(ConnectionError::StaleGeneration);
        }
        Ok(SecretKey::for_connection(&connection))
    }

    pub async fn execute(
        &self,
        plan: &RequestPlan,
        consent: &ConsentToken,
    ) -> Result<NetworkResponse, ConnectionError> {
        let network = self.network.clone();
        self.execute_resolved_with(plan, consent, move |plan, secrets| {
            Box::pin(async move { network.send_resolved(plan, secrets).await })
        })
        .await
    }

    /// Preserve transport-native streaming responses without buffering their bodies.
    /// Dispatch must use the exact plan destination/payload; stream consumers use `fence`.
    pub async fn execute_with<T: Send, F>(
        &self,
        plan: &RequestPlan,
        consent: &ConsentToken,
        dispatch: F,
    ) -> Result<T, ConnectionError>
    where
        F: for<'a> FnOnce(&'a RequestPlan, Option<&'a Secret>) -> DispatchFuture<'a, T>,
    {
        self.validate_consent(plan, consent)?;
        if !plan.connection.config.extra_header_names.is_empty() {
            return Err(ConnectionError::NetworkUnavailable);
        }
        self.execute_resolved_with(plan, consent, move |plan, secrets| {
            dispatch(plan, secrets.api_key.as_ref())
        })
        .await
    }

    pub async fn execute_resolved_with<T: Send, F>(
        &self,
        plan: &RequestPlan,
        consent: &ConsentToken,
        dispatch: F,
    ) -> Result<T, ConnectionError>
    where
        F: for<'a> FnOnce(&'a RequestPlan, &'a ResolvedSecrets) -> DispatchFuture<'a, T>,
    {
        self.validate_consent(plan, consent)?;
        self.network.check_available(plan)?;
        if !plan.connection.config.extra_header_names.is_empty()
            && !self.network.supports_extra_headers()
        {
            return Err(ConnectionError::NetworkUnavailable);
        }
        let secrets = self.resolve_secrets(plan, consent).await?;
        self.validate_consent(plan, consent)?;
        let response = dispatch(plan, &secrets).await;
        self.validate_consent(plan, consent)?;
        response
    }

    pub fn fence(
        &self,
        plan: &RequestPlan,
        consent: &ConsentToken,
    ) -> Result<RequestFence, ConnectionError> {
        self.validate_consent(plan, consent)?;
        Ok(RequestFence {
            native: self.clone(),
            plan: plan.clone(),
            consent: consent.clone(),
        })
    }

    fn validate_plan(&self, plan: &RequestPlan) -> Result<(), ConnectionError> {
        if !Arc::ptr_eq(&self.authority, &plan.authority)
            || plan.connection.app_id != *self.repository.app_id()
        {
            return Err(ConnectionError::ConsentDenied);
        }
        let current = self.repository.get(&plan.connection.id)?;
        if current != plan.connection {
            return Err(ConnectionError::StaleGeneration);
        }
        validate_input(&current, &plan.input)
    }

    pub(crate) fn validate_consent(
        &self,
        plan: &RequestPlan,
        consent: &ConsentToken,
    ) -> Result<(), ConnectionError> {
        self.validate_plan(plan)?;
        if !Arc::ptr_eq(&self.authority, &consent.authority) {
            return Err(ConnectionError::ConsentDenied);
        }
        let authority = self
            .authority
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?;
        if !authority
            .approvals
            .get(&consent.id)
            .is_some_and(|approval| approval.covers(plan))
        {
            return Err(ConnectionError::ConsentDenied);
        }
        Ok(())
    }
}

fn validate_destination(base: &url::Url, destination: &url::Url) -> Result<(), ConnectionError> {
    let prefix = format!("{}/", base.path().trim_end_matches('/'));
    if base.origin() != destination.origin()
        || !(base.path() == destination.path() || destination.path().starts_with(&prefix))
        || !destination.username().is_empty()
        || destination.password().is_some()
        || destination.query().is_some()
        || destination.fragment().is_some()
    {
        return Err(ConnectionError::InvalidConfiguration);
    }
    Ok(())
}

fn validate_input(connection: &Connection, input: &PlanInput) -> Result<(), ConnectionError> {
    if !connection.config.enabled {
        return Err(ConnectionError::Disabled);
    }
    if !connection.config.capabilities.contains(&input.capability) {
        return Err(ConnectionError::UnsupportedCapability);
    }
    if matches!(
        input.capability,
        Capability::Probe | Capability::ModelDiscovery
    ) && input.model.is_none()
    {
        return Ok(());
    }
    let model = input
        .model
        .as_ref()
        .and_then(|id| {
            connection
                .config
                .models
                .iter()
                .find(|model| model.id == *id)
        })
        .ok_or(ConnectionError::UnsupportedModel)?;
    if !model.capabilities.contains(&input.capability) {
        return Err(ConnectionError::UnsupportedCapability);
    }
    Ok(())
}
