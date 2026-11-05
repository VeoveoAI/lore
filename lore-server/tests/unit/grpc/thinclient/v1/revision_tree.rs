// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::thin_client::v1 as thin_client_v1;
use lore_proto::lore::thin_client::v1::RevisionTreeRequest;
use lore_proto::lore::thin_client::v1::RevisionTreeResponse;
use lore_proto::lore::thin_client::v1::revision_tree_request::Query;
use lore_proto::lore::thin_client::v1::revision_tree_response::Payload;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::link::LinkFlags;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RawToken;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::server::RevisionListAcceleration;
use lore_server::grpc::thinclient::v1::revision_tree::*;
use lore_storage::hash::hash_string;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tokio_stream::StreamExt;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::store::test_support::test_store_create;

fn allow_all() -> Arc<dyn RepositoryAuthorizer> {
    Arc::new(AllowAllRepositoryAuthorizer)
}

/// A Tier 2 deployment reading the legacy `resources` claim shape, so
/// the link-read verdict comes from the token in the request.
fn resource_grants() -> Arc<dyn RepositoryAuthorizer> {
    Arc::new(ResourceGrantsAuthorizer::new(
        "resources".to_string(),
        "resource_id".to_string(),
        None,
        "urc-{id}".to_string(),
        "urc-*".to_string(),
    ))
}

fn make_request(
    repository: RepositoryId,
    query: Query,
    path_prefix: Option<String>,
    max_depth: Option<u32>,
) -> Request<RevisionTreeRequest> {
    let mut request = Request::new(RevisionTreeRequest {
        query: Some(query),
        path_prefix,
        max_depth,
    });
    request.metadata_mut().insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
    );
    request
}

