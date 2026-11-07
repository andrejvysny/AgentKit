mod common;
use agentkit_connections::*;
use common::*;
use std::sync::Arc;

#[test]
fn vault_references_survive_restore_and_bind_app_generation_account_and_purpose() {
    let repository = ConnectionRepository::default();
    let first = app(&repository, "first");
    let created = first.create(id(), config()).unwrap();
    let native = NativeConnections::new(
        first.clone(),
        Arc::new(MemorySecretStore::default()),
        Arc::new(UnavailableNetwork),
    );
    let reference = native.credential_key(&id()).unwrap().reference();
    assert!(reference.starts_with("agentkit:secret:v1:"));
    assert_ne!(reference, native.header_key(&id()).unwrap().reference());
    let restored = ConnectionRepository::restore(repository.export().unwrap()).unwrap();
    let restored = NativeConnections::new(
        app(&restored, "first"),
        Arc::new(MemorySecretStore::default()),
        Arc::new(UnavailableNetwork),
    );
    assert_eq!(
        reference,
        restored.credential_key(&id()).unwrap().reference()
    );
    first.update(&id(), created.generation, config()).unwrap();
    assert_ne!(reference, native.credential_key(&id()).unwrap().reference());
    let other = app(&repository, "other");
    other.create(id(), config()).unwrap();
    let other = NativeConnections::new(
        other,
        Arc::new(MemorySecretStore::default()),
        Arc::new(UnavailableNetwork),
    );
    assert_ne!(reference, other.credential_key(&id()).unwrap().reference());
}
