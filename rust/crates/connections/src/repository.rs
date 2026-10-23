use crate::{AppId, Connection, ConnectionConfig, ConnectionError, ConnectionId};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

const MAX_GENERATION: u64 = 9_007_199_254_740_991;

fn next_generation(current: u64) -> Result<u64, ConnectionError> {
    current
        .checked_add(1)
        .filter(|next| *next <= MAX_GENERATION)
        .ok_or(ConnectionError::Exhausted)
}

mod mutation;
pub use mutation::RepositoryMutation;

#[derive(Default)]
struct State {
    generation: u64,
    connections: BTreeMap<(AppId, ConnectionId), Connection>,
    next_mutation: u64,
    pending: Option<u64>,
    poisoned: bool,
}

impl State {
    fn ready(&self) -> Result<(), ConnectionError> {
        if self.poisoned {
            Err(ConnectionError::AuthorityPoisoned)
        } else if self.pending.is_some() {
            Err(ConnectionError::MutationPending)
        } else {
            Ok(())
        }
    }
}

/// Hosts persist this metadata transactionally. Credentials and consent are excluded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepositorySnapshot {
    pub version: u32,
    pub last_generation: u64,
    pub connections: Vec<Connection>,
}

/// App-scoped handles share storage, while every operation fixes the app namespace.
#[derive(Clone, Default)]
pub struct ConnectionRepository {
    state: Arc<Mutex<State>>,
}

impl ConnectionRepository {
    pub fn export(&self) -> Result<RepositorySnapshot, ConnectionError> {
        let state = self.state.lock().map_err(|_| ConnectionError::Poisoned)?;
        state.ready()?;
        Ok(RepositorySnapshot {
            version: 1,
            last_generation: state.generation,
            connections: state.connections.values().cloned().collect(),
        })
    }

    /// Restore only into a new authority; existing consent tokens are never restored.
    pub fn restore(snapshot: RepositorySnapshot) -> Result<Self, ConnectionError> {
        if snapshot.version != 1 || snapshot.last_generation > MAX_GENERATION {
            return Err(ConnectionError::InvalidConfiguration);
        }
        let mut state = State {
            generation: snapshot.last_generation,
            ..State::default()
        };
        for connection in snapshot.connections {
            connection.config.validate()?;
            if connection.generation == 0 || connection.generation > state.generation {
                return Err(ConnectionError::InvalidConfiguration);
            }
            let key = (connection.app_id.clone(), connection.id.clone());
            if state.connections.insert(key, connection).is_some() {
                return Err(ConnectionError::InvalidConfiguration);
            }
        }
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
        })
    }

    pub fn for_app(&self, app_id: AppId) -> AppConnections {
        AppConnections {
            app_id,
            repository: self.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AppConnections {
    pub(crate) app_id: AppId,
    repository: ConnectionRepository,
}

impl AppConnections {
    pub fn app_id(&self) -> &AppId {
        &self.app_id
    }

    pub fn export(&self) -> Result<RepositorySnapshot, ConnectionError> {
        let mut snapshot = self.repository.export()?;
        snapshot
            .connections
            .retain(|connection| connection.app_id == self.app_id);
        Ok(snapshot)
    }

    pub fn create(
        &self,
        id: ConnectionId,
        config: ConnectionConfig,
    ) -> Result<Connection, ConnectionError> {
        config.validate()?;
        let mut state = self
            .repository
            .state
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?;
        state.ready()?;
        let key = (self.app_id.clone(), id.clone());
        if state.connections.contains_key(&key) {
            return Err(ConnectionError::AlreadyExists);
        }
        state.generation = next_generation(state.generation)?;
        let connection = Connection {
            app_id: self.app_id.clone(),
            id,
            generation: state.generation,
            config,
        };
        state.connections.insert(key, connection.clone());
        Ok(connection)
    }

    pub fn get(&self, id: &ConnectionId) -> Result<Connection, ConnectionError> {
        let state = self
            .repository
            .state
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?;
        state.ready()?;
        state
            .connections
            .get(&(self.app_id.clone(), id.clone()))
            .cloned()
            .ok_or(ConnectionError::NotFound)
    }

    pub fn list(&self) -> Result<Vec<Connection>, ConnectionError> {
        let state = self
            .repository
            .state
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?;
        state.ready()?;
        Ok(state
            .connections
            .values()
            .filter(|value| value.app_id == self.app_id)
            .cloned()
            .collect())
    }

    pub fn update(
        &self,
        id: &ConnectionId,
        expected_generation: u64,
        config: ConnectionConfig,
    ) -> Result<Connection, ConnectionError> {
        config.validate()?;
        let mut state = self
            .repository
            .state
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?;
        state.ready()?;
        let key = (self.app_id.clone(), id.clone());
        let current = state
            .connections
            .get(&key)
            .ok_or(ConnectionError::NotFound)?;
        if current.generation != expected_generation {
            return Err(ConnectionError::StaleGeneration);
        }
        state.generation = next_generation(state.generation)?;
        let connection = Connection {
            app_id: self.app_id.clone(),
            id: id.clone(),
            generation: state.generation,
            config,
        };
        state.connections.insert(key, connection.clone());
        Ok(connection)
    }

    pub fn delete(
        &self,
        id: &ConnectionId,
        expected_generation: u64,
    ) -> Result<(), ConnectionError> {
        let mut state = self
            .repository
            .state
            .lock()
            .map_err(|_| ConnectionError::Poisoned)?;
        state.ready()?;
        let key = (self.app_id.clone(), id.clone());
        let current = state
            .connections
            .get(&key)
            .ok_or(ConnectionError::NotFound)?;
        if current.generation != expected_generation {
            return Err(ConnectionError::StaleGeneration);
        }
        state.connections.remove(&key);
        Ok(())
    }
}