/// Pushes a fresh branch and one revision for each of `revisions`; each
/// revision's state contains `revisions[i]` as File nodes at the root.
/// Returns the branch id and the revision signatures in push order.
async fn push_branch_with_revisions(
    repository: &Arc<RepositoryContext>,
    revisions: &[&[&str]],
) -> (BranchId, Vec<Hash>) {
    let write_token = get_write_token();
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    branch::create(
        repository.clone(),
        &write_token,
        branch_id,
        "test-branch",
        branch::default_category(),
        "creator",
        1,
        vec![],
        false,
        false,
    )
    .await
    .expect("create branch");

    let mut signatures = Vec::with_capacity(revisions.len());
    let mut parent = Hash::default();
    for (idx, files) in revisions.iter().enumerate() {
        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(repository.clone())
            .await
            .expect("serialize metadata");

        let state = state::State::new();
        state.set_parent_self(parent);
        state.set_revision_number((idx + 1) as u64);
        state.set_metadata_hash(metadata_hash);
        for name in *files {
            let node = Node {
                flags: NodeFlags::File.bits(),
                name_hash: hash_string(name),
                ..Default::default()
            };
            state
                .node_add(repository.clone(), ROOT_NODE, node, name)
                .await
                .expect("node_add");
        }
        let serialized = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize state");
        let signature = branch_push::push(
            repository.clone(),
            branch_id,
            serialized,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("push")
        .revision;
        signatures.push(signature);
        parent = signature;
    }
    (branch_id, signatures)
}

/// Push a branch with one revision laying out a small tree:
/// `top.txt` and `subdir/inner.txt`.
async fn push_branch_with_subdir(repository: &Arc<RepositoryContext>) -> (BranchId, Hash) {
    let write_token = get_write_token();
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    branch::create(
        repository.clone(),
        &write_token,
        branch_id,
        "test-branch",
        branch::default_category(),
        "creator",
        1,
        vec![],
        false,
        false,
    )
    .await
    .expect("create branch");

    let mut metadata = Metadata::new();
    metadata.set_branch(branch_id).expect("set branch");
    let metadata_hash = metadata
        .serialize(repository.clone())
        .await
        .expect("serialize metadata");

    let state = state::State::new();
    state.set_parent_self(Hash::default());
    state.set_revision_number(1);
    state.set_metadata_hash(metadata_hash);

    // Top-level file.
    let top = Node {
        flags: NodeFlags::File.bits(),
        name_hash: hash_string("top.txt"),
        ..Default::default()
    };
    state
        .node_add(repository.clone(), ROOT_NODE, top, "top.txt")
        .await
        .expect("node_add top.txt");

    // Top-level directory (no File/Link bits = directory).
    let subdir_node = Node {
        flags: NodeFlags::NoFlags.bits(),
        name_hash: hash_string("subdir"),
        ..Default::default()
    };
    let subdir_id = state
        .node_add(repository.clone(), ROOT_NODE, subdir_node, "subdir")
        .await
        .expect("node_add subdir");

    // File inside the subdirectory.
    let inner = Node {
        flags: NodeFlags::File.bits(),
        name_hash: hash_string("inner.txt"),
        ..Default::default()
    };
    state
        .node_add(repository.clone(), subdir_id, inner, "inner.txt")
        .await
        .expect("node_add inner.txt");

    let serialized = state
        .serialize(repository.clone(), &write_token)
        .await
        .expect("serialize state");
    let signature = branch_push::push(
        repository.clone(),
        branch_id,
        serialized,
        crate::util::test_support::PUSH_ANY,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        lore_server::grpc::server::RevisionListAcceleration::default(),
    )
    .await
    .expect("push")
    .revision;
    (branch_id, signature)
}

/// Pushes a fresh branch and one revision with `file_names` as File
/// nodes at the root. Returns the branch id and the revision
/// signature. Convenience wrapper around `push_branch_with_revisions`
/// for tests that don't care about history depth.
async fn push_branch_with_files(
    repository: &Arc<RepositoryContext>,
    file_names: &[&str],
) -> (BranchId, Hash) {
    let (branch_id, signatures) = push_branch_with_revisions(repository, &[file_names]).await;
    (branch_id, signatures[0])
}

/// `link_branch` registers a link reference for the mount node: zero for a
/// link following its parent's branch, an explicit id for a pinned one.
/// `None` leaves the reference unregistered, as an unresolvable link.
async fn push_branch_with_link(
    repository: &Arc<RepositoryContext>,
    link_name: &str,
    target_repo: RepositoryId,
    target_revision: Hash,
    target_node: lore_revision::node::NodeID,
    link_branch: Option<BranchId>,
) -> (BranchId, Hash) {
    use lore_base::types::Address;

    let write_token = get_write_token();
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    branch::create(
        repository.clone(),
        &write_token,
        branch_id,
        "test-branch",
        branch::default_category(),
        "creator",
        1,
        vec![],
        false,
        false,
    )
    .await
    .expect("create branch");

    let mut metadata = Metadata::new();
    metadata.set_branch(branch_id).expect("set branch");
    let metadata_hash = metadata
        .serialize(repository.clone())
        .await
        .expect("serialize metadata");

    let state = state::State::new();
    state.set_parent_self(Hash::default());
    state.set_revision_number(1);
    state.set_metadata_hash(metadata_hash);

    let link_node = Node {
        flags: NodeFlags::Link.bits(),
        child: target_node,
        address: Address {
            hash: target_revision,
            context: target_repo.into(),
        },
        name_hash: hash_string(link_name),
        ..Default::default()
    };
    let link_node_id = state
        .node_add(repository.clone(), ROOT_NODE, link_node, link_name)
        .await
        .expect("node_add link");

    if let Some(link_branch) = link_branch {
        state
            .link_add(
                repository.clone(),
                target_repo,
                link_branch,
                target_revision,
                link_node_id,
                LinkFlags::NoFlags,
            )
            .await
            .expect("link_add reference");
    }

    let serialized = state
        .serialize(repository.clone(), &write_token)
        .await
        .expect("serialize state");
    let signature = branch_push::push(
        repository.clone(),
        branch_id,
        serialized,
        crate::util::test_support::PUSH_ANY,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        lore_server::grpc::server::RevisionListAcceleration::default(),
    )
    .await
    .expect("push")
    .revision;
    (branch_id, signature)
}

async fn collect(
    response: Response<RevisionTreeStream>,
) -> Vec<Result<RevisionTreeResponse, Status>> {
    response.into_inner().collect().await
}

#[tokio::test]
async fn unset_query_returns_invalid_argument() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let mut request = Request::new(RevisionTreeRequest {
            query: None,
            path_prefix: None,
            max_depth: None,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        let err = match handler(
            request,
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        {
            Ok(_) => panic!("unset query should fail"),
            Err(err) => err,
        };
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }))
    .await;
}

#[tokio::test]
async fn signature_query_emits_header_then_nodes() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (branch_id, signature) =
            push_branch_with_files(&repository_context, &["a.txt", "b.txt"]).await;

        let response = handler(
            make_request(repository, Query::Signature(signature.into()), None, None),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let items: Vec<_> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"))
            .collect();

        // First message is the header echoing the resolved
        // identifier and signature.
        let header = match &items[0].payload {
            Some(Payload::Header(h)) => h,
            other => panic!("expected header first, got {other:?}"),
        };
        assert_eq!(Hash::from(header.signature.as_ref()), signature);
        let identifier = header.identifier.as_ref().expect("identifier");
        assert_eq!(identifier.number, 1);
        assert_eq!(BranchId::from(&identifier.branch_id), branch_id);

        // Remaining messages are TreeNode payloads.
        let nodes: Vec<&thin_client_v1::TreeNode> = items[1..]
            .iter()
            .map(|item| match &item.payload {
                Some(Payload::Node(n)) => n,
                other => panic!("expected node payload, got {other:?}"),
            })
            .collect();
        // The walk emits both files (in server-natural order).
        assert!(nodes.iter().any(|n| n.path == "a.txt"));
        assert!(nodes.iter().any(|n| n.path == "b.txt"));
        assert!(
            nodes
                .iter()
                .all(|n| n.node_type == thin_client_v1::NodeType::File as i32)
        );
    }))
    .await;
}

