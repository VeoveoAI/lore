# Native S3 + PostgreSQL storage

`s3_postgres` stores immutable bytes in a dedicated private S3-compatible bucket
and uses an existing PostgreSQL database for the native immutable index and mutable
references. It requires no DynamoDB service. Configure local native resource locks
and run one owning Lore server process; this is not a multi-server HA topology.

```toml
[immutable_store]
mode = "s3_postgres"
[mutable_store]
mode = "s3_postgres"
[lock_store]
mode = "local"
[plugins.s3_postgres]
postgres_url_env = "LORE_POSTGRES_URL"
# Alternatively select postgres_url_file, never both. The private file holds the URL.
schema = "lore"
s3_bucket = "lore"
s3_endpoint_url = "http://storage.internal:9000"
s3_region = "us-east-1"
s3_force_path_style = true
```

The operator creates the schema and its dedicated login in the existing database.
The login owns only that schema. Startup creates the native tables there; it never
creates a database, schema, role, migration ledger or compatibility data. Use a
secret connection URL in the named environment variable or a private `0600` file.
S3 authentication uses the native AWS SDK provider chain. The bucket must never
have enabled versioning. Startup checks `GetBucketVersioning`; serving also needs
`HeadBucket`, `PutObject`, `GetObject`, `HeadObject` and `DeleteObject`. The bucket
and its credentials must be private and dedicated to this store. Endpoint, TLS
and network policy are trusted installation choices, not user-agent credentials.

The immutable and mutable factories share one pool capped at 16 connections.
Acquisition has a 15-second timeout; statements have a 30-second timeout. Immutable
queries batch at most 512 addresses in one indexed SQL statement. Mutable lists
materialize at most 100,000 active references, then fail instead of truncating.
The typed-key byte, missing/zero reference semantics, CAS previous-value return,
copy proof rules and partition boundaries follow native Lore contracts.

## Publication and failure boundaries

Every upload uses a unique object key:
`<content hash>/<representation BLAKE3>/<random generation>`. The object carries
Lore's native fragment metadata alongside its bytes. PostgreSQL records the
candidate before upload, then atomically selects a canonical object key and
creates the partition/context association after a successful upload. Concurrent
writers keep the first published representation; a losing completed candidate
becomes retired. No writer overwrites a published key. Reads validate metadata,
bounded body size and the representation digest; content addressing and logical
fragment verification remain native Lore ingress/client responsibilities.

Per-hash transaction advisory locks serialize publication, copy and retirement;
their digest includes the schema and operation domain. A fresh `obliterate` always
removes the current association. The last association atomically clears the
canonical key and records retirement. Fragmented parents record exact child
association generations in the same transaction before publication of the
tombstone. Every successful put/copy renews even an existing association's
generation. Fragmented-parent publication reads the **winning** native reference
list, locks and renews every direct child tuple atomically with the parent
association, including children a writer's full-match query skipped. These are
additional S3 GETs; no duplicate representation discriminator is kept in SQL.
Missing children refuse publication/copy. Native `fragment_engine` finishes child
writes before its parent; native `write::copy_association` falls back to uploading
on copy refusal. A concurrent child removal can require the writer to retry its
children before the parent, rather than successfully publish a dangling tree.
Foreground cleanup removes old generations, preserving siblings,
other partitions and recreated tuples. This durable state allows interruption
and restart between recursive steps without a second controller.

Physical deletion uses only the retired generation's exact S3 key. A late delete
cannot remove a revived generation. Failed physical deletion returns an error and
retains SQL retirement state. Repeated obliteration or native `verify(heal=true)`
resumes retirement work; a repeated explicit obliteration still applies to any
current association. Cleanup may finish retirement from other interrupted calls.
`verify` validates live objects; healing additionally drains retired objects.

S3 and PostgreSQL do not share an atomic commit. A canceled/failed upload or a
failed SQL publication can leave an unpublished `pending` candidate and unreachable
S3 bytes. Pending candidates are deliberately retained: deleting them concurrently
with an upload that may still complete is unsafe. No automatic orphan collection,
age heuristic, cleanup lease, retry daemon or migration is provided. An observed
missing live object fails the read and invalidates only that exact canonical
generation; supplying the bytes again can repair it. If a missing fragmented
parent has already lost its reference list, invalidation cannot discover its
descendants for reclamation; those unreachable associations/bytes may remain.
No bytes are fabricated or complete reclamation claimed.

## Explicit real-backend qualification

```sh
cargo test --locked -p lore-s3-postgres --test unit
cargo test --locked -p lore-s3-postgres --features integration-tests --test integration -- --nocapture
cargo test --locked -p lore-server --test unit plugins::s3_postgres
cargo check --locked -p lore-server --example factory_branch_fixture
```

The integration command requires all of these environment variables and fails
when any is absent:

- `LORE_S3_POSTGRES_TEST_URL`: secret PostgreSQL URL for a dedicated test login.
- `LORE_S3_POSTGRES_TEST_SCHEMA=lore_test`: that login owns only this schema.
- `LORE_S3_POSTGRES_TEST_BUCKET`: dedicated unversioned test bucket; never `lore`.
- `LORE_S3_POSTGRES_TEST_ENDPOINT`: reachable S3 endpoint, path-style addressing.
- Native AWS SDK credentials authorized only for that test bucket.

The acceptance harness resets **only the native tables in `lore_test`** before its
scenario; do not run two harnesses against the same fixture. The database may be
the installation's existing database. The harness uses real PostgreSQL and S3,
native immutable/mutable conformance, deliberate concurrent operations and
per-instance synthetic fault/barrier controls. Its canceled/late upload and
failed-publication/deletion controls are synthetic failures around actual
database/object operations. They do not establish process-crash or dependency
restart recovery; qualify those separately with the exact deployed server.

For the stopped-server operator fixture, pass a native server configuration
directory containing `local.toml` to `factory_branch_fixture`. It opens the selected
stores through normal native construction, then emits the existing repository,
branch and protected-main JSON identities. Do not run the fixture against a store
owned by a live Lore server. Reset disposable development state and reseed from
retained source and trusted configuration when changing the backend; no conversion
from local packfiles is implemented.
