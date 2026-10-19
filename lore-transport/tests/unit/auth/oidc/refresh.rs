// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Native renewal and caller selection against a synthetic identity provider.
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use lore_base::types::RepositoryId;
use lore_credential::token_store;
use lore_credential::token_store::vulnerable_all_tokens;
use lore_transport::auth::AuthEndpoint;
use lore_transport::auth::exchange::auth_exchange;
use lore_transport::auth::exchange::auth_exchange_custom_resource;
use lore_transport::auth::exchange::exchange;
use lore_transport::auth::exchange::exchange_custom_resource;
use lore_transport::auth::oidc;
use lore_transport::auth::oidc::OidcAuthentication;
use lore_transport::types::Oidc;

use super::provider::*;

const CLIENT: &str = "lore-refresh-test";
const IDENTITY: &str = "refresh-user";
const DOMAIN: &str = "lore.test.invalid";

fn authentication(issuer: &str, client: &str) -> AuthEndpoint {
    AuthEndpoint::new(
        oidc::endpoint(issuer),
        Arc::new(OidcAuthentication::new(Oidc {
            issuer: issuer.to_owned(),
            client_id: client.to_owned(),
            scopes: vec!["must-not-be-requested-again".to_owned()],
            ..Default::default()
        })),
    )
}

fn claims(issuer: &str, expires: u64) -> serde_json::Value {
    serde_json::json!({"iss": issuer, "sub": IDENTITY, "aud": "lore-api",
        "root_domains": [DOMAIN], "exp": expires, "azp": CLIENT})
}

fn response(issuer: &str, refresh: Option<&str>) -> axum::response::Response {
    let mut response = serde_json::json!({"access_token": jwt(&claims(issuer, 2_000_000_000)),
        "token_type": "Bearer"});
    if let Some(refresh) = refresh {
        response["refresh_token"] = refresh.into();
    }
    json(&response)
}

async fn seed(endpoint: &AuthEndpoint, issuer: &str, client: Option<&str>) -> String {
    let expired = jwt(&claims(issuer, 1));
    token_store::store_user_grant(
        &endpoint.url,
        IDENTITY,
        &expired,
        vec![DOMAIN.to_owned()],
        Some("refresh-1"),
        client,
    )
    .await
    .expect("store the synthetic login grant");
    expired
}

async fn selected(endpoint: &AuthEndpoint) -> (String, String, String) {
    auth_exchange(endpoint, DOMAIN, IDENTITY, RepositoryId::default(), "", "").await
}

async fn grant(endpoint: &AuthEndpoint) -> token_store::LockedUserToken {
    token_store::lock_user_token(&endpoint.url, IDENTITY, vulnerable_all_tokens())
        .await
        .expect("lock the stored grant")
}