#[tokio::test]
async fn identifier_query_with_zero_resolves_latest() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (branch_id, signature) =
            push_branch_with_files(&repository_context, &["only.txt"]).await;

        let response = handler(
            make_request(
                repository,
                Query::Identifier(model_v1::RevisionIdentifier {
                    branch_id: branch_id.into(),
                    number: 0,
                }),
                None,
                None,
            ),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let items: Vec<_> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"))
            .collect();
        let header = match &items[0].payload {
            Some(Payload::Header(h)) => h,
            other => panic!("expected header, got {other:?}"),
        };
        assert_eq!(Hash::from(header.signature.as_ref()), signature);
        assert_eq!(header.identifier.as_ref().unwrap().number, 1);
    }))
    .await;
}

#[tokio::test]
async fn empty_revision_emits_header_only() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (_branch, signature) = push_branch_with_files(&repository_context, &[]).await;

        let response = handler(
            make_request(repository, Query::Signature(signature.into()), None, None),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let items: Vec<_> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"))
            .collect();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].payload, Some(Payload::Header(_))));
    }))
    .await;
}

#[tokio::test]
async fn unknown_signature_returns_not_found() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let bogus = Hash::from(random::<[u8; 32]>());
        let err = match handler(
            make_request(repository, Query::Signature(bogus.into()), None, None),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        {
            Ok(_) => panic!("unknown signature should fail"),
            Err(err) => err,
        };
        assert_eq!(err.code(), tonic::Code::NotFound);
    }))
    .await;
}

#[tokio::test]
async fn zero_signature_returns_invalid_argument() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let err = match handler(
            make_request(
                repository,
                Query::Signature(Hash::default().into()),
                None,
                None,
            ),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        {
            Ok(_) => panic!("zeroed revision should fail"),
            Err(err) => err,
        };

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert_eq!(err.message(), "Cannot get the tree of a zeroed revision");
    }))
    .await;
}

#[tokio::test]
async fn path_prefix_pointing_at_file_returns_invalid_argument_on_stream() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (_branch, signature) = push_branch_with_files(&repository_context, &["file.txt"]).await;

        let response = handler(
            make_request(
                repository,
                Query::Signature(signature.into()),
                Some("file.txt".into()),
                None,
            ),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("unary part succeeds");

        // The header is emitted first; the error follows on the stream.
        let items: Vec<_> = collect(response).await;
        assert!(matches!(
            items[0].as_ref().unwrap().payload,
            Some(Payload::Header(_))
        ));
        let err = items[1].as_ref().expect_err("expected error item");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }))
    .await;
}

#[tokio::test]
async fn path_prefix_pointing_at_missing_returns_not_found_on_stream() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (_branch, signature) = push_branch_with_files(&repository_context, &[]).await;

        let response = handler(
            make_request(
                repository,
                Query::Signature(signature.into()),
                Some("missing".into()),
                None,
            ),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("unary part succeeds");

        let items: Vec<_> = collect(response).await;
        assert!(matches!(
            items[0].as_ref().unwrap().payload,
            Some(Payload::Header(_))
        ));
        let err = items[1].as_ref().expect_err("expected error item");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }))
    .await;
}

#[tokio::test]
async fn identifier_query_with_concrete_number_returns_that_revision() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        // Two revisions: rev 1 with "first.txt", rev 2 with both files.
        let (branch_id, signatures) = push_branch_with_revisions(
            &repository_context,
            &[&["first.txt"], &["first.txt", "second.txt"]],
        )
        .await;

        // Querying (branch, 1) MUST resolve to revision 1 — the
        // earlier signature, not the latest. Exercises the
        // revision::resolve(`branch@N`) path.
        let response = handler(
            make_request(
                repository,
                Query::Identifier(model_v1::RevisionIdentifier {
                    branch_id: branch_id.into(),
                    number: 1,
                }),
                None,
                None,
            ),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let items: Vec<_> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"))
            .collect();
        let header = match &items[0].payload {
            Some(Payload::Header(h)) => h,
            other => panic!("expected header, got {other:?}"),
        };
        assert_eq!(header.identifier.as_ref().unwrap().number, 1);
        assert_eq!(Hash::from(header.signature.as_ref()), signatures[0]);
    }))
    .await;
}

