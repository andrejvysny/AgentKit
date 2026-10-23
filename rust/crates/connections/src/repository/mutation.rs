use super::*;
use crate::SecretKey;

type Connections = BTreeMap<(AppId, ConnectionId), Connection>;

enum Change {
    Snapshot,
    Create(ConnectionId, ConnectionConfig),
    Update(ConnectionId, u64, ConnectionConfig),
    Delete(ConnectionId, u64),
}

/// One shared-authority writer reservation. Drop before acknowledged publication
/// poisons the authority because an external durable commit may still complete.
pub struct RepositoryMutation {
    repository: ConnectionRepository,
    token: u64,
    next: Connections,
    snapshot: RepositorySnapshot,
    baseline: RepositorySnapshot,
    connection: Option<Connection>,
    published: bool,
    finished: bool,
}

impl AppConnections {
    /// Reserve an unchanged snapshot for metadata derived from this authority.
    pub fn reserve_snapshot(&self) -> Result<RepositoryMutation, ConnectionError> {
        self.reserve(Change::Snapshot)
    }

    pub fn reserve_create(
        &self,
        id: ConnectionId,
        config: ConnectionConfig,
    ) -> Result<RepositoryMutation, ConnectionError> {
        config.validate()?;
        self.reserve(Change::Create(id, config))
    }
    pub fn reserve_update(
        &self,
        id: &ConnectionId,
        generation: u64,
        config: ConnectionConfig,
    ) -> Result<RepositoryMutation, ConnectionError> {
        config.validate()?;
        self.reserve(Change::Update(id.clone(), generation, config))
    }
    pub fn reserve_delete(
        &self,
        id: &ConnectionId,
        generation: u64,
    ) -> Result<RepositoryMutation, ConnectionError> {
        self.reserve(Change::Delete(id.clone(), generation))
    }

    fn reserve(&self, change: Change) -> Result<RepositoryMutation, ConnectionError> {
        let mut state = self
            .repository
            .state
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?;
        state.ready()?;
        let baseline = snapshot(&state.connections, &self.app_id, state.generation);
        let (next, generation, connection) = prepare(&state, &self.app_id, change)?;
        let token = state
            .next_mutation
            .checked_add(1)
            .ok_or(ConnectionError::Exhausted)?;
        let snapshot = snapshot(&next, &self.app_id, generation);
        state.next_mutation = token;
        state.pending = Some(token);
        Ok(RepositoryMutation {
            repository: self.repository.clone(),
            token,
            next,
            snapshot,
            baseline,
            connection,
            published: false,
            finished: false,
        })
    }
}

fn prepare(
    state: &State,
    app: &AppId,
    change: Change,
) -> Result<(Connections, u64, Option<Connection>), ConnectionError> {
    let mut next = state.connections.clone();
    let (id, expected, config) = match change {
        Change::Snapshot => return Ok((next, state.generation, None)),
        Change::Create(id, config) => (id, None, Some(config)),
        Change::Update(id, generation, config) => (id, Some(generation), Some(config)),
        Change::Delete(id, generation) => (id, Some(generation), None),
    };
    let key = (app.clone(), id.clone());
    if let Some(expected) = expected {
        let current = next.get(&key).ok_or(ConnectionError::NotFound)?;
        if current.generation != expected {
            return Err(ConnectionError::StaleGeneration);
        }
    } else if next.contains_key(&key) {
        return Err(ConnectionError::AlreadyExists);
    }
    let Some(config) = config else {
        next.remove(&key);
        return Ok((next, state.generation, None));
    };
    let generation = next_generation(state.generation)?;
    let connection = Connection {
        app_id: app.clone(),
        id,
        generation,
        config,
    };
    next.insert(key, connection.clone());
    Ok((next, generation, Some(connection)))
}

fn snapshot(connections: &Connections, app: &AppId, generation: u64) -> RepositorySnapshot {
    RepositorySnapshot {
        version: 1,
        last_generation: generation,
        connections: connections
            .values()
            .filter(|connection| &connection.app_id == app)
            .cloned()
            .collect(),
    }
}

impl RepositoryMutation {
    pub fn baseline(&self) -> &RepositorySnapshot {
        &self.baseline
    }
    pub fn snapshot(&self) -> &RepositorySnapshot {
        &self.snapshot
    }
    pub fn connection(&self) -> Option<&Connection> {
        self.connection.as_ref()
    }
    pub fn credential_key(&self) -> Option<SecretKey> {
        self.connection.as_ref().map(SecretKey::for_connection)
    }
    pub fn header_key(&self) -> Option<SecretKey> {
        self.credential_key().map(SecretKey::for_headers)
    }

    /// Call only after an acknowledged durable commit. The reservation remains
    /// active until secret provisioning and retirement finish.
    pub fn publish(&mut self) -> Result<(), ConnectionError> {
        let mut state = self
            .repository
            .state
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?;
        self.check(&state)?;
        if self.published {
            return Err(ConnectionError::InvalidConfiguration);
        }
        state.connections = std::mem::take(&mut self.next);
        state.generation = self.snapshot.last_generation;
        self.published = true;
        Ok(())
    }

    /// A known rolled-back transaction can release without poisoning authority.
    pub fn abort(mut self) -> Result<(), ConnectionError> {
        if self.published {
            return Err(ConnectionError::InvalidConfiguration);
        }
        self.release()
    }

    pub fn finish(mut self) -> Result<(), ConnectionError> {
        if !self.published {
            return Err(ConnectionError::InvalidConfiguration);
        }
        self.release()
    }

    fn release(&mut self) -> Result<(), ConnectionError> {
        let mut state = self
            .repository
            .state
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?;
        self.check(&state)?;
        state.pending = None;
        self.finished = true;
        Ok(())
    }

    fn check(&self, state: &State) -> Result<(), ConnectionError> {
        if state.poisoned {
            Err(ConnectionError::AuthorityPoisoned)
        } else if state.pending != Some(self.token) {
            Err(ConnectionError::MutationPending)
        } else {
            Ok(())
        }
    }
}

impl Drop for RepositoryMutation {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Ok(mut state) = self.repository.state.lock() {
            if state.pending != Some(self.token) || !self.published {
                state.poisoned = true;
            }
            state.pending = None;
        }
    }
}
