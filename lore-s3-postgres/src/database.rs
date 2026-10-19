// Copyright 2026 VeoveoAI
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::time::Duration;

use lore_base::types::Hash;
use lore_storage::StoreError;
use sqlx::ConnectOptions;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;
use sqlx::postgres::PgConnectOptions;
use sqlx::postgres::PgPoolOptions;

/// A bounded connection pool restricted to an operator-selected schema.
#[derive(Clone)]
pub struct Database {
    pub(crate) pool: PgPool,
    schema: String,
}

/// Accept only ordinary unquoted PostgreSQL identifiers from trusted configuration.
pub fn valid_schema(schema: &str) -> bool {
    !schema.is_empty()
        && schema.len() <= 63
        && schema
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_lowercase() || b == b'_' || (i > 0 && b.is_ascii_digit()))
}

pub(crate) fn pg_error(error: sqlx::Error) -> StoreError {
    // Database messages can include values from a rejected statement. Expose SQLSTATE only.
    let code = error.as_database_error().and_then(|e| e.code());
    StoreError::internal(format!(
        "PostgreSQL storage operation failed (SQLSTATE {})",
        code.as_deref().unwrap_or("unavailable")
    ))
}

impl Database {
    /// Connect to an existing database/schema and initialize new-store tables.
    /// The connection string is secret and is never logged, including on failure.
    pub async fn connect(url: &str, schema: &str) -> Result<Self, StoreError> {
        if !valid_schema(schema) {
            return Err(StoreError::internal("Invalid PostgreSQL schema identifier"));
        }
        let options = PgConnectOptions::from_str(url)
            .map_err(|_| StoreError::internal("Invalid PostgreSQL connection configuration"))?
            .application_name("lore-s3-postgres")
            .disable_statement_logging();
        let search_path = format!("{schema},pg_catalog");
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .acquire_timeout(Duration::from_secs(15))
            .after_connect(move |connection, _| {
                let search_path = search_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false), set_config('statement_timeout', '30000', false)")
                        .bind(search_path).execute(connection).await?;
                    Ok(())
                })
            })
            .connect_with(options).await.map_err(pg_error)?;
        // Fail closed instead of silently creating in public when the selected schema is absent.
        let selected: String = sqlx::query_scalar("SELECT current_schema()")
            .fetch_one(&pool)
            .await
            .map_err(pg_error)?;
        if selected != schema {
            return Err(StoreError::internal(
                "PostgreSQL storage schema does not exist or is inaccessible",
            ));
        }
        sqlx::raw_sql(include_str!("schema.sql"))
            .execute(&pool)
            .await
            .map_err(pg_error)?;
        Ok(Self {
            pool,
            schema: schema.to_owned(),
        })
    }

    pub(crate) async fn lock_hash(
        &self,
        hash: Hash,
    ) -> Result<Transaction<'_, Postgres>, StoreError> {
        self.lock_bytes(b"immutable", hash.data()).await
    }

    pub(crate) async fn lock_bytes(
        &self,
        kind: &[u8],
        bytes: &[u8],
    ) -> Result<Transaction<'_, Postgres>, StoreError> {
        // Collisions only serialize unrelated work. Namespace both lock domains and schemas.
        let mut hash = blake3::Hasher::new_derive_key("lore-s3-postgres advisory lock v1");
        hash.update(self.schema.as_bytes())
            .update(&[0])
            .update(kind)
            .update(&[0])
            .update(bytes);
        let mut prefix = [0_u8; 8];
        prefix.copy_from_slice(&hash.finalize().as_bytes()[..8]);
        let value = i64::from_le_bytes(prefix);
        let mut tx = self.pool.begin().await.map_err(pg_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(value)
            .execute(&mut *tx)
            .await
            .map_err(pg_error)?;
        Ok(tx)
    }
}