#[tokio::test]
async fn path_prefix_at_subdirectory_emits_descendants() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (_branch, signature) = push_branch_with_subdir(&repository_context).await;

        // Walking from `subdir` should not see `top.txt`.
        let response = handler(
            make_request(
                repository,
                Query::Signature(signature.into()),
                Some("subdir".into()),
                None,
            ),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"))
            .filter_map(|item| match item.payload {
                Some(Payload::Node(n)) => Some(n),
                _ => None,
            })
            .collect();

        let paths: Vec<&str> = nodes.iter().map(|n| n.path.as_str()).collect();
        assert!(
            paths.contains(&"subdir/inner.txt"),
            "expected subdir/inner.txt in {paths:?}",
        );
        assert!(
            !paths.contains(&"top.txt"),
            "top.txt should be filtered out, got {paths:?}",
        );
    }))
    .await;
}

#[tokio::test]
async fn max_depth_one_excludes_grandchildren() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (_branch, signature) = push_branch_with_subdir(&repository_context).await;

        // max_depth = 1 from root: direct children only. The subdir
        // entry is emitted, but `subdir/inner.txt` (a grandchild)
        // must not be.
        let response = handler(
            make_request(
                repository,
                Query::Signature(signature.into()),
                None,
                Some(1),
            ),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let paths: Vec<String> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"))
            .filter_map(|item| match item.payload {
                Some(Payload::Node(n)) => Some(n.path),
                _ => None,
            })
            .collect();
        assert!(
            paths.iter().any(|p| p == "top.txt"),
            "expected top.txt in {paths:?}",
        );
        assert!(
            paths.iter().any(|p| p == "subdir"),
            "expected subdir in {paths:?}",
        );
        assert!(
            paths.iter().all(|p| p != "subdir/inner.txt"),
            "max_depth=1 must exclude grandchildren, got {paths:?}",
        );
    }))
    .await;
}

#[tokio::test]
async fn unreachable_link_emits_link_node_then_skips_subtree() {
    // The link points at (target_repo, target_revision), but no state has
    // been pushed for that pair. The walker must still emit the link node
    // and continue (silently skipping the absent subtree), per the spec's
    // failure-mode contract.
    let repository = random::<RepositoryId>();
    let target_repo = random::<RepositoryId>();
    let target_revision = Hash::from(random::<[u8; 32]>());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (_branch, signature) = push_branch_with_link(
            &repository_context,
            "linked",
            target_repo,
            target_revision,
            ROOT_NODE,
            None,
        )
        .await;

        let response = handler(
            make_request(repository, Query::Signature(signature.into()), None, None),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"))
            .filter_map(|item| match item.payload {
                Some(Payload::Node(n)) => Some(n),
                _ => None,
            })
            .collect();

        assert_eq!(
            nodes.len(),
            1,
            "expected exactly the link entry, got {nodes:?}"
        );
        let link_node = &nodes[0];
        assert_eq!(link_node.path, "linked");
        assert_eq!(link_node.node_type, thin_client_v1::NodeType::Link as i32);
        let address = link_node.address.as_ref().expect("link address");
        assert_eq!(
            Hash::from(address.hash.as_ref()),
            target_revision,
            "link.address.hash should be the linked revision signature",
        );
        let emitted_context = lore_base::types::Context::from(address.context.as_ref());
        assert_eq!(
            emitted_context,
            target_repo.into(),
            "link.address.context should be the target repository id",
        );
    }))
    .await;
}

#[tokio::test]
async fn link_emits_linked_repository_contents() {
    // Build a real linked repo, push one revision with `inner.txt` at its
    // root, then build the originating repo and push a revision whose
    // root contains a link node pointing at the linked revision. The
    // walker should emit: link entry, then the entry under it.
    let originating = random::<RepositoryId>();
    let linked = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let linked_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            linked,
        ));
        let (_branch, linked_signature) =
            push_branch_with_files(&linked_context, &["inner.txt"]).await;

        let originating_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            originating,
        ));
        let (_branch, signature) = push_branch_with_link(
            &originating_context,
            "linked",
            linked,
            linked_signature,
            ROOT_NODE,
            None,
        )
        .await;

        let response = handler(
            make_request(originating, Query::Signature(signature.into()), None, None),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"))
            .filter_map(|item| match item.payload {
                Some(Payload::Node(n)) => Some(n),
                _ => None,
            })
            .collect();

        // Expect exactly two entries: the link mount, then the file inside.
        assert_eq!(nodes.len(), 2, "expected link + child, got {nodes:?}");

        let link = &nodes[0];
        assert_eq!(link.path, "linked");
        assert_eq!(link.node_type, thin_client_v1::NodeType::Link as i32);
        let link_address = link.address.as_ref().expect("link address");
        assert_eq!(
            lore_base::types::Context::from(link_address.context.as_ref()),
            linked.into(),
            "link node's address.context is the target repository id, so clients \
                 can resolve where to fetch linked content from",
        );
        assert_eq!(
            Hash::from(link_address.hash.as_ref()),
            linked_signature,
            "link node's address.hash is the linked revision signature",
        );

        let child = &nodes[1];
        assert_eq!(child.path, "linked/inner.txt");
        assert_eq!(child.node_type, thin_client_v1::NodeType::File as i32);
    }))
    .await;
}

