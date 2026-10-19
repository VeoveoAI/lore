// Copyright 2026 VeoveoAI
// SPDX-License-Identifier: MIT
#![cfg(feature = "integration-tests")]
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use lore_aws::clients::AwsClientBuilder;
use lore_aws::clients::HttpClientSettings;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::FragmentReference;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_s3_postgres::Database;
use lore_s3_postgres::PostgresMutableStore;
use lore_s3_postgres::S3PostgresStore;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::store_types::KeyType;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("Explicit integration test requires {name}"))
}

async fn store() -> Arc<S3PostgresStore> {
    let schema = required("LORE_S3_POSTGRES_TEST_SCHEMA");
    assert_eq!(
        schema, "lore_test",
        "acceptance requires the dedicated disposable lore_test schema"
    );
    let database = Database::connect(&required("LORE_S3_POSTGRES_TEST_URL"), &schema)
        .await
        .expect("connect isolated test schema in existing database");
    let bucket = required("LORE_S3_POSTGRES_TEST_BUCKET");
    assert_ne!(
        bucket, "lore",
        "integration tests require a dedicated test bucket"
    );
    let s3 = s3(&bucket).await;
    S3PostgresStore::new(database, s3, bucket)
        .await
        .expect("open real PG/S3 store")
}
async fn s3(bucket: &str) -> lore_aws::s3::S3 {
    AwsClientBuilder::builder()
        .with_http_settings(&HttpClientSettings::default())
        .maybe_endpoint(Some(required("LORE_S3_POSTGRES_TEST_ENDPOINT")))
        .maybe_region(Some("us-east-1".into()))
        .build_config()
        .await
        .s3_with_path_style(true)
        .ensure_bucket(bucket)
        .build()
        .await
        .expect("open dedicated test bucket")
}
fn partition() -> Partition {
    uuid::Uuid::new_v4().into()
}
fn address(bytes: &[u8]) -> Address {
    Address {
        hash: lore_storage::hash::hash_slice(bytes),
        context: uuid::Uuid::new_v4().into(),
    }
}
fn fragment(bytes: &[u8]) -> Fragment {
    Fragment {
        flags: 0,
        size_content: bytes.len() as u64,
        size_payload: bytes.len() as u32,
    }
}
async fn put(
    store: &Arc<S3PostgresStore>,
    partition: Partition,
    address: Address,
    bytes: &'static [u8],
) {
    store
        .clone()
        .put(
            partition,
            address,
            fragment(bytes),
            Some(Bytes::from_static(bytes)),
            false,
        )
        .await
        .expect("put payload");
}
async fn wait(notify: &tokio::sync::Notify) {
    tokio::time::timeout(Duration::from_secs(15), notify.notified())
        .await
        .expect("fixture barrier reached");
}

async fn native_conformance_against_real_postgres_and_seaweed() {
    let store = store().await;
    lore_storage::conformance::verify_immutable_store(
        store.clone(),
        lore_storage::conformance::Capabilities::new("s3_postgres"),
    )
    .await;
    lore_storage::mutable_conformance::verify_mutable_store(
        PostgresMutableStore::new(store.database()),
        lore_storage::mutable_conformance::Capabilities::new("postgres"),
    )
    .await;
}

