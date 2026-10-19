// Copyright 2026 VeoveoAI
// SPDX-License-Identifier: MIT
#[test]
fn schema_names_cannot_escape_the_operator_namespace() {
    use lore_s3_postgres::database_schema_valid;
    for valid in ["lore", "lore_test", "_test1"] {
        assert!(database_schema_valid(valid));
    }
    for invalid in [
        "",
        "public,lore",
        "lore;drop schema public",
        "1lore",
        "Lore",
        "pg_catalog\"",
        "lore.test",
    ] {
        assert!(!database_schema_valid(invalid));
    }
    assert!(!database_schema_valid(&"a".repeat(64)));
}