/// Push a revision holding a single link node whose reference is registered
/// on `link_branch`, then return the `TreeNode` emitted for the mount path.
/// The link target is never pushed, so the walk stops at the mount.
async fn link_tree_node(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository: RepositoryId,
    link_branch: BranchId,
) -> thin_client_v1::TreeNode {
    let repository_context = Arc::new(RepositoryContext::new_server_context(
        immutable_store.clone(),
        mutable_store.clone(),
        repository,
    ));
    let (_branch, signature) = push_branch_with_link(
        &repository_context,
        "linked",
        random::<RepositoryId>(),
        Hash::from(random::<[u8; 32]>()),
        ROOT_NODE,
        Some(link_branch),
    )
    .await;

    let response = handler(
        make_request(repository, Query::Signature(signature.into()), None, None),
        immutable_store,
        mutable_store,
        allow_all(),
        DEFAULT_HISTORY_STEP_SIZE,
        RevisionListAcceleration::default(),
    )
    .await
    .expect("handler ok");

    let mut nodes: Vec<thin_client_v1::TreeNode> = collect(response)
        .await
        .into_iter()
        .map(|item| item.expect("stream item"))
        .filter_map(|item| match item.payload {
            Some(Payload::Node(node)) => Some(node),
            _ => None,
        })
        .collect();

    assert_eq!(
        nodes.len(),
        1,
        "expected exactly the link entry, got {nodes:?}"
    );
    let link_node = nodes.pop().expect("one node");
    assert_eq!(link_node.path, "linked");
    assert_eq!(link_node.node_type, thin_client_v1::NodeType::Link as i32);
    link_node
}

/// A zero-branch link reports `tracking = true`.
#[tokio::test]
async fn tree_marks_zero_branch_link_as_tracking() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let link_node = link_tree_node(
            immutable_store,
            mutable_store,
            repository,
            BranchId::default(),
        )
        .await;
        assert!(
            link_node.tracking,
            "a zero-branch link must be reported as tracking",
        );
    }))
    .await;
}

/// A link pinned to an explicit branch reports `tracking = false`.
#[tokio::test]
async fn tree_marks_pinned_link_as_not_tracking() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let link_node = link_tree_node(
            immutable_store,
            mutable_store,
            repository,
            BranchId::from(uuid::Uuid::now_v7()),
        )
        .await;
        assert!(
            !link_node.tracking,
            "a pinned (non-zero branch) link must not be reported as tracking",
        );
    }))
    .await;
}

/// Build an `AuthorizationToken` whose `resources` list authorizes only
/// the given repository ids. Anything else is unauthorized.
fn token_authorized_for(repos: &[RepositoryId]) -> lore_server::auth::jwt::AuthorizationToken {
    use lore_server::auth::jwt::AuthorizationToken;
    use lore_server::auth::jwt::ResourcePermission;

    AuthorizationToken {
        resources: Some(
            repos
                .iter()
                .map(|id| ResourcePermission {
                    permission: vec![],
                    resource_id: format!("urc-{id}"),
                })
                .collect(),
        ),
        ..Default::default()
    }
}

#[tokio::test]
async fn unauthorized_link_emits_link_only() {
    // Same setup as link_emits_linked_repository_contents, but the
    // request carries an authorization token that only grants access to
    // the originating repo. The walker emits the link node but does
    // not descend into the linked repo.
    let originating = random::<RepositoryId>();
    let linked = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let linked_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            linked,
        ));
        let (_branch, linked_signature) =
            push_branch_with_files(&linked_context, &["inner.txt"]).await;

        let originating_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            originating,
        ));
        let (_branch, signature) = push_branch_with_link(
            &originating_context,
            "linked",
            linked,
            linked_signature,
            ROOT_NODE,
            None,
        )
        .await;

        let mut request = make_request(originating, Query::Signature(signature.into()), None, None);
        request
            .extensions_mut()
            .insert(token_authorized_for(&[originating]));
        request
            .extensions_mut()
            .insert(RawToken("raw.jwt".to_string()));

        let response = handler(
            request,
            immutable_store,
            mutable_store,
            resource_grants(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"))
            .filter_map(|item| match item.payload {
                Some(Payload::Node(n)) => Some(n),
                _ => None,
            })
            .collect();

        assert_eq!(
            nodes.len(),
            1,
            "expected only the link entry, got {nodes:?}"
        );
        assert_eq!(nodes[0].node_type, thin_client_v1::NodeType::Link as i32);
        assert_eq!(nodes[0].path, "linked");
    }))
    .await;
}

