// Copyright 2026 VeoveoAI
// SPDX-License-Identifier: MIT
//! S3 payloads and PostgreSQL associations/references in an existing database.
use std::any::Any;
use std::sync::Arc;

use lore_aws::clients::AwsClientBuilder;
use lore_aws::clients::HttpClientSettings;
use lore_base::error::PluginConfigError;
use lore_base::error::PluginInitError;
use lore_s3_postgres::Database;
use lore_s3_postgres::PostgresMutableStore;
use lore_s3_postgres::S3PostgresStore;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use serde::Deserialize;

use crate::plugins::ImmutableStorePluginFactory;
use crate::plugins::MutableStorePluginFactory;
use crate::plugins::PluginError;
use crate::plugins::PluginRegistry;

const NAME: &str = "s3_postgres";

/// Nonsecret storage configuration. PostgreSQL credentials come only from the selected
/// environment variable or private file; S3 uses the native AWS credential provider chain.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3PostgresConfig {
    pub postgres_url_env: Option<String>,
    pub postgres_url_file: Option<String>,
    pub schema: String,
    pub s3_bucket: String,
    pub s3_endpoint_url: Option<String>,
    pub s3_region: Option<String>,
    #[serde(default)]
    pub s3_force_path_style: bool,
}

fn config_error(message: &str) -> PluginError {
    PluginConfigError {
        plugin_name: NAME.into(),
        message: message.into(),
    }
    .into()
}

fn parse(config: &toml::Value) -> Result<S3PostgresConfig, PluginError> {
    let value: S3PostgresConfig = config
        .clone()
        .try_into()
        .map_err(|_| config_error("Invalid s3_postgres configuration"))?;
    if value.postgres_url_env.is_some() == value.postgres_url_file.is_some()
        || !lore_s3_postgres::database_schema_valid(&value.schema)
    {
        return Err(config_error(
            "Select exactly one PostgreSQL URL environment variable or private file and a valid schema",
        ));
    }
    if value.s3_bucket.is_empty() {
        return Err(config_error("S3 bucket is required"));
    }
    Ok(value)
}

/// Open the configured native store for the server or a stopped-server operator fixture.
pub async fn open_store(config: &toml::Value) -> Result<Arc<S3PostgresStore>, PluginError> {
    let config = parse(config)?;
    let url = if let Some(name) = config.postgres_url_env {
        std::env::var(name).map_err(|_| {
            config_error("PostgreSQL connection environment variable is unavailable")
        })?
    } else {
        let path = config
            .postgres_url_file
            .ok_or_else(|| config_error("PostgreSQL connection file is unavailable"))?;
        tokio::fs::read_to_string(path)
            .await
            .map_err(|_| config_error("PostgreSQL connection file is unreadable"))?
    };
    let database = Database::connect(url.trim(), &config.schema)
        .await
        .map_err(|error| {
            PluginError::from(PluginInitError {
                plugin_name: NAME.into(),
                message: error.to_string(),
            })
        })?;
    let s3 = AwsClientBuilder::builder()
        .with_http_settings(&HttpClientSettings::default())
        .maybe_endpoint(config.s3_endpoint_url)
        .maybe_region(config.s3_region)
        .build_config()
        .await
        .s3_with_path_style(config.s3_force_path_style)
        .ensure_bucket(&config.s3_bucket)
        .build()
        .await
        .map_err(|_| {
            PluginError::from(PluginInitError {
                plugin_name: NAME.into(),
                message: "Cannot open the configured S3 bucket".into(),
            })
        })?;
    S3PostgresStore::new(database, s3, config.s3_bucket)
        .await
        .map_err(|error| {
            PluginError::from(PluginInitError {
                plugin_name: NAME.into(),
                message: error.to_string(),
            })
        })
}

struct ImmutableFactory;
impl ImmutableStorePluginFactory for ImmutableFactory {
    fn name(&self) -> &'static str {
        NAME
    }
    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        parse(config).map(|_| ())
    }
    fn create(&self, config: &toml::Value) -> Result<Arc<dyn ImmutableStore>, PluginError> {
        tokio::task::block_in_place(|| lore_base::runtime::runtime().block_on(open_store(config)))
            .map(|store| store as Arc<dyn ImmutableStore>)
    }
}
struct MutableFactory;
impl MutableStorePluginFactory for MutableFactory {
    fn name(&self) -> &'static str {
        NAME
    }
    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        parse(config).map(|_| ())
    }
    fn create(
        &self,
        config: &toml::Value,
        immutable: Arc<dyn ImmutableStore>,
    ) -> Result<Arc<dyn MutableStore>, PluginError> {
        parse(config)?;
        let any: &dyn Any = immutable.as_ref();
        let store = any.downcast_ref::<S3PostgresStore>().ok_or_else(|| {
            config_error(
                "s3_postgres mutable storage requires the same s3_postgres immutable store",
            )
        })?;
        Ok(PostgresMutableStore::new(store.database()))
    }
}

/// Register native immutable and mutable store factories; locks remain separately configured.
pub fn register(registry: &mut PluginRegistry) {
    registry.register_immutable_store_plugin(Box::new(ImmutableFactory));
    registry.register_mutable_store_plugin(Box::new(MutableFactory));
}
