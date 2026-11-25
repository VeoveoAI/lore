// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_base::error::Disconnected;
use lore_base::error::NotAuthorized;
use lore_base::error::SlowDown;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_proto::lore::repository::v1::RepositoryListRequest;
use lore_proto::lore::repository::v1::RepositoryTextMetadata;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::authnz::repository_catalog::RepositoryCatalog;
use lore_server::grpc::repository::v1::repository_list::handler;
use tokio_stream::StreamExt;
use tonic::Code;
use tonic::Request;
use tonic::Status;

use crate::store::test_support::FailingLoadStore;
use crate::store::test_support::test_store_create;

struct AuthorizedCatalog(Vec<RepositoryId>);

#[async_trait::async_trait]
impl RepositoryCatalog for AuthorizedCatalog {
    async fn list_repositories(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _size: Option<u32>,
        _page: Option<&str>,
    ) -> Result<(Vec<RepositoryId>, Option<String>), Status> {
        Ok((self.0.clone(), None))
    }
}

fn predicate(text: &str) -> RepositoryTextMetadata {
    RepositoryTextMetadata {
        key: "app.project".into(),
        text: text.into(),
    }
}

#[tokio::test]
async fn filtered_listing_skips_uncreated_grants_and_preserves_existing_results() {
    LORE_CONTEXT
        .scope(
            lore_server::util::setup_execution(module_path!(), "".into(), "".into()),
            async {
                let (immutable, mutable, _) = test_store_create().await.unwrap();
                let mut ids = Vec::new();
                for (index, value) in [
                    Some("true"),
                    Some("TRUE"),
                    Some("true "),
                    None,
                    Some("true"),
                    Some("true"),
                ]
                .iter()
                .enumerate()
                {
                    let id: RepositoryId = Context::from([(index + 1) as u8; 16]).into();
                    ids.push(id);
                    let repository = Arc::new(RepositoryContext::new_server_context(
                        immutable.clone(),
                        mutable.clone(),
                        id,
                    ));
                    let hash = repository::metadata_store(
                        repository.clone(),
                        RepositoryMetadata {
                            name: format!("project-{index}"),
                            default_branch_name: "workspace".into(),
                            creator: "owner".into(),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                    let mut metadata = Metadata::deserialize(repository.clone(), hash)
                        .await
                        .unwrap();
                    if index == 4 {
                        metadata.set_bool("app.project", true).unwrap();
                    } else if let Some(value) = value {
                        metadata.set_string("app.project", value).unwrap();
                    }
                    let hash = metadata.serialize(repository.clone()).await.unwrap();
                    repository::metadata_store_hash(repository, hash)
                        .await
                        .unwrap();
                }
                // The last matching repository is outside this caller's catalog.
                let catalog: Arc<dyn RepositoryCatalog> = Arc::new(AuthorizedCatalog(
                    ids[..5]
                        .iter()
                        .copied()
                        .chain([Context::from([7; 16]).into()])
                        .collect(),
                ));
                let uncreated = Arc::new(RepositoryContext::new_server_context(
                    immutable.clone(),
                    mutable.clone(),
                    Context::from([7; 16]).into(),
                ));
                assert!(
                    repository::metadata_hash(uncreated)
                        .await
                        .unwrap_err()
                        .is_address_not_found()
                );
                let request = RepositoryListRequest {
                    creator: None,
                    text_metadata: Some(predicate("true")),
                };
                let response = handler(
                    Request::new(request.clone()),
                    catalog.clone(),
                    Duration::from_secs(10),
                    immutable.clone(),
                    mutable.clone(),
                )
                .await
                .unwrap();
                assert_eq!(
                    response
                        .metadata()
                        .get("lore-repository-text-filter")
                        .unwrap(),
                    "1"
                );
                let rows = response.into_inner().collect::<Vec<_>>().await;
                assert_eq!(rows.len(), 1);
                let row = rows[0].as_ref().unwrap();
                assert_eq!(
                    hex::encode(&row.repository.as_ref().unwrap().id),
                    ids[0].to_string()
                );
                assert_eq!(
                    row.repository.as_ref().unwrap().default_branch_name,
                    "workspace"
                );
                assert_eq!(row.matched_text_metadata, Some(predicate("true")));

                let response = handler(
                    Request::new(RepositoryListRequest::default()),
                    catalog.clone(),
                    Duration::from_secs(10),
                    immutable.clone(),
                    mutable.clone(),
                )
                .await
                .unwrap();
                assert!(
                    response
                        .metadata()
                        .get("lore-repository-text-filter")
                        .is_none()
                );
                let rows = response.into_inner().collect::<Vec<_>>().await;
                assert_eq!(rows.len(), 5);
                assert!(
                    rows.iter()
                        .all(|row| row.as_ref().unwrap().matched_text_metadata.is_none())
                );

                let response = handler(
                    Request::new(RepositoryListRequest {
                        creator: Some("other-owner".into()),
                        ..request
                    }),
                    catalog,
                    Duration::from_secs(10),
                    immutable,
                    mutable,
                )
                .await
                .unwrap();
                assert_eq!(
                    response
                        .metadata()
                        .get("lore-repository-text-filter")
                        .unwrap(),
                    "1"
                );
                assert!(response.into_inner().collect::<Vec<_>>().await.is_empty());
            },
        )
        .await;
}

#[tokio::test]
async fn existing_pointer_with_missing_or_malformed_metadata_fails_filtered_listing() {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(execution, async {
            let id = Context::from([7; 16]).into();
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable.clone(),
                mutable.clone(),
                id,
            ));
            let mut malformed = Metadata::new();
            malformed.set_string("app.project", "true").unwrap();
            let malformed_hash = malformed.serialize(repository.clone()).await.unwrap();
            let missing_hash = Hash::from([9u8; 32].as_slice());
            assert!(
                Metadata::deserialize(repository.clone(), missing_hash)
                    .await
                    .is_err()
            );
            for hash in [missing_hash, malformed_hash] {
                repository::metadata_store_hash(repository.clone(), hash)
                    .await
                    .unwrap();
                assert_eq!(
                    repository::metadata_hash(repository.clone()).await.unwrap(),
                    hash
                );
                let catalog: Arc<dyn RepositoryCatalog> = Arc::new(AuthorizedCatalog(vec![id]));
                let response = handler(
                    Request::new(RepositoryListRequest {
                        creator: None,
                        text_metadata: Some(predicate("true")),
                    }),
                    catalog.clone(),
                    Duration::from_secs(10),
                    immutable.clone(),
                    mutable.clone(),
                )
                .await
                .unwrap();
                let rows = response.into_inner().collect::<Vec<_>>().await;
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].as_ref().unwrap_err().code(), Code::Unavailable);
                let response = handler(
                    Request::new(RepositoryListRequest::default()),
                    catalog,
                    Duration::from_secs(10),
                    immutable.clone(),
                    mutable.clone(),
                )
                .await
                .unwrap();
                assert!(response.into_inner().collect::<Vec<_>>().await.is_empty());
            }
        })
        .await;
}

