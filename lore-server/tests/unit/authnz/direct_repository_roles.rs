// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::types::RepositoryId;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::authnz::repository_authorizer::Grants;
use lore_server::authnz::repository_authorizer::PartitionGrants;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::authnz::repository_authorizer::repository_authorizer;
use lore_server::authnz::repository_authorizer::select_repository_authorizer;
use lore_server::authnz::repository_catalog::repository_catalog;
use lore_server::settings::AuthSettings;
use lore_server::settings::Settings;
use lore_server::settings::validate_auth_config;
use serde_json::json;
use tonic::Code;

const FIRST: &str = "0194b726b34e72b0b45550b88a967076";
const SECOND: &str = "0192ae48ccf17060bc1ba9d04f6acb2f";
const ABSENT: &str = "0192ae48ccf17060bc1ba9d04f6acb20";

fn settings() -> AuthSettings {
    toml::from_str(&format!(
        r#"
        jwt_issuer = "https://auth.example.com"
        jwt_audience = ["lore-api"]
        jwt_typ = "JWT"
        resource_id_template = "repo:{{id}}"
        repository_catalog = "token_grants"
        [oidc]
        client_id = "public-cli"
        scopes = ["openid", "lore-api/access_as_user"]
        [direct_repository_roles]
        claim = "roles"
        [[direct_repository_roles.grants]]
        role = "first.member"
        repository = "{FIRST}"
        permissions = []
        [[direct_repository_roles.grants]]
        role = "first.owner"
        repository = "{FIRST}"
        permissions = ["owner", "obliterate"]
        [[direct_repository_roles.grants]]
        role = "first.migrator"
        repository = "{FIRST}"
        permissions = ["migrate"]
        [[direct_repository_roles.grants]]
        role = "second.member"
        repository = "{SECOND}"
        permissions = []
        [[direct_repository_roles.grants]]
        role = "absent.member"
        repository = "{ABSENT}"
        permissions = []
    "#
    ))
    .unwrap()
}

fn repository(id: &str) -> RepositoryId {
    id.parse().unwrap()
}

fn token_claims(roles: serde_json::Value) -> AuthorizationToken {
    AuthorizationToken {
        user_id: "signed-user".into(),
        extra: serde_json::from_value(json!({"roles": roles})).unwrap(),
        ..Default::default()
    }
}

