// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::lore::repository::v1::{RepositoryListRequest, RepositoryTextMetadata};
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::repository::{self, RepositoryContext, RepositoryMetadata};
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::authnz::repository_catalog::RepositoryCatalog;
use lore_server::grpc::repository::v1::repository_list::handler;
use tokio_stream::StreamExt;
use tonic::{Code, Request, Status};

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
async fn exact_text_filter_preserves_authorized_subset_type_default_and_plain_listing() {
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
                let catalog: Arc<dyn RepositoryCatalog> =
                    Arc::new(AuthorizedCatalog(ids[..5].to_vec()));
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
async fn unreadable_filtered_metadata_is_not_an_empty_result_and_plain_skip_is_preserved() {
    let (immutable, mutable, _) = test_store_create().await.unwrap();
    let catalog: Arc<dyn RepositoryCatalog> =
        Arc::new(AuthorizedCatalog(vec![Context::from([7; 16]).into()]));
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
        immutable,
        mutable,
    )
    .await
    .unwrap();
    assert!(response.into_inner().collect::<Vec<_>>().await.is_empty());
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