#[tokio::test]
async fn nested_links_emit_contents_from_every_repository() {
    // A -> B -> C. Walking A emits A's link to B, B's link to C, then
    // C's file. The two link entries carry their target repo id in
    // address.context (where clients should fetch linked content from).
    let a = random::<RepositoryId>();
    let b = random::<RepositoryId>();
    let c = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let c_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            c,
        ));
        let (_branch_c, c_sig) = push_branch_with_files(&c_context, &["c_file.txt"]).await;

        // B's revision: one link to C.
        let b_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            b,
        ));
        let (_branch_b, b_sig) =
            push_branch_with_link(&b_context, "link_to_c", c, c_sig, ROOT_NODE, None).await;

        // A's revision: one link to B.
        let a_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            a,
        ));
        let (_branch_a, a_sig) =
            push_branch_with_link(&a_context, "link_to_b", b, b_sig, ROOT_NODE, None).await;

        let response = handler(
            make_request(a, Query::Signature(a_sig.into()), None, None),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"))
            .filter_map(|item| match item.payload {
                Some(Payload::Node(n)) => Some(n),
                _ => None,
            })
            .collect();

        let paths_and_types: Vec<(&str, i32)> = nodes
            .iter()
            .map(|n| (n.path.as_str(), n.node_type))
            .collect();
        assert_eq!(
            paths_and_types,
            vec![
                ("link_to_b", thin_client_v1::NodeType::Link as i32),
                ("link_to_b/link_to_c", thin_client_v1::NodeType::Link as i32),
                (
                    "link_to_b/link_to_c/c_file.txt",
                    thin_client_v1::NodeType::File as i32,
                ),
            ],
            "expected A's link to B, B's link to C, then C's file",
        );

        // Each LINK node's address.context is the *target* repo id —
        // i.e. where clients fetch the linked content from.
        let target_context = |idx: usize| {
            let addr = nodes[idx].address.as_ref().expect("link address");
            lore_base::types::Context::from(addr.context.as_ref())
        };
        assert_eq!(target_context(0), b.into(), "link_to_b targets repo B");
        assert_eq!(target_context(1), c.into(), "link_to_c targets repo C");
    }))
    .await;
}

#[tokio::test]
async fn distinct_links_to_same_repository_both_walked() {
    // Two siblings in repo A both link to repo B at the same signature.
    // Both linked subtrees must stream — neither sibling is "inside" the
    // other, so the depth bound applies independently.
    let a = random::<RepositoryId>();
    let b = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let b_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            b,
        ));
        let (_branch_b, b_sig) = push_branch_with_files(&b_context, &["shared.txt"]).await;

        // A's revision: two link nodes pointing at the same B revision.
        let a_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            a,
        ));
        let write_token = get_write_token();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            a_context.clone(),
            &write_token,
            branch_id,
            "test-branch",
            branch::default_category(),
            "creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create branch");

        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(a_context.clone())
            .await
            .expect("serialize metadata");

        let state = state::State::new();
        state.set_parent_self(Hash::default());
        state.set_revision_number(1);
        state.set_metadata_hash(metadata_hash);

        for name in ["first", "second"] {
            let link_node = Node {
                flags: NodeFlags::Link.bits(),
                child: ROOT_NODE,
                address: lore_base::types::Address {
                    hash: b_sig,
                    context: b.into(),
                },
                name_hash: hash_string(name),
                ..Default::default()
            };
            state
                .node_add(a_context.clone(), ROOT_NODE, link_node, name)
                .await
                .expect("node_add");
        }

        let serialized = state
            .serialize(a_context.clone(), &write_token)
            .await
            .expect("serialize state");
        let a_sig = branch_push::push(
            a_context.clone(),
            branch_id,
            serialized,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("push")
        .revision;

        let response = handler(
            make_request(a, Query::Signature(a_sig.into()), None, None),
            immutable_store,
            mutable_store,
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"))
            .filter_map(|item| match item.payload {
                Some(Payload::Node(n)) => Some(n),
                _ => None,
            })
            .collect();

        let paths: Vec<&str> = nodes.iter().map(|n| n.path.as_str()).collect();
        assert!(paths.contains(&"first"), "missing first link in {paths:?}");
        assert!(
            paths.contains(&"first/shared.txt"),
            "missing first/shared.txt in {paths:?} — sibling links to the \
                 same repo must each walk",
        );
        assert!(
            paths.contains(&"second"),
            "missing second link in {paths:?}"
        );
        assert!(
            paths.contains(&"second/shared.txt"),
            "missing second/shared.txt in {paths:?} — siblings must each \
                 walk independently of each other",
        );
    }))
    .await;
}

