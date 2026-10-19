// Copyright 2026 VeoveoAI
// SPDX-License-Identifier: MIT
use lore_server::plugins::PluginRegistry;
use lore_server::plugins::s3_postgres;

fn config() -> toml::Value {
    toml::from_str("postgres_url_env='LORE_POSTGRES_URL'\nschema='lore'\ns3_bucket='lore'\ns3_endpoint_url='http://storage.internal:9000'\ns3_force_path_style=true").unwrap()
}

#[test]
fn validates_only_nonsecret_and_unambiguous_storage_configuration() {
    let mut registry = PluginRegistry::new();
    s3_postgres::register(&mut registry);
    assert!(
        registry
            .validate_immutable_store_config("s3_postgres", &config())
            .is_ok()
    );
    assert!(
        registry
            .validate_mutable_store_config("s3_postgres", &config())
            .is_ok()
    );
    for (key, value) in [
        ("postgres_url_file", "/private/url"),
        ("schema", "public,lore"),
        ("postgres_url", "not-accepted-inline"),
        ("s3_bucket", ""),
    ] {
        let mut invalid = config();
        invalid
            .as_table_mut()
            .unwrap()
            .insert(key.into(), toml::Value::String(value.into()));
        assert!(
            registry
                .validate_immutable_store_config("s3_postgres", &invalid)
                .unwrap_err()
                .is_plugin_config_error()
        );
    }
    let mut file = config();
    file.as_table_mut().unwrap().remove("postgres_url_env");
    file.as_table_mut().unwrap().insert(
        "postgres_url_file".into(),
        toml::Value::String("/private/url".into()),
    );
    assert!(
        registry
            .validate_immutable_store_config("s3_postgres", &file)
            .is_ok()
    );
}
