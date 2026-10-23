// Copyright 2026 VeoveoAI
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use lore_aws::s3::S3;
use lore_aws::store::object_metadata::from_object_metadata;
use lore_aws::store::object_metadata::to_object_metadata;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::FragmentReference;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_base::types::TypedBytes;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::sanitise_fragment_behavior_flags;
use sqlx::Postgres;
use sqlx::Row;
use sqlx::Transaction;

use crate::database::Database;
use crate::database::pg_error;

/// Durable S3 objects selected atomically by a `PostgreSQL` index.
/// Objects are immutable generations, never overwritten at a content-hash key.
pub struct S3PostgresStore {
    database: Database,
    s3: S3,
    bucket: String,
    #[cfg(feature = "test-util")]
    pub test_hooks: crate::test_util::TestHooks,
}

impl S3PostgresStore {
    /// Open a dedicated, unversioned bucket. Native AWS credential custody belongs to the client.
    pub async fn new(database: Database, s3: S3, bucket: String) -> Result<Arc<Self>, StoreError> {
        let versioning = s3
            .sdk_client()
            .get_bucket_versioning()
            .bucket(&bucket)
            .send()
            .await
            .map_err(|_error| StoreError::internal("Cannot inspect S3 bucket versioning"))?;
        if versioning.status().is_some() {
            return Err(StoreError::internal(
                "S3 PostgreSQL storage requires a bucket that has never enabled versioning",
            ));
        }
        Ok(Arc::new(Self {
            database,
            s3,
            bucket,
            #[cfg(feature = "test-util")]
            test_hooks: Default::default(),
        }))
    }

    /// Share the same bounded `PostgreSQL` pool with the mutable store.
    pub fn database(&self) -> Database {
        self.database.clone()
    }

    async fn association(
        &self,
        partition: Partition,
        address: Address,
    ) -> Result<Option<String>, StoreError> {
        sqlx::query_scalar("SELECT p.object_key FROM payloads p JOIN associations a USING(hash) WHERE a.hash=$1 AND a.partition_id=$2 AND a.context=$3 AND p.object_key IS NOT NULL")
            .bind(address.hash.data().as_slice()).bind(partition.data().as_slice()).bind(address.context.data().as_slice())
            .fetch_optional(&self.database.pool).await.map_err(pg_error)
    }

    async fn invalidate_missing(&self, hash: Hash, key: &str) -> Result<(), StoreError> {
        let mut tx = self.database.lock_hash(hash).await?;
        let changed =
            sqlx::query("UPDATE payloads SET object_key=NULL WHERE hash=$1 AND object_key=$2")
                .bind(hash.data().as_slice())
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(pg_error)?
                .rows_affected();
        if changed > 0 {
            sqlx::query("UPDATE objects SET state='retired' WHERE object_key=$1")
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(pg_error)?;
        }
        tx.commit().await.map_err(pg_error)
    }