async fn concurrent_cas_has_one_winner_and_zero_removes() {
    let store = store().await;
    let mutable = PostgresMutableStore::new(store.database());
    let p = partition();
    let key = address(b"cas").hash;
    let one = address(b"one").hash;
    let two = address(b"two").hash;
    let (a, b) = tokio::join!(
        mutable.clone().compare_and_swap(
            p,
            key,
            Hash::default(),
            one,
            KeyType::BranchLatestPointer
        ),
        mutable.clone().compare_and_swap(
            p,
            key,
            Hash::default(),
            two,
            KeyType::BranchLatestPointer
        )
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_ne!(a.is_zero(), b.is_zero());
    let winner = mutable
        .clone()
        .load(p, key, KeyType::BranchLatestPointer)
        .await
        .unwrap();
    assert!(winner == one || winner == two);
    assert_eq!(
        mutable
            .clone()
            .compare_and_swap(
                p,
                key,
                winner,
                Hash::default(),
                KeyType::BranchLatestPointer
            )
            .await
            .unwrap(),
        winner
    );
    assert!(
        mutable
            .load(p, key, KeyType::BranchLatestPointer)
            .await
            .unwrap_err()
            .is_address_not_found()
    );
}

async fn delayed_upload_cannot_overwrite_the_published_representation() {
    let store = store().await;
    let p = partition();
    let data = Bytes::from(vec![93_u8; 4096]);
    let a = address(&data);
    store.test_hooks.upload.pause.store(true, Ordering::SeqCst);
    let worker_store = store.clone();
    let first_data = data.clone();
    let first = lore_base::lore_spawn!(async move {
        worker_store
            .put(p, a, fragment(&first_data), Some(first_data), false)
            .await
    });
    wait(&store.test_hooks.upload.reached).await;
    let (compressed_fragment, compressed) = lore_storage::compress::compress(
        fragment(&data),
        &data,
        lore_storage::CompressionMode::Zstd,
    )
    .unwrap();
    store
        .clone()
        .put(p, a, compressed_fragment, Some(compressed.clone()), false)
        .await
        .unwrap();
    store.test_hooks.upload.resume.notify_one();
    first.await.unwrap().unwrap();
    let actual = store.clone().get(p, a).await.unwrap();
    assert_eq!(actual.payload.unwrap(), compressed);
    assert_eq!(
        actual.fragment.size_payload,
        compressed_fragment.size_payload
    );
    let (_, expanded) = lore_storage::expand_payload(actual.fragment, compressed).unwrap();
    assert_eq!(expanded, data);
    store.verify(true).await.unwrap();
}

async fn canceled_and_failed_publication_remains_unreachable_across_reopen() {
    let store = store().await;
    let p = partition();
    let a = address(b"canceled");
    store
        .test_hooks
        .uploaded
        .pause
        .store(true, Ordering::SeqCst);
    let worker = store.clone();
    let task = lore_base::lore_spawn!(async move {
        worker
            .put(
                p,
                a,
                fragment(b"canceled"),
                Some(Bytes::from_static(b"canceled")),
                false,
            )
            .await
    });
    wait(&store.test_hooks.uploaded.reached).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(
        store
            .clone()
            .get(p, a)
            .await
            .unwrap_err()
            .is_address_not_found()
    );
    store.test_hooks.fail_publish.store(true, Ordering::SeqCst);
    assert!(
        store
            .clone()
            .put(
                p,
                a,
                fragment(b"canceled"),
                Some(Bytes::from_static(b"canceled")),
                false
            )
            .await
            .is_err()
    );
    assert!(
        store
            .clone()
            .get(p, a)
            .await
            .unwrap_err()
            .is_address_not_found()
    );
    let reopened = self::store().await;
    put(&reopened, p, a, b"canceled").await;
    assert_eq!(
        reopened.get(p, a).await.unwrap().payload.unwrap(),
        b"canceled".as_slice()
    );
}

async fn late_delete_and_retry_cannot_delete_a_revived_generation() {
    let store = store().await;
    let p = partition();
    let a = address(b"revive");
    put(&store, p, a, b"revive").await;
    store.test_hooks.delete.pause.store(true, Ordering::SeqCst);
    let worker = store.clone();
    let old = lore_base::lore_spawn!(async move {
        worker
            .obliterate(p, a, Arc::new(StoreObliterateStats::default()))
            .await
    });
    wait(&store.test_hooks.delete.reached).await;
    put(&store, p, a, b"revive").await;
    store.test_hooks.delete.resume.notify_one();
    old.await.unwrap().unwrap();
    assert_eq!(
        store.clone().get(p, a).await.unwrap().payload.unwrap(),
        b"revive".as_slice()
    );
    store.test_hooks.fail_delete.store(true, Ordering::SeqCst);
    assert!(
        store
            .clone()
            .obliterate(p, a, Arc::new(StoreObliterateStats::default()))
            .await
            .is_err()
    );
    put(&store, p, a, b"revive").await;
    store.test_hooks.fail_delete.store(false, Ordering::SeqCst);
    store.clone().verify(true).await.unwrap();
    assert_eq!(
        store.clone().get(p, a).await.unwrap().payload.unwrap(),
        b"revive".as_slice()
    );
    store
        .clone()
        .obliterate(p, a, Arc::new(StoreObliterateStats::default()))
        .await
        .unwrap();
    assert!(store.get(p, a).await.unwrap_err().is_address_not_found());
}

async fn copy_requires_native_exact_or_zero_context_source_proof() {
    let store = store().await;
    let p = partition();
    let other = partition();
    let a = address(b"proof");
    put(&store, p, a, b"proof").await;
    let wrong = Address {
        context: uuid::Uuid::new_v4().into(),
        ..a
    };
    assert!(
        store
            .clone()
            .copy(p, wrong, other, a.context, CopyBehavior::default())
            .await
            .unwrap_err()
            .is_address_not_found()
    );
    assert!(
        store
            .clone()
            .copy(other, a, p, a.context, CopyBehavior::default())
            .await
            .unwrap_err()
            .is_address_not_found()
    );
    store
        .clone()
        .copy(
            p,
            Address {
                context: Context::default(),
                ..a
            },
            other,
            a.context,
            CopyBehavior::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        store.get(other, a).await.unwrap().payload.unwrap(),
        b"proof".as_slice()
    );
}

async fn fragmented(store: &Arc<S3PostgresStore>) -> (Partition, Address, Address, Address) {
    let p = partition();
    let context: Context = uuid::Uuid::new_v4().into();
    let mut leaves = Vec::new();
    for byte in [61_u8, 62] {
        let data = Bytes::from(vec![byte; 128]);
        let a = Address {
            context,
            ..address(&data)
        };
        store
            .clone()
            .put(p, a, fragment(&data), Some(data), false)
            .await
            .unwrap();
        leaves.push(a);
    }
    let references = [
        FragmentReference {
            hash: leaves[0].hash,
            offset_content: 0,
        },
        FragmentReference {
            hash: leaves[1].hash,
            offset_content: 128,
        },
    ];
    use zerocopy::IntoBytes;
    let payload = Bytes::copy_from_slice(references.as_bytes());
    let parent = Address {
        context,
        ..address(&[vec![61_u8; 128], vec![62_u8; 128]].concat())
    };
    let fragment = Fragment {
        flags: FragmentFlags::PayloadFragmented.bits(),
        size_payload: payload.len() as u32,
        size_content: 256,
    };
    store
        .clone()
        .put(p, parent, fragment, Some(payload), false)
        .await
        .unwrap();
    (p, parent, leaves[0], leaves[1])
}

async fn recursive_obliteration_preserves_shared_and_renewed_child_associations() {
    let store = store().await;
    let (p, parent, one, two) = fragmented(&store).await;
    let shared = partition();
    store
        .clone()
        .copy(p, one, shared, one.context, CopyBehavior::default())
        .await
        .unwrap();
    store
        .clone()
        .obliterate(p, parent, Arc::new(StoreObliterateStats::default()))
        .await
        .unwrap();
    assert!(
        store
            .clone()
            .get(p, parent)
            .await
            .unwrap_err()
            .is_address_not_found()
    );
    assert!(
        store
            .clone()
            .get(p, one)
            .await
            .unwrap_err()
            .is_address_not_found()
    );
    assert!(
        store
            .clone()
            .get(p, two)
            .await
            .unwrap_err()
            .is_address_not_found()
    );
    assert_eq!(
        store
            .clone()
            .get(shared, one)
            .await
            .unwrap()
            .payload
            .unwrap(),
        vec![61_u8; 128]
    );
    let (p, parent, one, two) = fragmented(&store).await;
    store.test_hooks.retired.pause.store(true, Ordering::SeqCst);
    let worker = store.clone();
    let task = lore_base::lore_spawn!(async move {
        worker
            .obliterate(p, parent, Arc::new(StoreObliterateStats::default()))
            .await
    });
    wait(&store.test_hooks.retired.reached).await;
    // Both tuples already exist. Native put and copy must renew their generations,
    // without deleting/recreating a row through SQL test setup.
    let data = Bytes::from(vec![61_u8; 128]);
    store
        .clone()
        .put(p, one, fragment(&data), Some(data), false)
        .await
        .unwrap();
    store
        .clone()
        .copy(p, two, p, two.context, CopyBehavior::default())
        .await
        .unwrap();
    store.test_hooks.retired.resume.notify_one();
    task.await.unwrap().unwrap();
    assert_eq!(
        store.clone().get(p, one).await.unwrap().payload.unwrap(),
        vec![61_u8; 128]
    );
    assert_eq!(
        store.get(p, two).await.unwrap().payload.unwrap(),
        vec![62_u8; 128]
    );
}

async fn query_skipped_children_survive_revived_parent_publication() {
    let store = store().await;
    let (p, parent, one, two) = fragmented(&store).await;
    let representation = store.clone().get(p, parent).await.unwrap();
    store.test_hooks.retired.pause.store(true, Ordering::SeqCst);
    let worker = store.clone();
    let cleanup = lore_base::lore_spawn!(async move {
        worker
            .obliterate(p, parent, Arc::new(StoreObliterateStats::default()))
            .await
    });
    wait(&store.test_hooks.retired.reached).await;
    let mut matches = [Default::default(); 2];
    store
        .clone()
        .query(p, &[one, two], &mut matches)
        .await
        .unwrap();
    assert!(
        matches
            .iter()
            .all(|r| r.match_made == lore_storage::StoreMatch::MatchFull)
    );
    // The native writer sees both leaves already durable and skips their puts.
    store
        .clone()
        .put(
            p,
            parent,
            representation.fragment,
            representation.payload,
            false,
        )
        .await
        .unwrap();
    store.test_hooks.retired.resume.notify_one();
    cleanup.await.unwrap().unwrap();
    assert!(
        store
            .clone()
            .get(p, parent)
            .await
            .unwrap()
            .payload
            .is_some()
    );
    assert_eq!(
        store.clone().get(p, one).await.unwrap().payload.unwrap(),
        vec![61_u8; 128]
    );
    assert_eq!(
        store.get(p, two).await.unwrap().payload.unwrap(),
        vec![62_u8; 128]
    );
}

async fn fragmented_parent_requires_completed_child_writes_and_copy_falls_back() {
    let store = store().await;
    let (p, parent, one, two) = fragmented(&store).await;
    let representation = store.clone().get(p, parent).await.unwrap();
    let destination = partition();
    let context = uuid::Uuid::new_v4().into();
    let destination_parent = Address { context, ..parent };
    // Native write::copy_association treats refusal as upload fallback. Root uploads cannot
    // publish a dangling list either; fragment_engine completes its child writes first.
    assert!(
        store
            .clone()
            .copy(p, parent, destination, context, CopyBehavior::default())
            .await
            .unwrap_err()
            .is_address_not_found()
    );
    assert!(
        store
            .clone()
            .put(
                destination,
                destination_parent,
                representation.fragment,
                representation.payload.clone(),
                false
            )
            .await
            .unwrap_err()
            .is_address_not_found()
    );
    assert!(
        store
            .clone()
            .get(destination, destination_parent)
            .await
            .unwrap_err()
            .is_address_not_found()
    );
    for child in [one, two] {
        store
            .clone()
            .copy(p, child, destination, context, CopyBehavior::default())
            .await
            .unwrap();
    }
    store
        .clone()
        .copy(p, parent, destination, context, CopyBehavior::default())
        .await
        .unwrap();
    assert_eq!(
        store
            .clone()
            .get(destination, destination_parent)
            .await
            .unwrap()
            .payload,
        representation.payload
    );
    store
        .clone()
        .obliterate(p, parent, Arc::new(StoreObliterateStats::default()))
        .await
        .unwrap();
    // The copied parent still owns the global object. Remove p's children explicitly.
    for child in [one, two] {
        store
            .clone()
            .obliterate(p, child, Arc::new(StoreObliterateStats::default()))
            .await
            .unwrap();
    }
    assert!(
        store
            .clone()
            .put(
                p,
                parent,
                representation.fragment,
                representation.payload.clone(),
                false
            )
            .await
            .unwrap_err()
            .is_address_not_found()
    );
    for (child, byte) in [(one, 61_u8), (two, 62_u8)] {
        let data = Bytes::from(vec![byte; 128]);
        store
            .clone()
            .put(p, child, fragment(&data), Some(data), false)
            .await
            .unwrap();
    }
    store
        .clone()
        .put(
            p,
            parent,
            representation.fragment,
            representation.payload,
            false,
        )
        .await
        .unwrap();
    assert!(store.get(p, parent).await.unwrap().payload.is_some());
}

async fn missing_and_same_length_corrupt_objects_never_return_successful_bytes() {
    let store = store().await;
    let p = partition();
    let data = b"integrity";
    let a = address(data);
    put(&store, p, a, data).await;
    let pool = sqlx::PgPool::connect(&required("LORE_S3_POSTGRES_TEST_URL"))
        .await
        .unwrap();
    let key: String = sqlx::query_scalar("SELECT object_key FROM lore_test.payloads WHERE hash=$1")
        .bind(a.hash.data().as_slice())
        .fetch_one(&pool)
        .await
        .unwrap();
    let bucket = required("LORE_S3_POSTGRES_TEST_BUCKET");
    let s3 = s3(&bucket).await;
    s3.put_object(
        &bucket,
        &key,
        Bytes::from_static(b"INTEGRITY"),
        Some(lore_aws::store::object_metadata::to_object_metadata(
            &fragment(data),
        )),
    )
    .await
    .unwrap();
    assert!(
        store.clone().get(p, a).await.is_err(),
        "representation checksum must detect same-length corruption"
    );
    s3.delete_object(&bucket, &key, None).await.unwrap();
    assert!(
        store
            .clone()
            .get(p, a)
            .await
            .unwrap_err()
            .is_payload_not_found()
    );
    let mut result = [Default::default()];
    store.clone().query(p, &[a], &mut result).await.unwrap();
    assert_eq!(result[0].match_made, lore_storage::StoreMatch::MatchNone);
    put(&store, p, a, data).await;
    assert_eq!(
        store.clone().get(p, a).await.unwrap().payload.unwrap(),
        data.as_slice()
    );
    store.verify(true).await.unwrap();
}

/// One explicit destructive acceptance scenario owns the dedicated schema. Internal operations
/// race deliberately; separate test runners must not share this fixture while it is running.
#[tokio::test(flavor = "multi_thread")]
async fn real_backend_acceptance() {
    lore_base::runtime::LORE_CONTEXT
        .scope(Arc::new(()), acceptance_scenario())
        .await;
}

async fn acceptance_scenario() {
    let store = store().await;
    let pool = sqlx::PgPool::connect(&required("LORE_S3_POSTGRES_TEST_URL"))
        .await
        .unwrap();
    sqlx::raw_sql("TRUNCATE lore_test.retired_children,lore_test.associations,lore_test.payloads,lore_test.objects,lore_test.mutable_refs RESTART IDENTITY")
        .execute(&pool).await.expect("reset only dedicated disposable acceptance tables");
    drop(store);
    native_conformance_against_real_postgres_and_seaweed().await;
    concurrent_cas_has_one_winner_and_zero_removes().await;
    delayed_upload_cannot_overwrite_the_published_representation().await;
    canceled_and_failed_publication_remains_unreachable_across_reopen().await;
    late_delete_and_retry_cannot_delete_a_revived_generation().await;
    copy_requires_native_exact_or_zero_context_source_proof().await;
    recursive_obliteration_preserves_shared_and_renewed_child_associations().await;
    query_skipped_children_survive_revived_parent_publication().await;
    fragmented_parent_requires_completed_child_writes_and_copy_falls_back().await;
    missing_and_same_length_corrupt_objects_never_return_successful_bytes().await;
}
