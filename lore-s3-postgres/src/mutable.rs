// Copyright 2026 VeoveoAI
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use lore_storage::store_types::KeyType;
use lore_storage::store_types::KeyValueStream;
use sqlx::Row;

use crate::database::Database;
use crate::database::pg_error;

/// `PostgreSQL` reference storage with native Lore absent/zero and CAS semantics.
pub struct PostgresMutableStore {
    database: Database,
}

fn typed_key(mut key: Hash, kind: KeyType) -> Hash {
    key.data_mut()[2] = kind as u8;
    key
}

impl PostgresMutableStore {
    /// Use an initialized native storage schema in the existing database.
    pub fn new(database: Database) -> Arc<Self> {
        Arc::new(Self { database })
    }

    async fn write(
        &self,
        partition: Partition,
        key: Hash,
        expected: Option<Hash>,
        value: Hash,
    ) -> Result<Hash, StoreError> {
        let mut lock = [0_u8; 48];
        lock[..16].copy_from_slice(partition.data());
        lock[16..].copy_from_slice(key.data());
        let mut tx = self.database.lock_bytes(b"mutable", &lock).await?;
        let previous: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT value FROM mutable_refs WHERE partition_id=$1 AND key=$2")
                .bind(partition.data().as_slice())
                .bind(key.data().as_slice())
                .fetch_optional(&mut *tx)
                .await
                .map_err(pg_error)?;
        let previous = previous.map_or(Hash::default(), |v| Hash::from(v.as_slice()));
        if expected.is_none_or(|expected| previous == expected) {
            if value.is_zero() {
                sqlx::query("DELETE FROM mutable_refs WHERE partition_id=$1 AND key=$2")
                    .bind(partition.data().as_slice())
                    .bind(key.data().as_slice())
                    .execute(&mut *tx)
                    .await
                    .map_err(pg_error)?;
            } else {
                sqlx::query("INSERT INTO mutable_refs(partition_id,key,value) VALUES($1,$2,$3) ON CONFLICT(partition_id,key) DO UPDATE SET value=EXCLUDED.value")
                    .bind(partition.data().as_slice()).bind(key.data().as_slice()).bind(value.data().as_slice()).execute(&mut *tx).await.map_err(pg_error)?;
            }
        }
        tx.commit().await.map_err(pg_error)?;
        Ok(previous)
    }
}

#[async_trait]
impl MutableStore for PostgresMutableStore {
    /// Nothing on disk to claim: the references are in PostgreSQL.
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, StoreError> {
        Ok(None)
    }

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        kind: KeyType,
    ) -> Result<Hash, StoreError> {
        let typed = typed_key(key, kind);
        let value: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT value FROM mutable_refs WHERE partition_id=$1 AND key=$2")
                .bind(partition.data().as_slice())
                .bind(typed.data().as_slice())
                .fetch_optional(&self.database.pool)
                .await
                .map_err(pg_error)?;
        value
            .map(|value| Hash::from(value.as_slice()))
            .ok_or_else(|| {
                StoreError::from(lore_base::error::AddressNotFound::from(
                    Address::zero_context_hash(key),
                ))
            })
    }
    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        kind: KeyType,
    ) -> Result<(), StoreError> {
        self.write(partition, typed_key(key, kind), None, value)
            .await?;
        Ok(())
    }
    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        kind: KeyType,
    ) -> Result<Hash, StoreError> {
        self.write(partition, typed_key(key, kind), Some(expected), value)
            .await
    }
    async fn list(
        self: Arc<Self>,
        partition: Partition,
        kind: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        let (stream, sender) = KeyValueStream::new();
        if kind == KeyType::Untyped {
            return Ok(stream);
        }
        // The native stream has an unbounded sender and no error item. Bound materialization,
        // failing the operation before exposing a truncated catalog.
        let rows = sqlx::query("SELECT key,value FROM mutable_refs WHERE ($1::boolean OR partition_id=$2) AND get_byte(key,2)=$3 LIMIT 100001")
            .bind(partition.is_zero()).bind(partition.data().as_slice()).bind(kind as i32).fetch_all(&self.database.pool).await.map_err(pg_error)?;
        if rows.len() > 100000 {
            return Err(StoreError::internal(
                "Mutable reference listing exceeds 100000 entries",
            ));
        }
        for row in rows {
            let key: Vec<u8> = row.get(0);
            let value: Vec<u8> = row.get(1);
            sender
                .send((Hash::from(key.as_slice()), Hash::from(value.as_slice())))
                .map_err(|_error| {
                    StoreError::internal("Mutable reference listing receiver closed")
                })?;
        }
        Ok(stream)
    }
    async fn flush(self: Arc<Self>, _sync: bool) -> Result<(), StoreError> {
        Ok(())
    }
}
