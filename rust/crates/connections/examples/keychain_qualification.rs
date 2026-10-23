//! Opt-in synthetic qualification of the real platform credential adapter.
use agentkit_connections::*;
use std::{collections::BTreeSet, future::Future, process::ExitCode, sync::Arc, time::Duration};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const VALUE_A: &str = "AgentKit synthetic qualification value A";
const VALUE_B: &str = "AgentKit synthetic qualification value B";

struct Namespace {
    service: String,
    apps: [String; 2],
    connection: String,
    keys: [SecretKey; 2],
}

struct Failure {
    stage: &'static str,
    reason: &'static str,
    timed_out: bool,
}

fn main() -> ExitCode {
    let namespace = match namespace() {
        Ok(namespace) => namespace,
        Err(_) => {
            eprintln!("Synthetic namespace setup failed; no keychain access attempted.");
            return ExitCode::FAILURE;
        }
    };
    println!("Synthetic keychain service: {}", namespace.service);
    println!("Applications: {}, {}", namespace.apps[0], namespace.apps[1]);
    println!(
        "Connection: {}; generation: 1; purpose: api-key",
        namespace.connection
    );
    let store = match KeychainSecretStore::new(namespace.service.clone()) {
        Ok(store) => store,
        Err(_) => {
            eprintln!("Keychain adapter setup failed; no fallback used.");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("Async runtime setup failed; no keychain access attempted.");
            return ExitCode::FAILURE;
        }
    };
    let passed = runtime.block_on(qualify(&store, &namespace.keys));
    runtime.shutdown_timeout(Duration::from_secs(1));
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn namespace() -> Result<Namespace, ConnectionError> {
    let nonce = uuid::Uuid::new_v4();
    let service = format!("AgentKit.synthetic-keychain-qualification.{nonce}");
    let apps = [
        format!("synthetic-app-a-{nonce}"),
        format!("synthetic-app-b-{nonce}"),
    ];
    let connection = format!("synthetic-connection-{nonce}");
    let repository = ConnectionRepository::default();
    let keys = [
        key(&repository, &apps[0], &connection)?,
        key(&repository, &apps[1], &connection)?,
    ];
    Ok(Namespace {
        service,
        apps,
        connection,
        keys,
    })
}

fn key(
    repository: &ConnectionRepository,
    app: &str,
    connection: &str,
) -> Result<SecretKey, ConnectionError> {
    let app = repository.for_app(AppId::new(app)?);
    let id = ConnectionId::new(connection)?;
    app.create(
        id.clone(),
        ConnectionConfig {
            label: "Synthetic keychain qualification".into(),
            provider_kind: "qualification-no-network".into(),
            endpoint: "https://synthetic-keychain.invalid"
                .parse()
                .map_err(|_| ConnectionError::InvalidConfiguration)?,
            account: Some("synthetic-account".into()),
            credential_required: true,
            extra_header_names: BTreeSet::new(),
            allow_insecure_http: false,
            enabled: true,
            capabilities: BTreeSet::new(),
            models: vec![],
        },
    )?;
    NativeConnections::new(
        app,
        Arc::new(UnavailableSecretStore),
        Arc::new(UnavailableNetwork),
    )
    .credential_key(&id)
}

async fn qualify(store: &KeychainSecretStore, keys: &[SecretKey; 2]) -> bool {
    let mut writes = [false; 2];
    let result = exercise(store, keys, &mut writes).await;
    if let Err(failure) = &result {
        report(failure);
    }
    let cleanup = cleanup(store, keys, writes).await;
    if result.as_ref().is_err_and(|failure| failure.timed_out) {
        eprintln!("An OS call timed out and cannot be cancelled; final cleanup is not guaranteed.");
    }
    let passed = result.is_ok() && cleanup;
    println!(
        "Real keychain qualification: {}",
        if passed { "PASS" } else { "FAIL" }
    );
    println!(
        "Scope: synthetic CRUD and application isolation only; no provider calls or fallback."
    );
    passed
}

async fn exercise(
    store: &KeychainSecretStore,
    keys: &[SecretKey; 2],
    writes: &mut [bool; 2],
) -> Result<(), Failure> {
    missing("app-a initial missing", store, &keys[0]).await?;
    missing("app-b initial missing", store, &keys[1]).await?;
    writes[0] = true;
    checked("app-a put", store.put(&keys[0], Secret::new(VALUE_A))).await?;
    expected("app-a read after put", store, &keys[0], VALUE_A).await?;
    missing("app-b isolated from app-a", store, &keys[1]).await?;
    writes[1] = true;
    checked("app-b put", store.put(&keys[1], Secret::new(VALUE_B))).await?;
    expected("app-a unchanged by app-b", store, &keys[0], VALUE_A).await?;
    expected("app-b read after put", store, &keys[1], VALUE_B).await?;
    checked("app-a delete", store.delete(&keys[0])).await?;
    missing("app-a missing after delete", store, &keys[0]).await?;
    expected("app-b survives app-a delete", store, &keys[1], VALUE_B).await?;
    checked("app-b delete", store.delete(&keys[1])).await?;
    missing("app-b missing after delete", store, &keys[1]).await
}

async fn cleanup(store: &KeychainSecretStore, keys: &[SecretKey; 2], writes: [bool; 2]) -> bool {
    let mut passed = true;
    for index in 0..2 {
        if !writes[index] {
            continue;
        }
        let delete = checked("cleanup delete", store.delete(&keys[index])).await;
        let verify = missing("cleanup missing verification", store, &keys[index]).await;
        for result in [delete, verify] {
            if let Err(failure) = result {
                report(&failure);
                passed = false;
            }
        }
    }
    println!(
        "Cleanup: {}",
        if !writes.iter().any(|written| *written) {
            "no writes attempted; no existing entry deleted"
        } else if passed {
            "synthetic entries verified missing"
        } else {
            "UNCONFIRMED"
        }
    );
    passed
}

async fn missing(
    stage: &'static str,
    store: &KeychainSecretStore,
    key: &SecretKey,
) -> Result<(), Failure> {
    match call(stage, store.get(key)).await? {
        Err(ConnectionError::SecretMissing) => {
            println!("{stage}: OK");
            Ok(())
        }
        Err(error) => Err(adapter_failure(stage, error)),
        Ok(_) => Err(Failure {
            stage,
            reason: "unexpected existing entry; aborting without overwriting it",
            timed_out: false,
        }),
    }
}

async fn expected(
    stage: &'static str,
    store: &KeychainSecretStore,
    key: &SecretKey,
    expected: &str,
) -> Result<(), Failure> {
    let value = checked(stage, store.get(key)).await?;
    if value.expose() == expected {
        Ok(())
    } else {
        Err(Failure {
            stage,
            reason: "synthetic value mismatch",
            timed_out: false,
        })
    }
}

async fn checked<T>(
    stage: &'static str,
    future: impl Future<Output = Result<T, ConnectionError>>,
) -> Result<T, Failure> {
    let value = call(stage, future)
        .await?
        .map_err(|error| adapter_failure(stage, error))?;
    println!("{stage}: OK");
    Ok(value)
}

async fn call<T>(
    stage: &'static str,
    future: impl Future<Output = Result<T, ConnectionError>>,
) -> Result<Result<T, ConnectionError>, Failure> {
    println!("{stage}: begin");
    tokio::time::timeout(OPERATION_TIMEOUT, future)
        .await
        .map_err(|_| Failure {
            stage,
            reason: "OS operation timed out",
            timed_out: true,
        })
}

fn adapter_failure(stage: &'static str, error: ConnectionError) -> Failure {
    let reason = match error {
        ConnectionError::SecretUnavailable => "credential service unavailable or access denied",
        ConnectionError::SecretMissing => "synthetic entry unexpectedly missing",
        _ => "credential adapter rejected the synthetic operation",
    };
    Failure {
        stage,
        reason,
        timed_out: false,
    }
}

fn report(failure: &Failure) {
    eprintln!("{}: FAIL ({})", failure.stage, failure.reason);
}