#[tokio::test]
async fn direct_roles_union_only_exact_repository_actions_and_feed_privileged_checks() {
    let auth = settings();
    let authorizer = repository_authorizer(Some(&auth), None).unwrap();
    let claims = token_claims(json!([
        "first.member",
        "first.owner",
        "first.migrator",
        "first.owner",
        "unknown"
    ]));
    let token = VerifiedToken {
        raw: "synthetic",
        claims: &claims,
    };
    let first = repository(FIRST);
    let second = repository(SECOND);
    for action in [None, Some("owner"), Some("migrate"), Some("obliterate")] {
        authorizer
            .check_repository_access(Some(&token), first, action)
            .await
            .unwrap();
    }
    assert_eq!(
        authorizer
            .check_repository_access(Some(&token), first, Some("admin"))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    assert_eq!(
        authorizer
            .check_repository_access(Some(&token), second, None)
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    assert_eq!(
        authorizer
            .check_repository_access_sync(Some(&token), second, Some("obliterate"))
            .unwrap()
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );

    let grants = authorizer
        .granted_actions(Some(&token), first)
        .await
        .unwrap()
        .unwrap();
    let mut extensions = tonic::Extensions::new();
    extensions.insert(claims.clone());
    extensions.insert(PartitionGrants {
        repository_id: first,
        grants,
    });
    assert!(lore_server::grpc::is_owner_or_admin(&extensions, first));
    assert!(lore_server::grpc::can_admin_lock(&extensions, first));
    assert!(lore_server::grpc::can_obliterate(&extensions, first));
    assert!(!lore_server::grpc::can_obliterate(&extensions, second));
    assert_eq!(
        lore_server::grpc::get_authorization(&extensions)
            .unwrap()
            .is_service_account,
        None
    );

    // An admitted finite action set wins over a structured claim on the same JWT.
    let mut ordinary = claims;
    ordinary.resources = Some(vec![
        lore_server::auth::jwt::ResourcePermission {
            resource_id: format!("urc-{FIRST}"),
            permission: vec!["obliterate".into()],
        },
        lore_server::auth::jwt::ResourcePermission {
            resource_id: format!("urc-{SECOND}"),
            permission: vec!["obliterate".into()],
        },
    ]);
    extensions.insert(ordinary);
    extensions.insert(PartitionGrants {
        repository_id: first,
        grants: Grants::Actions(Default::default()),
    });
    assert!(!lore_server::grpc::can_obliterate(&extensions, first));
    // Another repository cannot recover foreign structured actions past the
    // exact admitted partition boundary, even from a signed mixed-shape token.
    assert!(!lore_server::grpc::can_obliterate(&extensions, second));
}

#[tokio::test]
async fn absent_unknown_or_malformed_role_claims_never_partially_authorize() {
    let auth = settings();
    let authorizer = repository_authorizer(Some(&auth), None).unwrap();
    for roles in [
        json!(null),
        json!("first.member"),
        json!({"first.member": true}),
        json!([]),
        json!(["unknown"]),
        json!(["first.member", null]),
        json!(["first.member", 1]),
        json!(["first.member", ""]),
        json!(["first.member", " "]),
    ] {
        let claims = token_claims(roles);
        let token = VerifiedToken {
            raw: "synthetic",
            claims: &claims,
        };
        for id in [FIRST, SECOND] {
            assert_eq!(
                authorizer
                    .check_repository_access(Some(&token), repository(id), None)
                    .await
                    .unwrap_err()
                    .code(),
                Code::PermissionDenied
            );
        }
    }
    let claims = AuthorizationToken::default();
    let token = VerifiedToken {
        raw: "synthetic",
        claims: &claims,
    };
    assert_eq!(
        authorizer
            .check_repository_access(Some(&token), repository(FIRST), None)
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    let claims = token_claims(json!(["second.member"]));
    let token = VerifiedToken {
        raw: "synthetic",
        claims: &claims,
    };
    authorizer
        .check_repository_access(Some(&token), repository(SECOND), None)
        .await
        .unwrap();
    assert_eq!(
        authorizer
            .check_repository_access(Some(&token), repository(FIRST), None)
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
}

#[test]
fn direct_role_configuration_is_admitted_before_runtime_construction() {
    let valid = settings();
    let validate = |auth: AuthSettings| {
        let mut config: Settings =
            toml::from_str(include_str!("../../../config/default.toml")).unwrap();
        config.server.auth = Some(auth);
        validate_auth_config(&config)
    };
    validate(valid.clone()).unwrap();
    assert!(
        select_repository_authorizer(Some(&valid), Some("ucs-auth://auth.example.com")).is_err()
    );
    let mut invalid = Vec::new();
    let mut auth = valid.clone();
    auth.resource_claim = Some("resources".into());
    invalid.push(auth);
    let mut auth = valid.clone();
    auth.permission_claim = Some("roles".into());
    invalid.push(auth);
    let mut auth = valid.clone();
    auth.direct_repository_roles.as_mut().unwrap().claim = "roles..values".into();
    invalid.push(auth);
    let mut auth = valid.clone();
    auth.direct_repository_roles
        .as_mut()
        .unwrap()
        .grants
        .clear();
    invalid.push(auth);
    let mut auth = valid.clone();
    auth.direct_repository_roles.as_mut().unwrap().grants[0]
        .role
        .clear();
    invalid.push(auth);
    let mut auth = valid.clone();
    let duplicate = auth.direct_repository_roles.as_ref().unwrap().grants[0].clone();
    auth.direct_repository_roles
        .as_mut()
        .unwrap()
        .grants
        .push(duplicate);
    invalid.push(auth);
    let mut auth = valid.clone();
    auth.direct_repository_roles.as_mut().unwrap().grants[0].repository = RepositoryId::default();
    invalid.push(auth);
    let mut auth = valid.clone();
    auth.direct_repository_roles.as_mut().unwrap().grants[1]
        .permissions
        .push(lore_server::settings::RepositoryAction::Owner);
    invalid.push(auth);
    for template in ["repo", "repo:{id}:{id}", "repo:{other}:{id}"] {
        let mut auth = valid.clone();
        auth.resource_id_template = template.into();
        invalid.push(auth);
    }
    let mut auth = valid.clone();
    auth.resource_wildcard = format!("repo:{FIRST}");
    invalid.push(auth);
    for field in ["scope", "resource", "issuer"] {
        let mut auth = valid.clone();
        let oidc = auth.oidc.as_mut().unwrap();
        match field {
            "scope" => oidc.scope_template = Some("repo:{id}".into()),
            "resource" => oidc.resource_template = Some("https://auth.example.com/{id}".into()),
            _ => oidc.token_exchange_issuer = Some("https://auth.example.com".into()),
        }
        invalid.push(auth);
    }
    for auth in invalid {
        assert!(validate(auth).is_err());
    }
    for permission in ["", "push-protected", "admin ", "*", "typo"] {
        assert!(
            toml::from_str::<lore_server::settings::RepositoryRoleGrant>(&format!(
                r#"role="invalid"
repository="{FIRST}"
permissions=["{permission}"]"#
            ))
            .is_err()
        );
    }
    assert!(
        toml::from_str::<lore_server::settings::RepositoryRoleGrant>(
            r#"role="invalid"
repository="*"
permissions=[]"#
        )
        .is_err()
    );
}

#[tokio::test]
async fn token_grants_catalog_pages_actual_repositories_under_current_roles() {
    let auth = settings();
    let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
        .await
        .unwrap();
    let context = Arc::new(
        lore_revision::repository::RepositoryContext::new_server_context(
            immutable.clone(),
            mutable.clone(),
            Default::default(),
        ),
    );
    for (name, id) in [("first", FIRST), ("second", SECOND)] {
        lore_revision::repository::store_name_to_id(context.clone(), name, repository(id))
            .await
            .unwrap();
    }
    let catalog = repository_catalog(Some(&auth), None, immutable, mutable).unwrap();
    let claims = token_claims(json!(["first.member", "second.member", "absent.member"]));
    let token = VerifiedToken {
        raw: "synthetic",
        claims: &claims,
    };
    let (first, cursor) = catalog
        .list_repositories(Some(&token), Some(1), None)
        .await
        .unwrap();
    assert_eq!(first, vec![repository(SECOND)]);
    let cursor = cursor.unwrap();
    let (second, terminal) = catalog
        .list_repositories(Some(&token), Some(1), Some(&cursor))
        .await
        .unwrap();
    assert_eq!(second, vec![repository(FIRST)]);
    assert_eq!(terminal, None);
    let claims = token_claims(json!(["second.member"]));
    let token = VerifiedToken {
        raw: "synthetic",
        claims: &claims,
    };
    assert!(
        catalog
            .list_repositories(Some(&token), Some(1), Some(&cursor))
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert_eq!(
        catalog
            .list_repositories(Some(&token), None, None)
            .await
            .unwrap()
            .0,
        vec![repository(SECOND)]
    );
    assert_eq!(
        catalog
            .list_repositories(None, None, None)
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    assert_eq!(
        catalog
            .list_repositories(Some(&token), Some(1), Some("not-an-id"))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
}