#[tokio::test]
async fn path_prefix_into_unauthorized_link_returns_not_found() {
    // path_prefix points inside the linked repo, but the caller is not
    // authorized for it. The handler must return Status::not_found (the
    // same shape as a path that does not exist) — never PermissionDenied.
    let originating = random::<RepositoryId>();
    let linked = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let linked_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            linked,
        ));
        let (_branch, linked_signature) =
            push_branch_with_files(&linked_context, &["inner.txt"]).await;

        let originating_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            originating,
        ));
        let (_branch, signature) = push_branch_with_link(
            &originating_context,
            "linked",
            linked,
            linked_signature,
            ROOT_NODE,
            None,
        )
        .await;

        let mut request = make_request(
            originating,
            Query::Signature(signature.into()),
            Some("linked/inner.txt".into()),
            None,
        );
        request
            .extensions_mut()
            .insert(token_authorized_for(&[originating]));
        request
            .extensions_mut()
            .insert(RawToken("raw.jwt".to_string()));

        let response = handler(
            request,
            immutable_store,
            mutable_store,
            resource_grants(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("unary part succeeds");

        let items: Vec<_> = collect(response).await;
        // Header is emitted before the error.
        assert!(matches!(
            items[0].as_ref().unwrap().payload,
            Some(Payload::Header(_))
        ));
        let err = items[1].as_ref().expect_err("expected error item");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }))
    .await;
}

/// `RevisionTree` under what `GrpcServerBuilder::partition_scoped` registers: the JWT interceptor
/// around the partition layer around the service, with the authorizer Bioma selects and a signed
/// token of the shape Hub issues.
mod under_the_servers_layers {
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use jsonwebtoken::Algorithm;
    use jsonwebtoken::DecodingKey;
    use jsonwebtoken::EncodingKey;
    use jsonwebtoken::Header;
    use jsonwebtoken::encode;
    use lore_proto::lore::thin_client::v1::thin_client_service_client::ThinClientServiceClient;
    use lore_proto::lore::thin_client::v1::thin_client_service_server::ThinClientServiceServer;
    use lore_server::auth::jwk::JWKService;
    use lore_server::auth::jwk::JWKServiceError;
    use lore_server::auth::jwt::AuthorizationToken;
    use lore_server::auth::jwt::JwtVerifier;
    use lore_server::auth::jwt::ResourcePermission;
    use lore_server::auth::jwt_interceptor::JWTInterceptor;
    use lore_server::grpc::thinclient::v1::revision_diff::RevisionDiffConfig;
    use lore_server::grpc::thinclient::v1::service::LoreThinClientV1Service;
    use lore_server::grpc::tower::partition_access::PartitionAccessLayer;
    use lore_transport::grpc::PARTITION_ID_KEY;
    use tonic::Code;
    use tonic::metadata::BinaryMetadataValue;
    use tonic::service::interceptor::InterceptedService;
    use tower::Layer;

    use super::*;

    mockall::mock! {
        KeySource {}

        #[async_trait::async_trait]
        impl JWKService for KeySource {
            async fn get_key(&self, kid: &str) -> Result<(DecodingKey, Algorithm), JWKServiceError>;
            fn get_cached_key(&self, kid: &str) -> Option<(DecodingKey, Algorithm)>;
            async fn refresh_key(&self, kid: &str) -> Result<Option<(DecodingKey, Algorithm)>, JWKServiceError>;
        }
    }

