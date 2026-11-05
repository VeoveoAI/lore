// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Add;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum_test::TestServer;
use jsonwebtoken::Algorithm;
use jsonwebtoken::DecodingKey;
use jsonwebtoken::EncodingKey;
use jsonwebtoken::Header;
use jsonwebtoken::encode;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_revision::fragment;
use lore_server::auth::jwk::JWKService;
use lore_server::auth::jwk::JWKServiceError;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::JwtVerifier;
use lore_server::auth::jwt::ResourcePermission;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;
use lore_server::http::server::LoreHttpServerSettings;
use lore_server::http::server::ServerHealth;
use lore_server::http::server::ServerState;
use lore_server::http::server::create_router;
use rand::random;

use crate::store::test_support::test_store_create;

#[tokio::test]
async fn test_server_is_up_and_listening() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            // Create the server and test the request
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: None,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };

            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();

            let response = test_server.get("/does-not-exist").expect_failure().await;

            assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
        })
        .await;
}

#[tokio::test]
async fn test_address_in_wrong_format() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: None,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();

            // Create the server and test the request
            let non_existing_address = "/v1/repository/fffff/content/fffff-ff"; // Wrong lengths
            let response = test_server.get(non_existing_address).expect_failure().await;

            assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
        })
        .await;
}

#[tokio::test]
async fn test_address_not_found() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: None,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();

            // Create the server and test the request
            let non_existing_address = "/v1/repository/ffffffffffffffffffffffffffffffff/content/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";
            let response = test_server.get(non_existing_address).expect_failure().await;

            assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
            assert_eq!(response.text(), "address not found");
        }).await;
}

#[tokio::test]
async fn test_address_returned_correctly() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            // Create a fragment on a repository in the store
            let repository = random::<Context>();
            let (fragment, address, payload) = fragment::generate_random();

            immutable_store
                .clone()
                .put(
                    repository.into(),
                    address,
                    fragment,
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("Failed to put data in immutable store");

            // Create the server and test the request
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: None,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();
            let valid_url = format!("/v1/repository/{repository}/content/{address}");

            let response = test_server.get(valid_url.as_str()).await;

            assert_eq!(response.status_code(), StatusCode::OK);
        })
        .await;
}

#[tokio::test]
async fn test_address_returned_correctly_with_headers() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            // Create a fragment on a repository in the store
            let repository = random::<Context>();
            let (fragment, address, payload) = fragment::generate_random();

            immutable_store
                .clone()
                .put(
                    repository.into(),
                    address,
                    fragment,
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("Failed to put data in immutable store");

            // Create the server and test the request
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: None,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();
            let valid_url = format!("/v1/repository/{repository}/content/{address}");

            let response = test_server
                .get(valid_url.as_str())
                .add_query_param("content_type", "image/png")
                .add_query_param("content_encoding", "gzip")
                .add_query_param("content_disposition", "inline")
                .await;

            assert_eq!(response.status_code(), StatusCode::OK);
            assert_eq!(
                response.headers().get("content-type").unwrap(),
                HeaderValue::from_static("image/png")
            );
            assert_eq!(
                response.headers().get("content-encoding").unwrap(),
                HeaderValue::from_static("gzip")
            );
            assert_eq!(
                response.headers().get("content-disposition").unwrap(),
                HeaderValue::from_static("inline")
            );
        })
        .await;
}

