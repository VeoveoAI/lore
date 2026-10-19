// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Operator-only disposable test fixture. Stop the owning server before opening its stores.
//! Uses ordinary repository/branch functions; it is not part of the server or hook runtime.
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_base::types::Context;
use lore_revision::branch;
use lore_revision::repository::RepositoryContext;
use lore_server::grpc::forwarded_requests::CallerContext;
use lore_server::grpc::get_write_token;
use lore_server::grpc::repository::v1::repository_create::repository_create_implementation;
use lore_server::hooks::HookDispatcher;

struct Instruments;
impl lore_telemetry::InstrumentProvider for Instruments {
    fn namespace(&self) -> &'static str {
        "factory_branch_fixture"
    }
}

fn main() -> anyhow::Result<()> {
    let config_path = PathBuf::from(std::env::args().nth(1).ok_or_else(|| {
        anyhow::anyhow!("native server config directory required; server must be stopped")
    })?);
    anyhow::ensure!(
        config_path.join("local.toml").is_file(),
        "native server config directory must contain local.toml"
    );
    let (settings, _) = lore_server::settings::Settings::load(config_path.to_str(), Some("local"))?;
    lore_storage::assume_server_policies();
    runtime().block_on(LORE_CONTEXT.scope(lore_server::util::setup_execution("factory_branch_fixture", String::new(), "fixture-operator".into()), async move {
        let (immutable, mutable) = lore_server::server::open_operator_stores(&settings).await?;
        let repository = Context::from(uuid::Uuid::now_v7());
        let forbidden = Context::from(uuid::Uuid::now_v7());
        let main = Context::from(uuid::Uuid::now_v7());
        let assigned = Context::from(uuid::Uuid::now_v7());
        let other = Context::from(uuid::Uuid::now_v7());
        for (id, default_branch) in [(repository, main), (forbidden, Context::from(uuid::Uuid::now_v7()))] {
            repository_create_implementation(lore_proto::lore::repository::v1::RepositoryCreateRequest {
                id: id.data().to_vec().into(), name: format!("vv-hook-fixture-{id}"), default_branch_id: default_branch.data().to_vec().into(), default_branch_name: "main".into(), ..Default::default()
            }, CallerContext { repository_id: Default::default(), user_id: "fixture-operator".into(), correlation_id: String::new(), authorization: None }, None, immutable.clone(), mutable.clone(), &HookDispatcher::empty(), &Instruments).await?;
        }
        let context = Arc::new(RepositoryContext::new_server_context(immutable.clone(), mutable.clone(), repository.into()));
        for (id, name) in [(assigned, "assigned"), (other, "other-author")] {
            branch::create(context.clone(), &get_write_token(), id, name, branch::default_category(), "fixture-operator", 1, vec![], false, false).await?;
        }
        // Branch metadata alone has no cloneable head. Seed ordinary empty
        // revisions before protecting main, using the same native push helper.
        for id in [main, assigned, other] {
            let mut metadata = lore_revision::metadata::Metadata::new();
            metadata.set_branch(id)?;
            let metadata_hash = metadata.serialize(context.clone()).await?;
            let state = lore_revision::state::State::new();
            state.set_revision_number(1);
            state.set_metadata_hash(metadata_hash);
            let revision = state.serialize(context.clone(), &get_write_token()).await?;
            lore_server::grpc::handlers::branch_push::push(
                context.clone(), id, revision, false, false, false,
                branch::DEFAULT_HISTORY_STEP_SIZE, Default::default(),
            ).await?;
        }
        branch::protect(context, main).await?;
        immutable.flush(true).await?;
        mutable.flush(true).await?;
        println!("{}", serde_json::json!({"repository":repository.to_string(),"forbidden":forbidden.to_string(),"main":main.to_string(),"assigned":assigned.to_string(),"other":other.to_string()}));
        Ok::<_,anyhow::Error>(())
    }))
}