    fn token_for(repository: Option<RepositoryId>) -> String {
        let claims = AuthorizationToken {
            user_id: "a-person".to_string(),
            issuer: "test".to_string(),
            audience: vec!["lore".to_string()],
            expires: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 60,
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
        encode(&header, &claims, &EncodingKey::from_secret(b"test-secret")).unwrap()
    }

    fn named(
        query: Query,
        token: Option<&str>,
        keys: &[(&'static str, RepositoryId)],
    ) -> Request<RevisionTreeRequest> {
        let mut request = Request::new(RevisionTreeRequest {
            query: Some(query),
            path_prefix: None,
            max_depth: None,
        });
        for (key, repository) in keys {
            request
                .metadata_mut()
                .insert_bin(*key, BinaryMetadataValue::from_bytes(repository.data()));
        }
        if let Some(token) = token {
            request
                .metadata_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        request
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_for_one_repository_is_refused_on_another() {
        let granted = random::<RepositoryId>();
        let other = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let context = |repository| {
                Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository,
                ))
            };
            let (_, granted_tip) = push_branch_with_files(&context(granted), &["mine.txt"]).await;
            let (other_branch, other_tip) =
                push_branch_with_files(&context(other), &["theirs.txt"]).await;

            let authorizer: Arc<dyn RepositoryAuthorizer> =
                Arc::new(ResourceGrantsAuthorizer::new(
                    "resources".to_string(),
                    "resource_id".to_string(),
                    Some("permission".to_string()),
                    "urc-{id}".to_string(),
                    "urc-*".to_string(),
                ));
            let mut keys = MockKeySource::new();
            keys.expect_get_cached_key()
                .returning(|_| Some((DecodingKey::from_secret(b"test-secret"), Algorithm::HS256)));
            keys.expect_get_key()
                .returning(|_| Ok((DecodingKey::from_secret(b"test-secret"), Algorithm::HS256)));
            keys.expect_refresh_key().returning(|_| Ok(None));
            let verifier = JwtVerifier {
                jwk_service: Arc::new(keys),
                jwt_issuer: None,
                jwt_audience: Some(vec!["lore".to_string()]),
                jwt_typ: None,
            };
            let service = ThinClientServiceServer::new(LoreThinClientV1Service::new(
                immutable_store.clone(),
                mutable_store.clone(),
                authorizer.clone(),
                Duration::from_secs(30),
                RevisionDiffConfig::default(),
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            ));
            let stack = InterceptedService::new(
                PartitionAccessLayer::new(authorizer, Duration::from_secs(5)).layer(service),
                JWTInterceptor::new(Some(&verifier)),
            );
            let mut client = ThinClientServiceClient::new(stack);

            let signature = |tip: Hash| Query::Signature(tip.into());
            let mine = token_for(Some(granted));
            let theirs = token_for(Some(other));
            let nobody = token_for(None);

            // The controls: each token reads its own repository, so both trees are there.
            for (tip, token, repository, file) in [
                (granted_tip, &mine, granted, "mine.txt"),
                (other_tip, &theirs, other, "theirs.txt"),
            ] {
                let mut stream = client
                    .revision_tree(named(
                        signature(tip),
                        Some(token),
                        &[(PARTITION_ID_KEY, repository)],
                    ))
                    .await
                    .expect("a token reads its own repository")
                    .into_inner();
                let mut found = false;
                while let Some(item) = stream.next().await {
                    if let Some(Payload::Node(node)) = item.expect("stream item").payload {
                        found |= node.path == file;
                    }
                }
                assert!(found, "{file}");
            }

            // Refused before the handler, for every way of naming the other repository.
            let refused = |result: Result<_, Status>, case: &str| {
                let status = result.map(|_: Response<_>| ()).expect_err(case);
                assert_eq!(status.code(), Code::PermissionDenied, "{case}");
                assert_eq!(status.message(), "Unauthorized", "{case}");
            };
            refused(
                client
                    .revision_tree(named(
                        signature(other_tip),
                        Some(&mine),
                        &[(PARTITION_ID_KEY, other)],
                    ))
                    .await,
                "partition names the other repository",
            );
            refused(
                client
                    .revision_tree(named(
                        signature(other_tip),
                        Some(&mine),
                        &[(REPOSITORY_ID_KEY, other)],
                    ))
                    .await,
                "the older key names the other repository",
            );
            refused(
                client
                    .revision_tree(named(
                        signature(other_tip),
                        Some(&mine),
                        &[(PARTITION_ID_KEY, other), (REPOSITORY_ID_KEY, granted)],
                    ))
                    .await,
                "partition other, older key own",
            );
            refused(
                client
                    .revision_tree(named(
                        Query::Identifier(model_v1::RevisionIdentifier {
                            branch_id: other_branch.as_ref().to_vec().into(),
                            number: 0,
                        }),
                        Some(&mine),
                        &[(PARTITION_ID_KEY, other)],
                    ))
                    .await,
                "the other branch's tip by identifier",
            );
            refused(
                client
                    .revision_tree(named(
                        signature(other_tip),
                        Some(&nobody),
                        &[(PARTITION_ID_KEY, other)],
                    ))
                    .await,
                "a signed token naming no repository",
            );

            let status = client
                .revision_tree(named(
                    signature(other_tip),
                    None,
                    &[(PARTITION_ID_KEY, other)],
                ))
                .await
                .map(|_| ())
                .expect_err("no token");
            assert_eq!(status.code(), Code::Unauthenticated);
        }))
        .await;
    }
}
