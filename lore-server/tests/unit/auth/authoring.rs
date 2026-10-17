// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use http::{Request, Response};
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use lore_base::types::Context as Id;
use lore_server::auth::{
    authoring::*, authoring_lifetime::AuthoringLifetimeService, jwt::AuthorizationToken,
};
use serde_json::json;
use std::{
    convert::Infallible,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};
use tonic::body::Body as TonicBody;
use tower::Service;

const REPO: &str = "0194b726b34e72b0b45550b88a967076";
const BRANCH: &str = "0194b726b34e72b0b45550b88a967077";
const OTHER: &str = "0194b726b34e72b0b45550b88a967078";
fn token() -> AuthorizationToken {
    AuthorizationToken {
        issued_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expires: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60,
        extra: [(
            CLAIM.into(),
            json!({"version":1,"repository":REPO,"branch":BRANCH}),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    }
}
#[test]
fn confines_both_push_apis_and_rejects_mutation_bypasses() {
    let token = token();
    let repo = Id::from_str(REPO).unwrap().into();
    for service in [
        "lore.revision.v1.RevisionService",
        "urc.rpc.RevisionService",
    ] {
        assert!(rpc_access(&token, &format!("/{service}/BranchPush"), Some(repo)).unwrap());
        for method in [
            "BranchCreate",
            "BranchDelete",
            "BranchProtect",
            "BranchMetadataSet",
            "FutureMutation",
        ] {
            assert!(rpc_access(&token, &format!("/{service}/{method}"), Some(repo)).is_err());
        }
    }
    for service in ["lore.storage.v1.StorageService", "urc.rpc.StorageService"] {
        assert!(rpc_access(&token, &format!("/{service}/Put"), Some(repo)).unwrap());
        assert!(!rpc_access(&token, &format!("/{service}/MutableLoad"), Some(repo)).unwrap());
        for method in [
            "MutableStore",
            "MutableCompareAndSwap",
            "PutResolved",
            "Delete",
            "FutureMutation",
        ] {
            assert!(rpc_access(&token, &format!("/{service}/{method}"), Some(repo)).is_err());
        }
        assert!(rpc_access(&token, &format!("/{service}/Put"), None).is_err());
        assert!(
            rpc_access(
                &token,
                &format!("/{service}/Get"),
                Some(Id::from_str(OTHER).unwrap().into())
            )
            .is_err()
        );
    }
    assert!(
        rpc_access(
            &token,
            "/lore.repository.v1.RepositoryService/RepositoryDelete",
            None
        )
        .is_err()
    );
}
#[test]
fn branch_identity_is_signed_and_exact() {
    let mut ext = tonic::Extensions::new();
    ext.insert(token());
    let repo = Id::from_str(REPO).unwrap().into();
    assert!(branch_access(&ext, repo, Id::from_str(BRANCH).unwrap()).is_ok());
    assert!(branch_access(&ext, repo, Id::from_str(OTHER).unwrap()).is_err());
    assert!(
        branch_access(
            &ext,
            Id::from_str(OTHER).unwrap().into(),
            Id::from_str(BRANCH).unwrap()
        )
        .is_err()
    );
}
#[test]
fn malformed_expired_or_service_account_scope_fails_closed() {
    let mut token = token();
    for value in [
        json!(null),
        json!({}),
        json!({"version":2,"repository":REPO,"branch":BRANCH}),
        json!({"version":1,"repository":"*","branch":"*"}),
        json!({"version":1,"repository":REPO,"branch":BRANCH,"extra":true}),
    ] {
        token.extra.insert(CLAIM.into(), value);
        assert!(scope(&token).is_err());
    }
    let mut token = self::token();
    token.is_service_account = Some(true);
    assert!(scope(&token).is_err());
    token.is_service_account = None;
    token.expires = 1;
    assert!(scope(&token).is_err());
}
#[test]
fn ordinary_clients_preserve_their_authorization_contract() {
    let token = AuthorizationToken::default();
    assert!(scope(&token).unwrap().is_none());
    assert!(!rpc_access(&token, "/arbitrary.Service/Mutation", None).unwrap());
}

struct PendingBody;
impl Body for PendingBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Pending
    }
}
#[derive(Clone)]
struct EchoBody;
impl Service<Request<TonicBody>> for EchoBody {
    type Response = Response<TonicBody>;
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Infallible>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, req: Request<TonicBody>) -> Self::Future {
        std::future::ready(Ok(Response::new(req.into_body())))
    }
}
#[tokio::test]
async fn idle_bidirectional_session_ends_at_hard_token_expiry() {
    let mut token = token();
    token.expires = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 1;
    let mut req = Request::new(PendingBody);
    req.extensions_mut().insert(token);
    let response = AuthoringLifetimeService(EchoBody).call(req).await.unwrap();
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        response.into_body().collect(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("Authoring grant expired"));
}
