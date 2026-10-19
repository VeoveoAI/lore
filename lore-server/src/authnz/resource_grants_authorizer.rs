// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeMap;
use std::collections::HashSet;

use anyhow::bail;
use async_trait::async_trait;
use lore_base::types::RepositoryId;
use tonic::Status;

use super::repository_authorizer::Grants;
use super::repository_authorizer::RepositoryAuthorizer;
use super::repository_authorizer::VerifiedToken;
use crate::auth::jwt::ResourceMatcher;
use crate::auth::jwt::ResourcePermission;
use crate::settings::AuthSettings;

enum GrantSource {
    Structured {
        claim: String,
        resource_id_claim: String,
        permission_claim: String,
    },
    Roles {
        claim: String,
        grants: BTreeMap<String, ResourcePermission>,
    },
}

/// Per-repository grants read from the token's resource claim or explicitly
/// configured direct role mappings, with no network call.
///
/// The dotted claim path named by `[server.auth].resource_claim` holds the
/// resource entries. An entry matches a repository when its resource name
/// equals what `resource_id_template` renders for it, or equals
/// `resource_wildcard`. Permissions are merged across all matching entries.
/// A token with no matching resource entry is denied everything,
/// `action: None` included.
///
/// The two fields inside a resource entry follow the same rule: the resource
/// name is read from the field named by `[server.auth].resource_id_claim`
/// (default `resource_id`) and the actions from the field named by
/// `[server.auth].permission_claim` (default `permission`). The defaults
/// match legacy `UrcAuthApi` tokens; both are configurable for providers
/// whose entry shape cannot be changed, such as Keycloak's UMA `permissions`
/// entries carrying `rsname` and `scopes`.
pub struct ResourceGrantsAuthorizer {
    source: GrantSource,
    matcher: ResourceMatcher,
}

impl ResourceGrantsAuthorizer {
    /// Read the existing structured resource-claim format.
    pub fn new(
        resource_claim: String,
        resource_id_claim: String,
        permission_claim: Option<String>,
        resource_id_template: String,
        resource_wildcard: String,
    ) -> Self {
        Self {
            source: GrantSource::Structured {
                claim: resource_claim,
                resource_id_claim,
                permission_claim: permission_claim.unwrap_or_else(|| "permission".to_string()),
            },
            matcher: ResourceMatcher::new(resource_id_template, resource_wildcard),
        }
    }

    /// Compile configured role meanings into exact native resource grants, or retain
    /// the structured resource-claim reader. No subject assignments are held here.
    pub fn from_settings(auth: &AuthSettings) -> anyhow::Result<Self> {
        let Some(roles) = &auth.direct_repository_roles else {
            let Some(claim) = &auth.resource_claim else {
                bail!(
                    "in-token repository grants require resource_claim or direct_repository_roles"
                );
            };
            return Ok(Self::new(
                claim.clone(),
                auth.resource_id_claim.clone(),
                auth.permission_claim.clone(),
                auth.resource_id_template.clone(),
                auth.resource_wildcard.clone(),
            ));
        };
        if auth.resource_claim.is_some() || auth.permission_claim.is_some() {
            bail!(
                "direct_repository_roles cannot be combined with resource_claim or permission_claim"
            );
        }
        if auth.oidc.as_ref().is_some_and(|oidc| {
            oidc.resource_template.is_some()
                || oidc.scope_template.is_some()
                || oidc.token_exchange_issuer.is_some()
        }) {
            bail!(
                "direct_repository_roles uses the original access token and cannot configure token exchange"
            );
        }
        if roles
            .claim
            .split('.')
            .any(|part| part.is_empty() || part.chars().any(char::is_whitespace))
        {
            bail!("direct_repository_roles.claim must be a nonempty dotted claim path");
        }
        if roles.grants.is_empty() {
            bail!("direct_repository_roles.grants must not be empty");
        }
        let template = &auth.resource_id_template;
        let remaining = template.replacen("{id}", "", 1);
        if template.matches("{id}").count() != 1 || remaining.contains(['{', '}']) {
            bail!(
                "direct_repository_roles requires a resource_id_template containing exactly one {{id}} and no other expressions"
            );
        }
        let matcher = ResourceMatcher::new(template.clone(), auth.resource_wildcard.clone());
        let mut grants = BTreeMap::new();
        for grant in &roles.grants {
            if grant.role.is_empty() || grant.role.trim() != grant.role {
                bail!(
                    "direct_repository_roles role values must be nonempty and have no surrounding whitespace"
                );
            }
            if grant.repository == RepositoryId::default() {
                bail!("direct_repository_roles cannot name the root partition");
            }
            let resource_id = matcher.resource_for(grant.repository);
            if resource_id == auth.resource_wildcard {
                bail!("a direct repository grant cannot equal resource_wildcard");
            }
            let mut actions = HashSet::new();
            for action in &grant.permissions {
                if !actions.insert(action) {
                    bail!("direct_repository_roles permissions must be distinct native actions");
                }
            }
            if grants
                .insert(
                    grant.role.clone(),
                    ResourcePermission {
                        resource_id,
                        permission: grant
                            .permissions
                            .iter()
                            .map(|action| action.as_str().to_owned())
                            .collect(),
                    },
                )
                .is_some()
            {
                bail!("direct_repository_roles role values must be unique");
            }
        }
        Ok(Self {
            source: GrantSource::Roles {
                claim: roles.claim.clone(),
                grants,
            },
            matcher,
        })
    }

