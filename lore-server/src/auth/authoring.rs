// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Optional, signed branch confinement. Absence preserves ordinary client behavior.
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use lore_base::types::{BranchId, Context, RepositoryId};
use serde::Deserialize;
use tonic::Status;

use super::jwt::AuthorizationToken;

pub const CLAIM: &str = "lore_authoring";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoringScope {
    pub version: u32,
    pub repository: String,
    pub branch: String,
}

fn denied() -> Status {
    Status::permission_denied("Operation outside authoring scope")
}

pub fn scope(token: &AuthorizationToken) -> Result<Option<AuthoringScope>, Status> {
    let Some(value) = token.extra.get(CLAIM) else {
        return Ok(None);
    };
    let scope: AuthoringScope = serde_json::from_value(value.clone()).map_err(|_| denied())?;
    let valid_id = |id: &str| {
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if scope.version != 1
        || !valid_id(&scope.repository)
        || !valid_id(&scope.branch)
        || Context::from_str(&scope.repository).is_err()
        || Context::from_str(&scope.branch).is_err()
        || token.is_service_account.unwrap_or(false)
    {
        return Err(denied());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| denied())?
        .as_secs();
    if token.issued_at > now.saturating_add(5)
        || token.expires > token.issued_at.saturating_add(300)
    {
        return Err(denied());
    }
    // Scoped grants have a hard expiry, independent of ordinary JWT clock leeway.
    if token.expires <= now {
        return Err(Status::unauthenticated("Authoring grant expired"));
    }
    Ok(Some(scope))
}

/// Returns whether this RPC requires the ordinary repository write permission.
/// Only immutable uploads and the separately branch-checked push may mutate.
pub fn rpc_access(
    token: &AuthorizationToken,
    path: &str,
    repository: Option<RepositoryId>,
) -> Result<bool, Status> {
    let Some(scope) = scope(token)? else {
        return Ok(false);
    };
    if repository.is_some_and(|id| id.to_string() != scope.repository) {
        return Err(denied());
    }
    let Some((service, method)) = path.strip_prefix('/').and_then(|p| p.split_once('/')) else {
        return Err(denied());
    };
    let (read, write) = match service {
        "lore.repository.v1.RepositoryService" => (
            matches!(method, "RepositoryGet" | "RepositoryMetadataGet"),
            false,
        ),
        "urc.rpc.RepositoryService" => (matches!(method, "Info" | "MetadataGet"), false),
        "lore.storage.v1.StorageService" | "urc.rpc.StorageService" => (
            matches!(
                method,
                "Get" | "GetMetadata" | "GetResolved" | "Query" | "MutableLoad"
            ),
            matches!(method, "Put" | "Copy"),
        ),
        "lore.revision.v1.RevisionService" | "urc.rpc.RevisionService" => (
            matches!(
                method,
                "BranchGet"
                    | "BranchList"
                    | "BranchQuery"
                    | "BranchMetadataGet"
                    | "RevisionList"
                    | "RevisionDescribe"
                    | "RevisionDiff"
                    | "RevisionTree"
                    | "RevisionStateHistory"
                    | "BranchDiff"
                    | "BranchRevisionList"
            ),
            method == "BranchPush",
        ),
        "lore.thinclient.v1.ThinClientService" => (
            matches!(method, "RevisionDescribe" | "RevisionDiff" | "RevisionTree"),
            false,
        ),
        _ => (false, false),
    };
    if !read && !write || write && repository.is_none() {
        return Err(denied());
    }
    Ok(write)
}

/// Called after decoding either supported push request, before any branch mutation.
pub fn branch_access(
    extensions: &tonic::Extensions,
    repository: RepositoryId,
    branch: BranchId,
) -> Result<(), Status> {
    if let Some(token) = extensions.get::<AuthorizationToken>()
        && let Some(scope) = scope(token)?
        && (scope.repository != repository.to_string() || scope.branch != branch.to_string())
    {
        return Err(denied());
    }
    Ok(())
}
