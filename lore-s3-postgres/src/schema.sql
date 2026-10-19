-- The operator creates and owns the schema; all relations belong to that schema.
CREATE TABLE IF NOT EXISTS objects (
    object_key text PRIMARY KEY,
    hash bytea NOT NULL CHECK (octet_length(hash) = 32),
    state text NOT NULL CHECK (state IN ('pending', 'live', 'retired'))
);
CREATE INDEX IF NOT EXISTS objects_hash ON objects(hash);
CREATE TABLE IF NOT EXISTS payloads (
    hash bytea PRIMARY KEY CHECK (octet_length(hash) = 32),
    object_key text UNIQUE REFERENCES objects(object_key)
);
CREATE TABLE IF NOT EXISTS associations (
    id bigserial UNIQUE NOT NULL,
    hash bytea NOT NULL REFERENCES payloads(hash),
    partition_id bytea NOT NULL CHECK (octet_length(partition_id) = 16),
    context bytea NOT NULL CHECK (octet_length(context) = 16),
    PRIMARY KEY(hash, partition_id, context)
);
CREATE TABLE IF NOT EXISTS retired_children (
    parent_key text NOT NULL REFERENCES objects(object_key),
    hash bytea NOT NULL CHECK (octet_length(hash) = 32),
    partition_id bytea NOT NULL CHECK (octet_length(partition_id) = 16),
    context bytea NOT NULL CHECK (octet_length(context) = 16),
    association_id bigint NOT NULL,
    PRIMARY KEY(parent_key, association_id)
);
CREATE TABLE IF NOT EXISTS mutable_refs (
    partition_id bytea NOT NULL CHECK (octet_length(partition_id) = 16),
    key bytea NOT NULL CHECK (octet_length(key) = 32),
    value bytea NOT NULL CHECK (octet_length(value) = 32),
    PRIMARY KEY(partition_id, key)
);
