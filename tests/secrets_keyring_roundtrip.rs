//! Real OS-keyring roundtrip (design doc §10 integration test).
//!
//! Exercises the production `KeyringStore` against a real macOS Keychain /
//! Linux Secret Service — the write path the mock-backed unit tests cannot
//! cover. Runs on the real keychain and cleans up after itself.
//!
//! Compiled only with the `keyring` feature (the backend is opt-in) and
//! guarded so it never runs unintentionally:
//! - `#[ignore]`: excluded from the normal `cargo test` batch.
//! - Runtime `probe_keyring()`: headless/CI boxes without a usable keyring
//!   print `SKIP` and return.
//!
//! Run explicitly on a desktop machine with a logged-in keychain:
//!
//! ```sh
//! cargo test --release --features keyring --test secrets_keyring_roundtrip \
//!   -- --ignored --nocapture
//! ```

#![cfg(feature = "keyring")]

use serial_test::serial;

use syscity::secrets::{probe_keyring, SecretId, SecretOrigin, SecretStoreHandle, StoreRef};

/// One row per design-doc storage namespace: `(namespace, entity, kind)`.
const NAMESPACES: &[(&str, &str, &str)] = &[
    ("llm", "rt-llm-provider", "api_key"),
    ("mcp-env", "rt-mcp-server", "env"),
    ("mcp-oauth", "rt-mcp-oauth", "refresh_token"),
    ("channel", "rt-channel", "access_token"),
    ("webhook", "rt-webhook", "webhook_secret"),
    ("security", "rt-oauth", "client_secret"),
    ("plugin", "rt-plugin", "secret_key"),
];

/// Write → read → delete a unique secret in every namespace, asserting the
/// roundtrip and that deletion clears the entry.
#[tokio::test]
#[serial]
#[ignore]
async fn keyring_roundtrip_per_namespace() {
    if !probe_keyring() {
        eprintln!("SKIP: no usable OS keyring on this machine");
        return;
    }

    let secrets = SecretStoreHandle::new();
    for &(namespace, entity, kind) in NAMESPACES {
        let id = SecretId::new(namespace, entity, kind);
        let value = format!("rt-{namespace}");
        let store = secrets.route(namespace);

        // Clean slate for a repeatable run.
        let _ = store.delete(&id).await;
        assert!(!store.has(&id).await, "{namespace}: should start absent");

        store
            .set(&id, &value, SecretOrigin::SystemGenerated)
            .await
            .unwrap_or_else(|e| panic!("{namespace}: set failed: {e}"));

        assert!(store.has(&id).await, "{namespace}: should exist after set");
        let got = store.get(&id).await.unwrap();
        assert_eq!(
            got.as_deref(),
            Some(value.as_str()),
            "{namespace}: value should roundtrip through the keyring"
        );

        store.delete(&id).await.unwrap();
        assert!(!store.has(&id).await, "{namespace}: should be gone after delete");
    }
}

/// Multi-provider LLM routing: two providers keep independent keyring entries
/// and each resolves to its own key by name.
#[tokio::test]
#[serial]
#[ignore]
async fn llm_provider_keys_route_independently() {
    if !probe_keyring() {
        eprintln!("SKIP: no usable OS keyring on this machine");
        return;
    }

    const PROVIDERS: &[(&str, &str)] = &[("rt-provider-a", "sk-a"), ("rt-provider-b", "sk-b")];
    let secrets = SecretStoreHandle::new();
    let store = secrets.route("llm");

    for (entity, key) in PROVIDERS {
        let id = SecretId::new("llm", entity, "api_key");
        let _ = store.delete(&id).await;
        store
            .set(&id, key, SecretOrigin::SystemGenerated)
            .await
            .unwrap();
    }

    for (entity, key) in PROVIDERS {
        let r = StoreRef::new("llm", entity, "api_key");
        let resolved = secrets.resolve_store_ref(&r).await.unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some(*key),
            "provider '{entity}' should resolve to its own key"
        );
    }

    for (entity, _) in PROVIDERS {
        let _ = store.delete(&SecretId::new("llm", entity, "api_key")).await;
    }
}
