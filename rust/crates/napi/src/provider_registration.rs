use super::*;

static REGISTRATION_GATES: LazyLock<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn registration_gate(connection_id: &str) -> Arc<Mutex<()>> {
    Arc::clone(
        REGISTRATION_GATES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(connection_id.to_owned())
            .or_default(),
    )
}

struct KeyedOptions {
    key: String,
    path: String,
    account: String,
    restore: Option<agentkit_store::ProviderContinuityRestore>,
}

fn parse_options(text: &str) -> Result<KeyedOptions> {
    let options: Value = agentkit_contracts::parse_javascript_json(text)
        .map_err(|_| errors::invalid("provider connection options must be valid JSON"))?;
    let field = |key: &str| -> Result<String> {
        options
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                errors::invalid(format!(
                    "provider connection options.{key} must be a non-empty string"
                ))
            })
    };
    let account = options
        .get("accountScope")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| errors::invalid("accountScope must be a string"))
        })
        .transpose()?
        .unwrap_or("")
        .to_owned();
    let restore = options
        .get("restoreContinuity")
        .map(parse_restore)
        .transpose()?;
    Ok(KeyedOptions {
        key: field("connectionKey")?,
        path: field("storePath")?,
        account,
        restore,
    })
}

fn parse_restore(restore: &Value) -> Result<agentkit_store::ProviderContinuityRestore> {
    let expected = restore["expectedGeneration"].as_u64();
    let previous = restore["previousGeneration"].as_u64();
    if restore["confirmUnchangedScope"] != true || expected.is_none() || previous.is_none() {
        return Err(errors::invalid(
            "continuity requires expectedGeneration, previousGeneration and confirmUnchangedScope",
        ));
    }
    Ok(agentkit_store::ProviderContinuityRestore {
        expected_generation: expected.unwrap(),
        previous_generation: previous.unwrap(),
        legacy: restore["legacy"] == true,
    })
}

/// Identity binds the store namespace, logical key, endpoint and account.
/// Rotation fences existing work; only explicit confirmed restoration transfers
/// compatible settled continuation state. API keys stay inside Rust memory.
#[napi]
pub async fn register_provider_connection(
    base_url: String,
    api_key: Option<String>,
    options_json: Option<String>,
) -> Result<String> {
    if base_url.is_empty() {
        return Err(errors::invalid("baseUrl must be a non-empty string"));
    }
    let connection = Connection {
        base_url,
        api_key: api_key.filter(|key| !key.is_empty()),
        generation: 1,
        store_path: None,
        scope_key: None,
        account_scope: String::new(),
        authority: Arc::new(AtomicBool::new(true)),
    };
    match options_json.as_deref().filter(|text| !text.is_empty()) {
        Some(text) => register_keyed(connection, parse_options(text)?).await,
        None => install(format!("conn_{}", uuid::Uuid::new_v4()), connection).await,
    }
}

async fn register_keyed(mut connection: Connection, options: KeyedOptions) -> Result<String> {
    let path = options.path.clone();
    let canonical = tokio::task::spawn_blocking(move || {
        std::fs::canonicalize(&path).unwrap_or_else(|_| std::path::PathBuf::from(path))
    })
    .await
    .map_err(|_| errors::internal("provider connection registration"))?;
    let gate_key = format!("{}\0{}", canonical.display(), options.key);
    let _gate = registration_gate(&gate_key).lock_owned().await;
    connection.store_path = Some(canonical.clone());
    connection.scope_key = Some(gate_key.clone());
    connection.account_scope = options.account.clone();
    invalidate_changed(&gate_key, &connection).await;
    let (path, secret, endpoint) = (
        canonical,
        connection.api_key.clone(),
        connection.base_url.clone(),
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);
    let (id, generation) = tokio::task::spawn_blocking(move || {
        agentkit_store::register_provider_connection_with_continuity(
            &path,
            &options.key,
            secret.as_deref(),
            &endpoint,
            &options.account,
            now,
            options.restore,
        )
    })
    .await
    .map_err(|_| errors::internal("provider connection registration"))?
    .map_err(errors::store)?;
    connection.generation = generation;
    install(id, connection).await
}

async fn invalidate_changed(scope_key: &str, replacement: &Connection) {
    let connections = CONNECTIONS.lock().await;
    for entry in connections
        .values()
        .filter(|entry| entry.scope_key.as_deref() == Some(scope_key))
    {
        if entry.base_url != replacement.base_url
            || entry.api_key != replacement.api_key
            || entry.account_scope != replacement.account_scope
        {
            entry.authority.store(false, Ordering::Release);
        }
    }
}

async fn install(id: String, mut replacement: Connection) -> Result<String> {
    let mut connections = CONNECTIONS.lock().await;
    for (key, entry) in connections.iter() {
        if key == &id
            && entry.generation == replacement.generation
            && entry.base_url == replacement.base_url
            && entry.authority.load(Ordering::Acquire)
        {
            replacement.authority = entry.authority.clone();
        } else if replacement.scope_key.is_some() && entry.scope_key == replacement.scope_key {
            entry.authority.store(false, Ordering::Release);
        }
    }
    connections.insert(id.clone(), replacement);
    Ok(id)
}

/// Revocation fences existing transports immediately and persists off-thread.
#[napi]
pub async fn revoke_provider_connection(connection_id: String) -> Result<bool> {
    let existing = CONNECTIONS.lock().await.get(&connection_id).cloned();
    let Some(existing) = existing else {
        return Ok(false);
    };
    let gate = existing.scope_key.as_deref().unwrap_or(&connection_id);
    let _gate = registration_gate(gate).lock_owned().await;
    let Some(existing) = CONNECTIONS.lock().await.remove(&connection_id) else {
        return Ok(false);
    };
    existing.authority.store(false, Ordering::Release);
    if let Some(path) = existing.store_path {
        tokio::task::spawn_blocking(move || {
            agentkit_store::revoke_persisted_provider_connection(&path, &connection_id)
        })
        .await
        .map_err(|_| errors::internal("provider connection revocation"))?
        .map_err(errors::store)?;
    }
    Ok(true)
}