#[tokio::test]
async fn test_address_works_with_jwt_verifier_and_good_token() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            // Create a fragment on a repository in the store
            let repository = random::<Context>();
            let (fragment, address, payload) = fragment::generate_random();

            immutable_store
                .clone()
                .put(
                    repository.into(),
                    address,
                    fragment,
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("Failed to put data in immutable store");

            // Create the server and test the request
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: None,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();
            let valid_url = format!("/v1/repository/{repository}/content/{address}");

            // Create a valid token, with a kid (which is what's checked for now, so it breaks if/when we check further)
            let auth_header = HeaderName::from_static("authorization");
            let jwt_header = Header {
                kid: Some("a_kid".to_owned()),
                ..Default::default()
            };

            let expiration = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .add(Duration::from_secs(60))
                .as_secs();

            let jwt_claims = AuthorizationToken {
                user_id: "qweasdzxc123".to_string(),
                issuer: "test".to_string(),
                issued_at: 123456700,
                audience: vec!["Lore".to_string()],
                env: Some("DEV".to_string()),
                name: Some("test".to_string()),
                preferred_username: Some("test".to_string()),
                client_id: None,
                resources: None,
                groups: None,
                is_service_account: Some(false),
                expires: expiration,
                idp: Some("test".to_string()),
                extra: Default::default(),
                identity: None,
            };
            let jwt_key = EncodingKey::from_secret("test-secret".as_ref());
            let bearer = encode(&jwt_header, &jwt_claims, &jwt_key).unwrap();
            let bearer_header_string = format!("Bearer {bearer}");
            let bearer_header = HeaderValue::from_str(bearer_header_string.as_str()).unwrap();
            let response = test_server
                .get(valid_url.as_str())
                .add_header(auth_header, bearer_header)
                .await;

            assert_eq!(response.status_code(), StatusCode::OK);
        })
        .await;
}

/// A server with `payload` stored under one repository, no verifier and no authorizer in the way.
async fn range_server(payload: &[u8], check: impl AsyncFnOnce(TestServer, String)) {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<Context>();
            let (fragment, address, stored) = stored_fragment(payload);
            immutable_store
                .clone()
                .put(repository.into(), address, fragment, Some(stored), false)
                .await
                .expect("Failed to put data in immutable store");
            let health = ServerHealth::new_without_availability(immutable_store.clone());
            let state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: None,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let server = TestServer::new(create_router(state, health, &settings)).unwrap();
            check(
                server,
                format!("/v1/repository/{repository}/content/{address}"),
            )
            .await;
        })
        .await;
}

fn stored_fragment(
    payload: &[u8],
) -> (
    lore_base::types::Fragment,
    lore_base::types::Address,
    bytes::Bytes,
) {
    let (mut fragment, mut address, _random) = fragment::generate_random();
    address.hash = lore_base::types::Hash::hash_buffer(payload);
    fragment.size_payload = u32::try_from(payload.len()).unwrap();
    fragment.size_content = payload.len() as u64;
    (fragment, address, bytes::Bytes::copy_from_slice(payload))
}

const RANGE: HeaderName = HeaderName::from_static("range");
const IF_RANGE: HeaderName = HeaderName::from_static("if-range");

/// A byte range answers 206 with exactly those bytes and says where they sit.
#[tokio::test]
async fn a_range_answers_those_bytes() {
    let payload: Vec<u8> = (0..=255).collect();
    let expected = payload.clone();
    range_server(&payload, async move |server, url| {
        for (range, slice, content_range) in [
            ("bytes=0-9", 0..10, "bytes 0-9/256"),
            ("bytes=100-199", 100..200, "bytes 100-199/256"),
            ("bytes=250-", 250..256, "bytes 250-255/256"),
            ("bytes=-6", 250..256, "bytes 250-255/256"),
            ("bytes=250-9000", 250..256, "bytes 250-255/256"),
            ("bytes=7-7", 7..8, "bytes 7-7/256"),
        ] {
            let response = server
                .get(&url)
                .add_header(RANGE, HeaderValue::from_static(range))
                .await;
            assert_eq!(
                response.status_code(),
                StatusCode::PARTIAL_CONTENT,
                "{range}"
            );
            assert_eq!(
                response.as_bytes().as_ref(),
                &expected[slice.clone()],
                "{range}"
            );
            assert_eq!(
                response.headers()["content-range"],
                content_range,
                "{range}"
            );
            assert_eq!(response.headers()["accept-ranges"], "bytes", "{range}");
            assert_eq!(
                response.headers()["content-length"],
                slice.len().to_string().as_str(),
                "{range}"
            );
        }
    })
    .await;
}