    /// The token's grant entries.
    fn grants(&self, token: &VerifiedToken<'_>) -> Vec<ResourcePermission> {
        match &self.source {
            GrantSource::Structured {
                claim,
                resource_id_claim,
                permission_claim,
            } => {
                let Some(serde_json::Value::Array(entries)) = token.claims.claim_at(claim) else {
                    return Vec::new();
                };
                entries
                    .iter()
                    .filter_map(|entry| {
                        let resource_id = value_at(entry, resource_id_claim)?.as_str()?.to_owned();
                        let permission = match value_at(entry, permission_claim) {
                            Some(serde_json::Value::Array(values)) => values
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .map(ToString::to_string)
                                .collect(),
                            _ => Vec::new(),
                        };
                        Some(ResourcePermission {
                            resource_id,
                            permission,
                        })
                    })
                    .collect()
            }
            GrantSource::Roles { claim, grants } => {
                let Some(serde_json::Value::Array(roles)) = token.claims.claim_at(claim) else {
                    return Vec::new();
                };
                // A malformed role cannot leave a partially authorized claim behind.
                if roles.iter().any(|role| {
                    role.as_str()
                        .is_none_or(|value| value.is_empty() || value.trim() != value)
                }) {
                    return Vec::new();
                }
                roles
                    .iter()
                    .filter_map(|role| role.as_str().and_then(|role| grants.get(role)))
                    .cloned()
                    .collect()
            }
        }
    }

    /// Filter an already-local identifier list with the same current-token grant
    /// source and matcher as partition admission, decoding the claim only once.
    pub(crate) fn retain_authorized_repositories(
        &self,
        token: &VerifiedToken<'_>,
        repositories: &mut Vec<RepositoryId>,
    ) {
        let grants = self.grants(token);
        repositories.retain(|repository| self.matcher.any_match(&grants, *repository));
    }
}

/// Resolve a dotted path inside one grant entry.
fn value_at<'a>(value: &'a serde_json::Value, dotted_path: &str) -> Option<&'a serde_json::Value> {
    dotted_path
        .split('.')
        .try_fold(value, |value, segment| value.get(segment))
}

impl ResourceGrantsAuthorizer {
    /// The token's access to one partition: unreachable when no entry
    /// matches, otherwise the permissions merged across every matching entry.
    fn grants_on(&self, token: &VerifiedToken<'_>, repository_id: RepositoryId) -> Grants {
        let entries = self.grants(token);
        if !self.matcher.any_match(&entries, repository_id) {
            return Grants::Denied;
        }
        Grants::Actions(
            self.matcher
                .merged_permissions(&entries, repository_id)
                .into_iter()
                .collect(),
        )
    }

    /// The whole verdict is in the token, so the async and sync trait
    /// methods share this body. Answers in place rather than through
    /// [`grants_on`](Self::grants_on): the link-read closure asks this per
    /// link, and the merged permission set is only worth building for an
    /// enumeration.
    fn check(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Result<(), Status> {
        let Some(token) = token else {
            return Err(Status::unauthenticated("No token"));
        };
        let entries = self.grants(token);
        if !self.matcher.any_match(&entries, repository_id) {
            return Err(Status::permission_denied("No grant for repository"));
        }
        match action {
            None => Ok(()),
            Some(action) if self.matcher.permits(&entries, repository_id, action) => Ok(()),
            Some(_) => Err(Status::permission_denied("Action not permitted")),
        }
    }
}

#[async_trait]
impl RepositoryAuthorizer for ResourceGrantsAuthorizer {
    async fn check_repository_access(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Result<(), Status> {
        self.check(token, repository_id, action)
    }

    /// Any verified token: the client looks a repository up before it can exchange for a
    /// token naming it, and a record discloses no more than `RepositoryCreate` refusing a
    /// taken name already does.
    async fn check_repository_lookup(
        &self,
        token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
    ) -> Result<(), Status> {
        match token {
            Some(_) => Ok(()),
            None => Err(Status::unauthenticated("No token")),
        }
    }

    fn check_repository_access_sync(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Option<Result<(), Status>> {
        Some(self.check(token, repository_id, action))
    }

    async fn granted_actions(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
    ) -> Result<Option<Grants>, Status> {
        Ok(Some(match token {
            None => Grants::Denied,
            Some(token) => self.grants_on(token, repository_id),
        }))
    }
}
