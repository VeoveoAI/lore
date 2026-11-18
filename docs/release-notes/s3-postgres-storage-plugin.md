---
type: feature
---

- `lore-server`: the `s3_postgres` native storage plugin: S3-compatible immutable payloads and PostgreSQL partition
  associations, lifecycle state and mutable references in an operator-provided schema. The immutable and mutable
  stores share a bounded pool; local resource locks remain scoped to one server. See
  [the backend contract](../../lore-s3-postgres/README.md) for publication, interruption, orphan-retention and
  explicit qualification limits.
