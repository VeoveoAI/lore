// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! This whole-file replacement fixture needs its own process and credential store.
use lore_credential::token_store::load_user_token_from_store;
use lore_credential::token_store::remove_user_token_if;
use lore_credential::token_store::store_user_token;
use lore_credential::token_store::test_util::store_file;
use lore_credential::token_store::vulnerable_all_tokens;

static SANDBOX: std::sync::Mutex<Option<lore_base::test_util::TempDir>> =
    std::sync::Mutex::new(None);

#[ctor::ctor]
fn sandbox() {
    let directory = lore_base::test_util::TempDir::new("lore-credential-conditional-removal-");
    // Safety: constructors run before the test harness creates any threads.
    unsafe {
        std::env::set_var("LORE_AUTH_PATH", directory.path());
        std::env::set_var("LORE_AUTH_STORE", "fallback");
    }
    *SANDBOX.lock().unwrap() = Some(directory);
}

#[ctor::dtor]
fn remove_sandbox() {
    drop(SANDBOX.lock().unwrap().take());
}

/// A token removed only where it is a given one is compared with the store as its file holds it,
/// so a token another process stored in its place after this process cached the store is left
/// alone, and the given one is removed. The test stands in for the other process by writing the
/// store's file back as it was before this process stored the given token.
#[tokio::test]
async fn a_conditional_removal_leaves_a_token_another_process_stored() {
    const ENDPOINT: &str = "https://auth.conditional-removal.test.invalid/repository";
    let file = store_file().expect("the token store's file");
    store_user_token(ENDPOINT, "alice", "newer", Vec::new())
        .await
        .expect("store the newer token");
    let newer = std::fs::read(&file).expect("read the store holding the newer token");
    store_user_token(ENDPOINT, "alice", "refused", Vec::new())
        .await
        .expect("store the refused token");
    std::fs::write(&file, newer).expect("store the newer token from another process");

    remove_user_token_if(ENDPOINT, "alice", "refused")
        .await
        .expect("remove the refused token");
    let kept = load_user_token_from_store(ENDPOINT, "alice", vulnerable_all_tokens()).await;
    assert_eq!(kept.ok().as_deref(), Some("newer"));

    remove_user_token_if(ENDPOINT, "alice", "newer")
        .await
        .expect("remove the newer token");
    let removed = load_user_token_from_store(ENDPOINT, "alice", vulnerable_all_tokens()).await;
    assert!(removed.is_err(), "the given token was left in the store");
}