/// Without a range, or with one it cannot use, the route serves the whole content.
#[tokio::test]
async fn no_usable_range_serves_the_whole_content() {
    let payload: Vec<u8> = (0..=255).collect();
    let expected = payload.clone();
    range_server(&payload, async move |server, url| {
        let response = server.get(&url).await;
        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(response.as_bytes().as_ref(), expected.as_slice());
        assert_eq!(response.headers()["accept-ranges"], "bytes");
        assert!(response.headers().get("content-range").is_none());

        for range in ["bytes=0-1,4-5", "bytes=9-3", "items=0-1", "nonsense"] {
            let response = server
                .get(&url)
                .add_header(RANGE, HeaderValue::from_static(range))
                .await;
            assert_eq!(response.status_code(), StatusCode::OK, "{range}");
            assert_eq!(response.as_bytes().as_ref(), expected.as_slice(), "{range}");
        }

        // This route sends no validator, so `If-Range` cannot be shown to match.
        let response = server
            .get(&url)
            .add_header(RANGE, HeaderValue::from_static("bytes=0-9"))
            .add_header(IF_RANGE, HeaderValue::from_static("\"anything\""))
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(response.as_bytes().as_ref(), expected.as_slice());
    })
    .await;
}

#[tokio::test]
async fn a_range_past_the_content_answers_416() {
    let payload: Vec<u8> = (0..=255).collect();
    range_server(&payload, async move |server, url| {
        for range in ["bytes=256-", "bytes=1000-2000", "bytes=-0"] {
            let response = server
                .get(&url)
                .add_header(RANGE, HeaderValue::from_static(range))
                .await;
            assert_eq!(
                response.status_code(),
                StatusCode::RANGE_NOT_SATISFIABLE,
                "{range}"
            );
            assert_eq!(
                response.headers()["content-range"],
                "bytes */256",
                "{range}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn a_range_of_an_address_that_is_not_there_answers_404() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let health = ServerHealth::new_without_availability(immutable_store.clone());
            let state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: None,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let server = TestServer::new(create_router(state, health, &settings)).unwrap();
            let (_fragment, address, _payload) = fragment::generate_random();
            let repository = random::<Context>();
            let response = server
                .get(&format!("/v1/repository/{repository}/content/{address}"))
                .add_header(RANGE, HeaderValue::from_static("bytes=0-9"))
                .expect_failure()
                .await;
            assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
        })
        .await;
}

mockall::mock! {
    KeySource {}

    #[async_trait::async_trait]
    impl JWKService for KeySource {
        async fn get_key(&self, kid: &str) -> Result<(DecodingKey, Algorithm), JWKServiceError>;
        fn get_cached_key(&self, kid: &str) -> Option<(DecodingKey, Algorithm)>;
        async fn refresh_key(&self, kid: &str) -> Result<Option<(DecodingKey, Algorithm)>, JWKServiceError>;
    }
}

/// A token as the Hub issues one: signed, with the one repository it grants in its `resources`.
fn token_for(repository: Option<Context>) -> HeaderValue {
    let claims = AuthorizationToken {
        user_id: "a-person".to_string(),
        issuer: "test".to_string(),
        audience: vec!["lore".to_string()],
        expires: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .add(Duration::from_secs(60))
            .as_secs(),
        resources: repository.map(|repository| {
            vec![ResourcePermission {
                resource_id: format!("urc-{repository}"),
                permission: vec!["read".to_string()],
            }]
        }),
        ..Default::default()
    };
    let header = Header {
        kid: Some("a_kid".to_owned()),
        alg: Algorithm::HS256,
        ..Default::default()
    };
    let token = encode(&header, &claims, &EncodingKey::from_secret(b"test-secret")).unwrap();
    HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
}

/// The content route under the authorizer an installation with `resource_claim` selects: a token
/// reads the repository it names and no other, even an address that is there, and a range changes
/// nothing about that.
#[tokio::test]
async fn a_token_for_one_repository_is_refused_on_another() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let granted = random::<Context>();
            let other = random::<Context>();
            let (fragment, address, payload) = fragment::generate_random();
            for repository in [granted, other] {
                immutable_store
                    .clone()
                    .put(
                        repository.into(),
                        address,
                        fragment,
                        Some(payload.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to put data in immutable store");
            }

            let mut keys = MockKeySource::new();
            keys.expect_get_key()
                .returning(|_| Ok((DecodingKey::from_secret(b"test-secret"), Algorithm::HS256)));
            // A signature that fails asks once whether the key has rotated: it has not.
            keys.expect_refresh_key().returning(|_| Ok(None));
            let health = ServerHealth::new_without_availability(immutable_store.clone());
            let state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: Some(JwtVerifier {
                    jwk_service: Arc::new(keys),
                    jwt_issuer: None,
                    jwt_audience: Some(vec!["lore".to_string()]),
                    jwt_typ: None,
                }),
                repository_authorizer: Arc::new(ResourceGrantsAuthorizer::new(
                    "resources".to_string(),
                    "resource_id".to_string(),
                    Some("permission".to_string()),
                    "urc-{id}".to_string(),
                    "urc-*".to_string(),
                )),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let server = TestServer::new(create_router(state, health, &settings)).unwrap();
            let authorization = HeaderName::from_static("authorization");
            let url =
                |repository: Context| format!("/v1/repository/{repository}/content/{address}");

            let response = server
                .get(&url(granted))
                .add_header(authorization.clone(), token_for(Some(granted)))
                .await;
            assert_eq!(response.status_code(), StatusCode::OK);
            assert_eq!(response.as_bytes().as_ref(), payload.as_ref());

            let response = server
                .get(&url(other))
                .add_header(authorization.clone(), token_for(Some(granted)))
                .expect_failure()
                .await;
            assert_eq!(response.status_code(), StatusCode::FORBIDDEN);
            assert!(response.as_bytes().is_empty());

            let response = server
                .get(&url(other))
                .add_header(authorization.clone(), token_for(Some(granted)))
                .add_header(RANGE, HeaderValue::from_static("bytes=0-9"))
                .expect_failure()
                .await;
            assert_eq!(response.status_code(), StatusCode::FORBIDDEN);

            // A signed token that names no repository reads none.
            let response = server
                .get(&url(granted))
                .add_header(authorization.clone(), token_for(None))
                .expect_failure()
                .await;
            assert_eq!(response.status_code(), StatusCode::FORBIDDEN);

            // A token that is not signed by the issuer's key is refused too.
            let forged = encode(
                &Header {
                    kid: Some("a_kid".to_owned()),
                    alg: Algorithm::HS256,
                    ..Default::default()
                },
                &AuthorizationToken {
                    user_id: "a-person".to_string(),
                    audience: vec!["lore".to_string()],
                    expires: u64::MAX / 2,
                    resources: Some(vec![ResourcePermission {
                        resource_id: format!("urc-{granted}"),
                        permission: vec!["read".to_string()],
                    }]),
                    ..Default::default()
                },
                &EncodingKey::from_secret(b"another-secret"),
            )
            .unwrap();
            let response = server
                .get(&url(granted))
                .add_header(
                    authorization.clone(),
                    HeaderValue::from_str(&format!("Bearer {forged}")).unwrap(),
                )
                .expect_failure()
                .await;
            assert_eq!(response.status_code(), StatusCode::FORBIDDEN);

            let response = server.get(&url(granted)).expect_failure().await;
            assert_eq!(response.status_code(), StatusCode::UNAUTHORIZED);
        })
        .await;
}