#[tokio::test]
async fn caller_renewal_rotates_the_pair_and_preserves_an_omitted_replacement() {
    let provider = spawn_recording_provider(|issuer, request, nth| match request.path.as_str() {
        "/token" => {
            let form = request.form();
            assert_eq!(form.len(), 3);
            assert_eq!(form["grant_type"], "refresh_token");
            assert_eq!(form["client_id"], CLIENT);
            assert_eq!(
                form["refresh_token"],
                if nth == 0 { "refresh-1" } else { "refresh-2" }
            );
            response(issuer, (nth == 0).then_some("refresh-2"))
        }
        _ => conforming(issuer, nth),
    })
    .await;
    let endpoint = authentication(&provider.issuer, CLIENT);
    let expired = seed(&endpoint, &provider.issuer, Some(CLIENT)).await;
    let expected = jwt(&claims(&provider.issuer, 2_000_000_000));
    assert_eq!(
        selected(&endpoint).await,
        (expected.clone(), String::new(), IDENTITY.to_owned())
    );
    {
        let stored = grant(&endpoint).await;
        assert_eq!(stored.token(), expected);
        assert_eq!(stored.refresh_token(), Some("refresh-2"));
        assert_eq!(stored.refresh_client_id(), Some(CLIENT));
    }
    token_store::store_user_token(&endpoint.url, IDENTITY, &expired, vec![DOMAIN.to_owned()])
        .await
        .unwrap();
    let repository = "00112233445566778899aabbccddeeff".parse().unwrap();
    assert_eq!(
        exchange(&endpoint, IDENTITY, repository, DOMAIN.to_owned(), "", "")
            .await
            .unwrap(),
        expected
    );
    assert_eq!(grant(&endpoint).await.refresh_token(), Some("refresh-2"));
    token_store::store_user_token(&endpoint.url, IDENTITY, &expired, vec![DOMAIN.to_owned()])
        .await
        .unwrap();
    assert_eq!(
        exchange_custom_resource(
            &endpoint,
            IDENTITY,
            "refresh-resource",
            DOMAIN.to_owned(),
            "",
            ""
        )
        .await
        .unwrap(),
        expected
    );
    let selected =
        auth_exchange_custom_resource(&endpoint, DOMAIN, IDENTITY, "refresh-resource", "", "")
            .await;
    assert_eq!(selected, (expected.clone(), expected, IDENTITY.to_owned()));
    assert_eq!(provider.requests_to("/token").len(), 3);
}

#[tokio::test]
async fn invalid_grant_discards_the_refresh_but_a_transient_failure_retains_it() {
    for terminal in [false, true] {
        let provider = spawn_recording_provider(move |issuer, request, nth| match request.path.as_str() {
            "/token" if terminal => (StatusCode::BAD_REQUEST, json(&serde_json::json!({"error": "invalid_grant", "error_description": "secret-sentinel"}))).into_response(),
            "/token" if nth == 0 => (StatusCode::SERVICE_UNAVAILABLE, "secret-sentinel").into_response(),
            "/token" => response(issuer, Some("refresh-2")),
            _ => conforming(issuer, nth),
        }).await;
        let endpoint = authentication(&provider.issuer, CLIENT);
        let expired = seed(&endpoint, &provider.issuer, Some(CLIENT)).await;
        assert_eq!(
            selected(&endpoint).await,
            (String::new(), String::new(), String::new())
        );
        {
            let stored = grant(&endpoint).await;
            assert_eq!(stored.token(), expired);
            assert_eq!(
                stored.refresh_token(),
                if terminal { None } else { Some("refresh-1") }
            );
        }
        let next = selected(&endpoint).await;
        assert_eq!(next.0.is_empty(), terminal);
        assert_eq!(
            provider.requests_to("/token").len(),
            if terminal { 1 } else { 2 }
        );
    }
}

#[tokio::test]
async fn an_unbound_or_different_client_cannot_consume_the_stored_refresh() {
    let provider = spawn_recording_provider(|issuer, request, nth| match request.path.as_str() {
        "/token" => panic!("a different or unknown client must not consume this refresh"),
        _ => conforming(issuer, nth),
    })
    .await;
    let endpoint = authentication(&provider.issuer, CLIENT);
    for client in [None, Some("another-public-client")] {
        let expired = seed(&endpoint, &provider.issuer, client).await;
        assert!(selected(&endpoint).await.0.is_empty());
        assert_eq!(grant(&endpoint).await.token(), expired);
    }
    assert!(provider.requests_to("/token").is_empty());
}

#[tokio::test]
async fn a_refresh_cannot_change_the_issuer_subject_identity_or_client() {
    for changed in ["iss", "sub", "identity", "azp", "client_id"] {
        let provider = spawn_recording_provider(move |issuer, request, nth| match request.path.as_str() {
            "/token" => {
                let mut token = claims(issuer, 2_000_000_000);
                token[changed] = "another-value".into();
                json(&serde_json::json!({"access_token": jwt(&token), "token_type": "Bearer", "refresh_token": "must-not-store"}))
            }
            _ => conforming(issuer, nth),
        }).await;
        let endpoint = authentication(&provider.issuer, CLIENT);
        let expired = seed(&endpoint, &provider.issuer, Some(CLIENT)).await;
        assert!(selected(&endpoint).await.0.is_empty());
        let stored = grant(&endpoint).await;
        assert_eq!(stored.token(), expired);
        assert_ne!(stored.refresh_token(), Some("must-not-store"));
    }
}

