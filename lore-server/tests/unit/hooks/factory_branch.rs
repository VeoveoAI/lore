// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::Context;
use lore_server::hooks::{
    Hook, HookContext, HookFactory, HookPoint, HookSettings, StatusCode,
    factory_branch::FactoryBranchHookFactory,
};
use std::str::FromStr;

const REPO: &str = "0194b726b34e72b0b45550b88a967076";
const ASSIGNED: &str = "0194b726b34e72b0b45550b88a967077";
const OTHER: &str = "0194b726b34e72b0b45550b88a967078";

fn hook() -> Box<dyn Hook> {
    FactoryBranchHookFactory
        .create(
            &toml::from_str(&format!(
                r#"
[assignments.factory-one]
repository = "{REPO}"
branch = "{ASSIGNED}"
"#
            ))
            .unwrap(),
        )
        .unwrap()
}
fn context(user: &str, repository: &str, branch: &str) -> HookContext {
    HookContext::builder()
        .correlation_id("test")
        .hook_point(HookPoint::BranchPush)
        .user(user)
        .repository(Context::from_str(repository).unwrap().into())
        .branch(Context::from_str(branch).unwrap())
        .build()
}
#[test]
fn assigned_push_is_admitted_and_other_push_explains_both_branches() {
    let hook = hook();
    assert_eq!(hook.hook_points(), &[HookPoint::BranchPush]);
    assert!(
        hook.pre_handler(&context("factory-one", REPO, ASSIGNED))
            .is_ok()
    );
    let error = hook
        .pre_handler(&context("factory-one", REPO, OTHER))
        .unwrap_err();
    assert_eq!(error.status_code(), Some(StatusCode::FailedPrecondition));
    assert!(error.to_string().contains(&format!(
        "This factory is assigned to branch {ASSIGNED}; attempted push to branch {OTHER}."
    )));
}
#[test]
fn humans_and_other_unassigned_identities_keep_normal_behavior() {
    for identity in ["human-one", "unassigned-factory"] {
        assert!(hook().pre_handler(&context(identity, REPO, OTHER)).is_ok());
    }
}
#[test]
fn assignment_is_not_taken_from_operation_metadata() {
    let ctx = HookContext::builder()
        .correlation_id("test")
        .hook_point(HookPoint::BranchPush)
        .user("factory-one")
        .repository(Context::from_str(REPO).unwrap().into())
        .branch(Context::from_str(OTHER).unwrap())
        .metadata("identity", "human-one")
        .metadata("assigned_branch", OTHER)
        .build();
    assert!(hook().pre_handler(&ctx).is_err());
}
#[test]
fn repository_assignment_also_has_to_match() {
    let error = hook()
        .pre_handler(&context("factory-one", OTHER, ASSIGNED))
        .unwrap_err();
    assert!(error.to_string().contains("Assigned repository"));
}
#[test]
fn invalid_trusted_configuration_is_rejected_at_startup() {
    for text in [
        "assignments = {}\nunknown = true",
        "[assignments.factory-one]\nrepository = '*'\nbranch = '*'",
        "[assignments.'<unknown>']\nrepository='0194b726b34e72b0b45550b88a967076'\nbranch='0194b726b34e72b0b45550b88a967077'",
        "[assignments.factory-one]\nrepository='0194b726b34e72b0b45550b88a967076'\nbranch='00000000000000000000000000000000'",
    ] {
        assert!(
            FactoryBranchHookFactory
                .create(&toml::from_str(text).unwrap())
                .is_err()
        );
    }
}
#[test]
fn ordinary_hook_settings_enable_the_guard_without_new_configuration_api() {
    let settings: HookSettings = toml::from_str(&format!(
        r#"enabled=true
[assignments.factory-one]
repository="{REPO}"
branch="{ASSIGNED}"
"#
    ))
    .unwrap();
    assert!(settings.enabled);
    assert!(FactoryBranchHookFactory.create(&settings.config).is_ok());
}

#[tokio::test]
async fn both_existing_push_handlers_invoke_the_guard_with_verified_identity() {
    use crate::{
        notification::testing::MockNotificationSender, store::test_support::test_store_create,
    };
    use lore_base::{runtime::LORE_CONTEXT, types::Hash};
    use lore_server::{auth::jwt::AuthorizationToken, hooks::HookDispatcher};
    use std::{sync::Arc, time::Duration};
    struct Instruments;
    impl lore_telemetry::InstrumentProvider for Instruments {
        fn namespace(&self) -> &'static str {
            "test.factory_branch"
        }
    }
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    let dispatcher = HookDispatcher::new(
        vec![("factory_branch".into(), hook())],
        Duration::from_millis(200),
        Duration::from_secs(1),
    );
    let authenticated = |mut request: tonic::Request<()>| {
        request.extensions_mut().insert(AuthorizationToken {
            user_id: "factory-one".into(),
            is_service_account: Some(false),
            ..Default::default()
        });
        request
            .metadata_mut()
            .insert("x-user-id", "human-one".parse().unwrap());
        request.metadata_mut().insert_bin(
            lore_transport::grpc::REPOSITORY_ID_KEY,
            tonic::metadata::MetadataValue::from_bytes(Context::from_str(REPO).unwrap().data()),
        );
        request
    };
    LORE_CONTEXT
        .scope(execution, async {
            let (metadata, extensions, _) = authenticated(tonic::Request::new(())).into_parts();
            let target: bytes::Bytes = Context::from_str(OTHER).unwrap().into();
            let revision: bytes::Bytes = Hash::from([1u8; 32]).into();
            let v0 = tonic::Request::from_parts(
                metadata.clone(),
                extensions.clone(),
                lore_proto::BranchPushRequest {
                    branch: target.clone(),
                    revision: revision.clone(),
                    ..Default::default()
                },
            );
            let error = lore_server::grpc::handlers::branch_push::handler(
                v0,
                immutable.clone(),
                mutable.clone(),
                Arc::new(MockNotificationSender::new()),
                &dispatcher,
                lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE,
                Default::default(),
                &Instruments,
            )
            .await
            .unwrap_err();
            assert_eq!(error.code(), tonic::Code::FailedPrecondition);
            assert!(error.message().contains(&format!(
                "assigned to branch {ASSIGNED}; attempted push to branch {OTHER}"
            )));
            let v1 = tonic::Request::from_parts(
                metadata,
                extensions,
                lore_proto::lore::revision::v1::BranchPushRequest {
                    id: target,
                    revision_signature: revision,
                    ..Default::default()
                },
            );
            let error = lore_server::grpc::revision::v1::branch_push::handler(
                v1,
                immutable,
                mutable,
                Arc::new(MockNotificationSender::new()),
                &dispatcher,
                lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE,
                Default::default(),
                &Instruments,
            )
            .await
            .unwrap_err();
            assert_eq!(error.code(), tonic::Code::FailedPrecondition);
            assert!(error.message().contains(&format!(
                "assigned to branch {ASSIGNED}; attempted push to branch {OTHER}"
            )));
        })
        .await;
}
