// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Independent test processes share one rotating grant through native Lore auth and its file lock.
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use lore_base::fs::lock::FSLock;
use lore_base::types::RepositoryId;
use lore_credential::token_store;
use lore_credential::token_store::vulnerable_all_tokens;
use lore_transport::auth::AuthEndpoint;
use lore_transport::auth::exchange::auth_exchange;
use lore_transport::auth::oidc;
use lore_transport::auth::oidc::OidcAuthentication;
use lore_transport::types::Oidc;

#[path = "support/oidc_provider.rs"]
mod provider;

use provider::*;

const CLIENT: &str = "lore-refresh-process-test";
const IDENTITY: &str = "process-user";
const DOMAIN: &str = "lore.test.invalid";

static SANDBOX: std::sync::Mutex<Option<lore_base::test_util::TempDir>> =
    std::sync::Mutex::new(None);

#[ctor::ctor]
fn sandbox() {
    let directory = lore_base::test_util::TempDir::new("lore-refresh-process-");
    let store = std::env::var_os("LORE_REFRESH_TEST_STORE")
        .map_or_else(|| directory.path().to_owned(), PathBuf::from);
    // Safety: constructors run before the test harness creates any threads.
    unsafe {
        std::env::set_var("LORE_AUTH_PATH", &store);
        std::env::set_var("LORE_AUTH_STORE", "fallback");
    }
    *SANDBOX.lock().unwrap() = Some(directory);
}

#[ctor::dtor]
fn remove_sandbox() {
    drop(SANDBOX.lock().unwrap().take());
}

fn endpoint(issuer: &str) -> AuthEndpoint {
    AuthEndpoint::new(
        oidc::endpoint(issuer),
        Arc::new(OidcAuthentication::new(Oidc {
            issuer: issuer.to_owned(),
            client_id: CLIENT.to_owned(),
            ..Default::default()
        })),
    )
}

fn token(issuer: &str, expires: u64) -> String {
    jwt(
        &serde_json::json!({"iss": issuer, "sub": IDENTITY, "aud": "lore-api",
        "root_domains": [DOMAIN], "exp": expires, "azp": CLIENT}),
    )
}

/// Invoked only by the parent, with synthetic credentials and an isolated store.
#[tokio::test]
#[ignore = "subprocess helper, executed by processes_reload_the_rotated_grant_under_the_native_lock"]
async fn refresh_child() {
    let issuer = std::env::var("LORE_REFRESH_TEST_ISSUER").unwrap();
    let store = PathBuf::from(std::env::var_os("LORE_REFRESH_TEST_STORE").unwrap());
    let child = std::env::var("LORE_REFRESH_TEST_CHILD").unwrap();
    let endpoint = endpoint(&issuer);
    // Both processes cache the expired grant before the parent admits either renewal.
    let cached =
        token_store::load_user_token_from_store(&endpoint.url, IDENTITY, vulnerable_all_tokens())
            .await
            .unwrap();
    assert_eq!(cached, token(&issuer, 1));
    std::fs::write(store.join(format!("ready-{child}")), []).unwrap();
    wait_until(
        || store.join("go").exists(),
        "the parent to hold the native lock",
    )
    .await;
    assert!(
        FSLock::try_acquire_file_lock(store.join("tokenstore.toml"))
            .unwrap()
            .is_none()
    );
    std::fs::write(store.join(format!("locked-{child}")), []).unwrap();
    let selected = tokio::time::timeout(
        Duration::from_secs(20),
        auth_exchange(&endpoint, DOMAIN, IDENTITY, RepositoryId::default(), "", ""),
    )
    .await
    .unwrap();
    assert_eq!(
        selected,
        (
            token(&issuer, 2_000_000_000),
            String::new(),
            IDENTITY.to_owned()
        )
    );
}

#[tokio::test]
async fn processes_reload_the_rotated_grant_under_the_native_lock() {
    // The processes present the first grant; this process presents the one it rotated to.
    let provider = spawn_recording_provider(|issuer, request, nth| match request.path.as_str() {
        "/token" => {
            assert!(nth < 2, "a one-time grant must be consumed only once");
            let presented = format!("process-refresh-{}", nth + 1);
            assert_eq!(request.form()["refresh_token"], presented);
            json(
                &serde_json::json!({"access_token": token(issuer, 2_000_000_000 + nth as u64),
                "token_type": "Bearer", "refresh_token": format!("process-refresh-{}", nth + 2)}),
            )
        }
        _ => conforming(issuer, nth),
    })
    .await;
    let endpoint = endpoint(&provider.issuer);
    let store = PathBuf::from(std::env::var_os("LORE_AUTH_PATH").unwrap());
    token_store::store_user_token_and_refresh_token(
        &endpoint.url,
        IDENTITY,
        &token(&provider.issuer, 1),
        vec![DOMAIN.to_owned()],
        Some("process-refresh-1"),
        None,
    )
    .await
    .unwrap();
    let executable = std::env::current_exe().unwrap();
    let spawn = |id: &str| {
        Command::new(&executable)
            .args(["--exact", "refresh_child", "--ignored", "--nocapture"])
            .env("LORE_REFRESH_TEST_ISSUER", &provider.issuer)
            .env("LORE_REFRESH_TEST_STORE", &store)
            .env("LORE_REFRESH_TEST_CHILD", id)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap()
    };
    let mut first = Child(spawn("first"));
    let mut second = Child(spawn("second"));
    wait_until(
        || store.join("ready-first").exists() && store.join("ready-second").exists(),
        "both processes to cache the expired grant",
    )
    .await;
    let lock = FSLock::acquire_file_lock(store.join("tokenstore.toml"), false)
        .await
        .unwrap();
    std::fs::write(store.join("go"), []).unwrap();
    wait_until(
        || store.join("locked-first").exists() && store.join("locked-second").exists(),
        "both processes to observe contention",
    )
    .await;
    assert!(provider.requests_to("/token").is_empty());
    drop(lock);
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let first = first.0.try_wait().unwrap();
            let second = second.0.try_wait().unwrap();
            if let (Some(first), Some(second)) = (first, second) {
                assert!(first.success() && second.success());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(provider.requests_to("/token").len(), 1);

    // This process still caches the expired token. Its renewal reloads the store under the
    // lock, finds the token the processes stored, and makes no grant of its own.
    let selected =
        auth_exchange(&endpoint, DOMAIN, IDENTITY, RepositoryId::default(), "", "").await;
    assert_eq!(selected.0, token(&provider.issuer, 2_000_000_000));
    assert_eq!(provider.requests_to("/token").len(), 1);

    // Expiring the stored token again keeps its refresh token, which is the rotated one.
    token_store::store_user_token(
        &endpoint.url,
        IDENTITY,
        &token(&provider.issuer, 1),
        vec![DOMAIN.to_owned()],
    )
    .await
    .unwrap();
    let selected =
        auth_exchange(&endpoint, DOMAIN, IDENTITY, RepositoryId::default(), "", "").await;
    assert_eq!(selected.0, token(&provider.issuer, 2_000_000_001));
    assert_eq!(provider.requests_to("/token").len(), 2);
}

/// Reaps children even if a barrier assertion or timeout fails.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