#[tokio::test]
async fn supplied_tokens_are_never_replaced_by_a_stored_refresh() {
    let provider = spawn_recording_provider(|issuer, request, nth| match request.path.as_str() {
        "/token" => panic!("explicit tokens must not trigger stored renewal"),
        _ => conforming(issuer, nth),
    })
    .await;
    let endpoint = authentication(&provider.issuer, CLIENT);
    let expired = seed(&endpoint, &provider.issuer, Some(CLIENT)).await;
    let selected = auth_exchange(
        &endpoint,
        DOMAIN,
        IDENTITY,
        RepositoryId::default(),
        &expired,
        "",
    )
    .await;
    assert_eq!(selected.0, expired);
    let selected =
        auth_exchange_custom_resource(&endpoint, DOMAIN, IDENTITY, "explicit", "", "factory-token")
            .await;
    assert_eq!(selected.0, "factory-token");
    assert_eq!(selected.1, "factory-token");
    assert_eq!(grant(&endpoint).await.refresh_token(), Some("refresh-1"));
    assert!(provider.requests_to("/token").is_empty());
}

#[tokio::test]
async fn renewed_credentials_do_not_inherit_the_old_recipient_grant() {
    let provider = spawn_recording_provider(|issuer, request, nth| match request.path.as_str() {
        "/token" => {
            let mut token = claims(issuer, 2_000_000_000);
            token["root_domains"] = serde_json::json!(["other.test.invalid"]);
            json(&serde_json::json!({"access_token": jwt(&token), "token_type": "Bearer", "refresh_token": "refresh-2"}))
        }
        _ => conforming(issuer, nth),
    }).await;
    let endpoint = authentication(&provider.issuer, CLIENT);
    seed(&endpoint, &provider.issuer, Some(CLIENT)).await;
    assert!(selected(&endpoint).await.0.is_empty());
    assert_eq!(grant(&endpoint).await.refresh_token(), Some("refresh-2"));
    assert!(
        token_store::load_user_token_from_store(
            &endpoint.url,
            IDENTITY,
            token_store::tokens_only_for_recipient_domain(DOMAIN.to_owned())
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn token_endpoint_failures_do_not_return_provider_secrets() {
    const SECRET: &str = "secret-sentinel-do-not-log";
    for case in 0..4 {
        let provider = spawn_recording_provider(move |issuer, request, nth| match request.path.as_str() {
            "/token" => match case {
                0 => json(&serde_json::json!({"access_token": "synthetic", "token_type": "Bearer", "expires_in": SECRET})),
                1 => json(&serde_json::json!({"access_token": "synthetic", "token_type": SECRET})),
                2 => (StatusCode::BAD_REQUEST, json(&serde_json::json!({"error": SECRET, "error_description": SECRET}))).into_response(),
                _ => (StatusCode::SERVICE_UNAVAILABLE, SECRET).into_response(),
            },
            _ => conforming(issuer, nth),
        }).await;
        let endpoint = authentication(&provider.issuer, CLIENT);
        let failure = endpoint
            .auth
            .refresh_authentication(&endpoint.url, "synthetic-refresh", "")
            .await
            .unwrap_err();
        assert!(failure.is_internal());
        assert!(!failure.to_string().contains(SECRET));
    }
    let refusal = oidc::token::ErrorResponse {
        error: SECRET.to_owned(),
        error_description: Some(SECRET.to_owned()),
    };
    assert!(!refusal.describe().contains(SECRET));
}