    async fn load(&self, key: &str) -> Result<(Fragment, Bytes), StoreError> {
        let hash: Hash = key
            .split('/')
            .next()
            .ok_or_else(|| StoreError::internal("Invalid native S3 object key"))?
            .parse()
            .map_err(|_error| StoreError::internal("Invalid native S3 object key"))?;
        let output = self
            .s3
            .get_object(&self.bucket, key, None)
            .await
            .map_err(|error| {
                if let lore_aws::aws_error::AwsError::AwsSdkError(error) = error
                    && error.as_service_error().is_some_and(|e| e.is_no_such_key())
                {
                    return StoreError::from(lore_base::error::PayloadNotFound::from(hash));
                }
                StoreError::internal("Referenced S3 payload could not be read")
            })?;
        let mut fragment = from_object_metadata(output.metadata())
            .map_err(|_error| StoreError::internal("Invalid S3 fragment object metadata"))?;
        lore_storage::validate_fragment_metadata(&fragment)?;
        let size = output
            .content_length()
            .ok_or_else(|| StoreError::internal("S3 response omitted content length"))?;
        if size < 0 || size as u64 != u64::from(fragment.size_payload) {
            return Err(StoreError::internal(
                "S3 fragment metadata and object length disagree",
            ));
        }
        // The SDK stream is untrusted even after Content-Length: stop before an oversized body
        // can cause an unbounded allocation. Each fragment has the native fixed size ceiling.
        let mut body = output.body;
        let mut bytes = bytes::BytesMut::with_capacity(fragment.size_payload as usize);
        loop {
            let Some(chunk) = body
                .try_next()
                .await
                .map_err(|_error| StoreError::internal("S3 payload stream failed"))?
            else {
                break;
            };
            if bytes.len().saturating_add(chunk.len()) > fragment.size_payload as usize {
                return Err(StoreError::internal("S3 payload exceeds its declared size"));
            }
            bytes.extend_from_slice(&chunk);
        }
        lore_storage::validate_fragment_payload(&fragment, bytes.len())?;
        let payload = bytes.freeze();
        let checksum_payload = payload.clone();
        let checksum = lore_base::lore_spawn_blocking!(move || lore_storage::hash::hash_slice(
            &checksum_payload
        ))
        .await
        .map_err(|_error| StoreError::internal("S3 representation checksum task failed"))?;
        if key.split('/').nth(1) != Some(checksum.to_string().as_str()) {
            return Err(StoreError::internal("S3 representation checksum mismatch"));
        }
        fragment.flags |= FragmentFlags::PayloadStoredDurable.bits();
        Ok((fragment, payload))
    }