#[tokio::test]
async fn pointer_read_failures_are_not_absence_and_backpressure_is_preserved() {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(execution, async {
            let id = Context::from([7; 16]).into();
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable.clone(),
                mutable.clone(),
                id,
            ));
            let hash =
                repository::metadata_store(repository.clone(), RepositoryMetadata::default())
                    .await
                    .unwrap();
            repository::metadata_store_hash(repository, hash)
                .await
                .unwrap();
            for (error, code) in [
                (
                    lore_storage::StoreError::from(NotAuthorized),
                    Code::Unavailable,
                ),
                (
                    lore_storage::StoreError::from(Disconnected),
                    Code::Unavailable,
                ),
                (
                    lore_storage::StoreError::internal("injected pointer read failure"),
                    Code::Unavailable,
                ),
                (
                    lore_storage::StoreError::from(SlowDown),
                    Code::ResourceExhausted,
                ),
            ] {
                let response = handler(
                    Request::new(RepositoryListRequest {
                        creator: None,
                        text_metadata: Some(predicate("true")),
                    }),
                    Arc::new(AuthorizedCatalog(vec![id])),
                    Duration::from_secs(10),
                    immutable.clone(),
                    FailingLoadStore::all(mutable.clone(), error),
                )
                .await
                .unwrap();
                let rows = response.into_inner().collect::<Vec<_>>().await;
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].as_ref().unwrap_err().code(), code);
            }
        })
        .await;
}

#[tokio::test]
async fn filter_bounds_are_checked_and_empty_supported_catalog_is_explicit() {
    let (immutable, mutable, _) = test_store_create().await.unwrap();
    let catalog: Arc<dyn RepositoryCatalog> = Arc::new(AuthorizedCatalog(Vec::new()));
    for filter in [
        RepositoryTextMetadata {
            key: String::new(),
            text: "true".into(),
        },
        RepositoryTextMetadata {
            key: "k".repeat(257),
            text: "true".into(),
        },
        RepositoryTextMetadata {
            key: "k".into(),
            text: "v".repeat(4097),
        },
    ] {
        let response = handler(
            Request::new(RepositoryListRequest {
                creator: None,
                text_metadata: Some(filter),
            }),
            catalog.clone(),
            Duration::from_secs(10),
            immutable.clone(),
            mutable.clone(),
        )
        .await;
        match response {
            Err(status) => assert_eq!(status.code(), Code::InvalidArgument),
            Ok(_) => panic!("invalid predicate accepted"),
        }
    }
    let response = handler(
        Request::new(RepositoryListRequest {
            creator: None,
            text_metadata: Some(predicate("true")),
        }),
        catalog,
        Duration::from_secs(10),
        immutable,
        mutable,
    )
    .await
    .unwrap();
    assert_eq!(
        response
            .metadata()
            .get("lore-repository-text-filter")
            .unwrap(),
        "1"
    );
    assert!(response.into_inner().collect::<Vec<_>>().await.is_empty());
}
