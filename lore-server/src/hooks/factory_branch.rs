// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Workflow guard for normal pushes by explicitly assigned factory identities.
use std::collections::BTreeMap;
use std::str::FromStr;

use async_trait::async_trait;
use lore_base::types::Context;
use lore_revision::lore::{BranchId, RepositoryId};
use serde::Deserialize;

use super::{
    Hook, HookContext, HookError, HookFactory, HookPoint, HookRegistrationContext, HookRegistry,
    StatusCode,
};

const NAME: &str = "factory_branch";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    assignments: BTreeMap<String, Assignment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Assignment {
    repository: String,
    branch: String,
}

struct FactoryBranchHook {
    assignments: BTreeMap<String, (RepositoryId, BranchId)>,
}

#[async_trait]
impl Hook for FactoryBranchHook {
    fn name(&self) -> &'static str {
        NAME
    }

    fn hook_points(&self) -> &'static [HookPoint] {
        &[HookPoint::BranchPush]
    }

    fn pre_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
        if ctx.hook_point() != HookPoint::BranchPush {
            return Ok(());
        }
        // The existing push handlers obtain this identity from verified JWT
        // extensions. Never infer it from request metadata or local CLI identity.
        let Some((repository, branch)) = ctx.user().and_then(|user| self.assignments.get(user))
        else {
            return Ok(());
        };
        if ctx.repository() == *repository && ctx.branch() == Some(*branch) {
            return Ok(());
        }
        let attempted = ctx
            .branch()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "<missing>".into());
        let mut message = format!(
            "This factory is assigned to branch {branch}; attempted push to branch {attempted}."
        );
        if ctx.repository() != *repository {
            message.push_str(&format!(
                " Assigned repository {repository}; attempted repository {}.",
                ctx.repository()
            ));
        }
        Err(HookError::rejected(
            NAME,
            message,
            // A workflow precondition, not a replacement authorization layer.
            // Ordinary clients preserve the message for this existing status.
            StatusCode::FailedPrecondition,
        ))
    }
}

pub struct FactoryBranchHookFactory;

impl HookFactory for FactoryBranchHookFactory {
    fn name(&self) -> &'static str {
        NAME
    }

    fn create(&self, config: &toml::Value) -> Result<Box<dyn Hook>, HookError> {
        let config: Config = config.clone().try_into().map_err(|error| {
            HookError::config_error(NAME, format!("Invalid assignments: {error}"))
        })?;
        let parse_id = |id: &str| -> Result<Context, HookError> {
            if id.len() != 32
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(HookError::config_error(
                    NAME,
                    "Assignment IDs must be canonical 32-character lowercase hex",
                ));
            }
            let id = Context::from_str(id)
                .map_err(|_| HookError::config_error(NAME, "Invalid assignment ID"))?;
            if id == Context::default() {
                return Err(HookError::config_error(
                    NAME,
                    "Assignment IDs must be nonzero",
                ));
            }
            Ok(id)
        };
        let mut assignments = BTreeMap::new();
        for (identity, assignment) in config.assignments {
            if identity.is_empty()
                || identity == "<unknown>"
                || identity.len() > 256
                || identity.chars().any(char::is_control)
            {
                return Err(HookError::config_error(
                    NAME,
                    "A factory assignment requires a nonempty verified identity",
                ));
            }
            assignments.insert(
                identity,
                (
                    parse_id(&assignment.repository)?.into(),
                    parse_id(&assignment.branch)?,
                ),
            );
        }
        Ok(Box::new(FactoryBranchHook { assignments }))
    }
}

pub fn register(registry: &mut HookRegistry, _ctx: &HookRegistrationContext) {
    registry.register_hook(Box::new(FactoryBranchHookFactory));
}