    async fn associate(
        tx: &mut Transaction<'_, Postgres>,
        partition: Partition,
        address: Address,
    ) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO associations(hash,partition_id,context) VALUES($1,$2,$3) ON CONFLICT(hash,partition_id,context) DO UPDATE SET id=DEFAULT")
            .bind(address.hash.data().as_slice()).bind(partition.data().as_slice()).bind(address.context.data().as_slice())
            .execute(&mut **tx).await.map_err(pg_error)?;
        Ok(())
    }

    // Native writers complete child writes before publishing a fragment-list parent. Renew the
    // exact child tuples in the same transaction as the parent, including query-skipped children.
    // A concurrent retirement either removed a child first (publication fails), or waits on these
    // row locks and then sees a generation that its stale job cannot remove.
    async fn renew_children(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        partition: Partition,
        address: Address,
        key: &str,
    ) -> Result<(), StoreError> {
        let (fragment, payload) = self.load(key).await?;
        if fragment.flags & FragmentFlags::PayloadFragmented == 0 {
            return Ok(());
        }
        lore_storage::immutable_store::validate_fragment_list(&fragment, &payload)?;
        let payload = payload.to_aligned::<FragmentReference>();
        let hashes: std::collections::BTreeSet<Hash> = payload
            .as_type_slice::<FragmentReference>()
            .iter()
            .map(|r| r.hash)
            .collect();
        let hashes: Vec<Vec<u8>> = hashes.iter().map(|h| h.data().to_vec()).collect();
        let rows = sqlx::query("SELECT a.hash FROM associations a JOIN payloads p USING(hash) WHERE a.partition_id=$1 AND a.context=$2 AND a.hash=ANY($3::bytea[]) AND p.object_key IS NOT NULL ORDER BY a.hash FOR UPDATE OF a")
            .bind(partition.data().as_slice()).bind(address.context.data().as_slice()).bind(&hashes)
            .fetch_all(&mut **tx).await.map_err(pg_error)?;
        if rows.len() != hashes.len() {
            return Err(StoreError::from(lore_base::error::AddressNotFound::from(
                address,
            )));
        }
        let renewed = sqlx::query("UPDATE associations SET id=DEFAULT WHERE partition_id=$1 AND context=$2 AND hash=ANY($3::bytea[])")
            .bind(partition.data().as_slice()).bind(address.context.data().as_slice()).bind(&hashes)
            .execute(&mut **tx).await.map_err(pg_error)?.rows_affected();
        if renewed != hashes.len() as u64 {
            return Err(StoreError::internal(
                "Fragment child renewal lost a locked association",
            ));
        }
        Ok(())
    }

    async fn live_key(
        tx: &mut Transaction<'_, Postgres>,
        hash: Hash,
    ) -> Result<Option<String>, StoreError> {
        sqlx::query_scalar::<_, Option<String>>("SELECT object_key FROM payloads WHERE hash=$1")
            .bind(hash.data().as_slice())
            .fetch_optional(&mut **tx)
            .await
            .map(|v| v.flatten())
            .map_err(pg_error)
    }

    // A child retirement names an exact association generation. Resuming an old parent's cleanup
    // cannot remove a tuple that a later put has recreated under the same partition/context.
    async fn retire(
        &self,
        partition: Partition,
        address: Address,
        generation: Option<i64>,
        parent: Option<&str>,
        stats: &StoreObliterateStats,
    ) -> Result<(), StoreError> {
        let mut tx = self.database.lock_hash(address.hash).await?;
        if let Some(parent) = parent {
            let claimed = sqlx::query(
                "DELETE FROM retired_children WHERE parent_key=$1 AND association_id=$2",
            )
            .bind(parent)
            .bind(generation)
            .execute(&mut *tx)
            .await
            .map_err(pg_error)?
            .rows_affected();
            if claimed == 0 {
                return tx.commit().await.map_err(pg_error);
            }
        }
        let removed = sqlx::query("DELETE FROM associations WHERE hash=$1 AND partition_id=$2 AND context=$3 AND ($4::bigint IS NULL OR id=$4)")
            .bind(address.hash.data().as_slice()).bind(partition.data().as_slice()).bind(address.context.data().as_slice()).bind(generation)
            .execute(&mut *tx).await.map_err(pg_error)?.rows_affected();
        let remaining: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM associations WHERE hash=$1)")
                .bind(address.hash.data().as_slice())
                .fetch_one(&mut *tx)
                .await
                .map_err(pg_error)?;
        if removed > 0
            && !remaining
            && let Some(key) = Self::live_key(&mut tx, address.hash).await?
        {
            let loaded = match self.load(&key).await {
                Ok(loaded) => Some(loaded),
                Err(error) if error.is_payload_not_found() => None,
                Err(error) => return Err(error),
            };
            if let Some((fragment, payload)) = loaded
                && fragment.flags & FragmentFlags::PayloadFragmented != 0
            {
                lore_storage::immutable_store::validate_fragment_list(&fragment, &payload)?;
                let payload = payload.to_aligned::<FragmentReference>();
                for reference in payload.as_type_slice::<FragmentReference>() {
                    sqlx::query("INSERT INTO retired_children(parent_key,hash,partition_id,context,association_id) SELECT $1,hash,partition_id,context,id FROM associations WHERE hash=$2 AND partition_id=$3 AND context=$4 ON CONFLICT DO NOTHING")
                        .bind(&key).bind(reference.hash.data().as_slice()).bind(partition.data().as_slice()).bind(address.context.data().as_slice())
                        .execute(&mut *tx).await.map_err(pg_error)?;
                }
            }
            sqlx::query("UPDATE payloads SET object_key=NULL WHERE hash=$1")
                .bind(address.hash.data().as_slice())
                .execute(&mut *tx)
                .await
                .map_err(pg_error)?;
            sqlx::query("UPDATE objects SET state='retired' WHERE object_key=$1")
                .bind(&key)
                .execute(&mut *tx)
                .await
                .map_err(pg_error)?;
        }
        tx.commit().await.map_err(pg_error)?;
        stats
            .num_fragments
            .fetch_add(removed as usize, Ordering::Relaxed);
        Ok(())
    }

    /// Resume durable retirement work. Pending uploads are deliberately not collected: an upload
    /// canceled at the caller may still complete at S3, so concurrent cleanup cannot safely erase it.
    async fn finish_retirements(&self, stats: &StoreObliterateStats) -> Result<(), StoreError> {
        loop {
            let row = sqlx::query("SELECT parent_key,hash,partition_id,context,association_id FROM retired_children LIMIT 1")
                .fetch_optional(&self.database.pool).await.map_err(pg_error)?;
            let Some(row) = row else {
                break;
            };
            let parent: String = row.get(0);
            let hash: Vec<u8> = row.get(1);
            let partition: Vec<u8> = row.get(2);
            let context: Vec<u8> = row.get(3);
            self.retire(
                Partition::from(partition.as_slice()),
                Address {
                    hash: Hash::from(hash.as_slice()),
                    context: Context::from(context.as_slice()),
                },
                Some(row.get(4)),
                Some(&parent),
                stats,
            )
            .await?;
        }
        loop {
            let key: Option<String> = sqlx::query_scalar("SELECT o.object_key FROM objects o WHERE o.state='retired' AND NOT EXISTS(SELECT 1 FROM retired_children c WHERE c.parent_key=o.object_key) LIMIT 1")
                .fetch_optional(&self.database.pool).await.map_err(pg_error)?;
            let Some(key) = key else {
                break;
            };
            // Every retired key is immutable and is never revived. A late successful delete can
            // therefore affect only this old generation, regardless of a new publication.
            #[cfg(feature = "test-util")]
            self.test_hooks.before_delete().await?;
            self.s3
                .delete_object(&self.bucket, &key, None)
                .await
                .map_err(|_error| {
                    StoreError::internal(
                        "S3 retired payload deletion failed; retirement retained for retry",
                    )
                })?;
            let removed =
                sqlx::query("DELETE FROM objects WHERE object_key=$1 AND state='retired'")
                    .bind(&key)
                    .execute(&self.database.pool)
                    .await
                    .map_err(pg_error)?
                    .rows_affected();
            stats
                .num_payloads
                .fetch_add(removed as usize, Ordering::Relaxed);
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableStore for S3PostgresStore {
    fn isolates_partitions(&self) -> bool {
        true
    }
    fn query_scope(&self) -> StoreMatch {
        StoreMatch::MatchFull
    }

    async fn is_available(self: Arc<Self>, timeout: std::time::Duration) -> bool {
        tokio::time::timeout(timeout, async {
            let (postgres, s3) = tokio::join!(
                sqlx::query("SELECT 1").execute(&self.database.pool),
                self.s3.bucket_exists(self.bucket.clone())
            );
            postgres.is_ok() && matches!(s3, Ok(true))
        })
        .await
        .unwrap_or(false)
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        if addresses.len() != results.len() || addresses.len() > 512 {
            return Err(StoreError::internal(
                "Invalid or oversized S3 PostgreSQL query batch",
            ));
        }
        let hashes: Vec<Vec<u8>> = addresses.iter().map(|a| a.hash.data().to_vec()).collect();
        let contexts: Vec<Vec<u8>> = addresses
            .iter()
            .map(|a| a.context.data().to_vec())
            .collect();
        let rows = sqlx::query("SELECT input.n FROM unnest($2::bytea[],$3::bytea[]) WITH ORDINALITY AS input(hash,context,n) JOIN payloads p ON p.hash=input.hash JOIN associations a ON a.hash=input.hash AND a.partition_id=$1 AND a.context=input.context WHERE p.object_key IS NOT NULL")
            .bind(partition.data().as_slice()).bind(hashes).bind(contexts).fetch_all(&self.database.pool).await.map_err(pg_error)?;
        results.fill(StoreMatchResult::default());
        for row in rows {
            let ordinal: i64 = row.get(0);
            let index = usize::try_from(ordinal - 1)
                .map_err(|_error| StoreError::internal("Invalid PostgreSQL batch ordinal"))?;
            let asked = addresses
                .get(index)
                .ok_or_else(|| StoreError::internal("Invalid PostgreSQL batch ordinal"))?;
            results[index] = StoreMatchResult {
                match_made: StoreMatch::MatchFull,
                partition,
                context: asked.context,
                stored_local: false,
                stored_durable: true,
            };
        }
        Ok(())
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let key = self
            .association(partition, address)
            .await?
            .ok_or_else(|| StoreError::from(lore_base::error::AddressNotFound::from(address)))?;
        let (fragment, payload) = match self.load(&key).await {
            Ok(loaded) => loaded,
            Err(error) if error.is_payload_not_found() => {
                self.invalidate_missing(address.hash, &key).await?;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        Ok(StoreGetData {
            fragment,
            payload: Some(payload),
            match_made: StoreMatch::MatchFull,
            partition,
            content: None,
        })
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let Some(key) = self.association(partition, address).await? else {
            return Ok(StoreGetData::default());
        };
        let output = match self.s3.head_object(&self.bucket, &key).await {
            Ok(output) => output,
            Err(lore_aws::aws_error::AwsError::AwsSdkError(error))
                if error.as_service_error().is_some_and(|e| e.is_not_found()) =>
            {
                self.invalidate_missing(address.hash, &key).await?;
                return Err(StoreError::from(lore_base::error::PayloadNotFound::from(
                    address.hash,
                )));
            }
            Err(_) => {
                return Err(StoreError::internal(
                    "Referenced S3 payload metadata could not be read",
                ));
            }
        };
        let mut fragment = from_object_metadata(output.metadata())
            .map_err(|_error| StoreError::internal("Invalid S3 fragment object metadata"))?;
        lore_storage::validate_fragment_metadata(&fragment)?;
        if output.content_length() != Some(i64::from(fragment.size_payload)) {
            return Err(StoreError::internal(
                "S3 fragment metadata and object length disagree",
            ));
        }
        fragment.flags |= FragmentFlags::PayloadStoredDurable.bits();
        Ok(StoreGetData::metadata(
            fragment,
            StoreMatch::MatchFull,
            partition,
        ))
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        mut fragment: Fragment,
        payload: Option<Bytes>,
        _force: bool,
    ) -> Result<(), StoreError> {
        sanitise_fragment_behavior_flags(&mut fragment);
        lore_storage::validate_fragment_metadata(&fragment)?;
        if let Some(payload) = payload.as_ref() {
            lore_storage::validate_fragment_payload(&fragment, payload.len())?;
        }
        let mut tx = self.database.lock_hash(address.hash).await?;
        if let Some(key) = Self::live_key(&mut tx, address.hash).await? {
            let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM associations WHERE hash=$1 AND partition_id=$2 AND context=$3)")
                .bind(address.hash.data().as_slice()).bind(partition.data().as_slice()).bind(address.context.data().as_slice()).fetch_one(&mut *tx).await.map_err(pg_error)?;
            if !exists && payload.is_none() {
                return Err(StoreError::internal(
                    "Payload buffer required to establish a new association",
                ));
            }
            self.renew_children(&mut tx, partition, address, &key)
                .await?;
            Self::associate(&mut tx, partition, address).await?;
            return tx.commit().await.map_err(pg_error);
        }
        tx.commit().await.map_err(pg_error)?;
        let payload = payload.ok_or_else(|| StoreError::internal("Payload buffer required"))?;
        let checksum_payload = payload.clone();
        let checksum = lore_base::lore_spawn_blocking!(move || lore_storage::hash::hash_slice(
            &checksum_payload
        ))
        .await
        .map_err(|_error| StoreError::internal("S3 representation checksum task failed"))?;
        let key = format!("{}/{}/{}", address.hash, checksum, uuid::Uuid::new_v4());
        sqlx::query("INSERT INTO objects(object_key,hash,state) VALUES($1,$2,'pending')")
            .bind(&key)
            .bind(address.hash.data().as_slice())
            .execute(&self.database.pool)
            .await
            .map_err(pg_error)?;
        #[cfg(feature = "test-util")]
        self.test_hooks.before_upload().await;
        self.s3
            .put_object(
                &self.bucket,
                &key,
                payload,
                Some(to_object_metadata(&fragment)),
            )
            .await
            .map_err(|_error| {
                StoreError::internal("S3 candidate upload failed; candidate remains unpublished")
            })?;
        #[cfg(feature = "test-util")]
        self.test_hooks.after_upload().await;
        let mut tx = self.database.lock_hash(address.hash).await?;
        let winner = Self::live_key(&mut tx, address.hash).await?;
        let exists = winner.is_some();
        self.renew_children(
            &mut tx,
            partition,
            address,
            winner.as_deref().unwrap_or(&key),
        )
        .await?;
        if !exists {
            sqlx::query("INSERT INTO payloads(hash,object_key) VALUES($1,$2) ON CONFLICT(hash) DO UPDATE SET object_key=EXCLUDED.object_key")
                .bind(address.hash.data().as_slice()).bind(&key).execute(&mut *tx).await.map_err(pg_error)?;
        }
        sqlx::query("UPDATE objects SET state=$2 WHERE object_key=$1")
            .bind(&key)
            .bind(if exists { "retired" } else { "live" })
            .execute(&mut *tx)
            .await
            .map_err(pg_error)?;
        Self::associate(&mut tx, partition, address).await?;
        #[cfg(feature = "test-util")]
        if self.test_hooks.fail_publish.load(Ordering::SeqCst) {
            return Err(StoreError::internal(
                "Synthetic fixture SQL publication failure",
            ));
        }
        tx.commit().await.map_err(pg_error)
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        _behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        let mut tx = self.database.lock_hash(source_address.hash).await?;
        let proof: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM associations a JOIN payloads p USING(hash) WHERE a.hash=$1 AND a.partition_id=$2 AND ($3::boolean OR a.context=$4) AND p.object_key IS NOT NULL)")
            .bind(source_address.hash.data().as_slice()).bind(source_partition.data().as_slice()).bind(source_address.context.is_zero()).bind(source_address.context.data().as_slice())
            .fetch_one(&mut *tx).await.map_err(pg_error)?;
        if !proof {
            return Err(StoreError::from(lore_base::error::AddressNotFound::from(
                source_address,
            )));
        }
        let key = Self::live_key(&mut tx, source_address.hash)
            .await?
            .ok_or_else(|| {
                StoreError::from(lore_base::error::AddressNotFound::from(source_address))
            })?;
        self.renew_children(
            &mut tx,
            destination_partition,
            Address {
                hash: source_address.hash,
                context: destination_context,
            },
            &key,
        )
        .await?;
        Self::associate(
            &mut tx,
            destination_partition,
            Address {
                hash: source_address.hash,
                context: destination_context,
            },
        )
        .await?;
        tx.commit().await.map_err(pg_error)
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        self.retire(partition, address, None, None, &stats).await?;
        #[cfg(feature = "test-util")]
        self.test_hooks.retired.wait().await;
        self.finish_retirements(&stats).await
    }
    async fn evict(
        self: Arc<Self>,
        _capacity: usize,
        _sync: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        Ok(0)
    }
    async fn compact(
        self: Arc<Self>,
        _size: usize,
        _at: Option<usize>,
        _sync: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        Ok(None)
    }
    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        None
    }
    fn max_query_batch(&self) -> Option<usize> {
        Some(512)
    }
    async fn flush(self: Arc<Self>, _sync: bool) -> Result<(), StoreError> {
        Ok(())
    }
    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        if heal {
            self.finish_retirements(&StoreObliterateStats::default())
                .await?;
        }
        let mut rows = sqlx::query("SELECT object_key FROM objects WHERE state='live'")
            .fetch(&self.database.pool);
        loop {
            let Some(row) = rows.try_next().await.map_err(pg_error)? else {
                break;
            };
            self.load(row.get::<&str, _>(0)).await?;
        }
        Ok(())
    }
}
