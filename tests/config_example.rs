use cirrusync::config::{Config, RecordType};

#[test]
fn distributed_example_configuration_stays_valid() {
    let source = include_str!("../config.example.toml");
    #[cfg(windows)]
    let source = source.replace(
        r#"api_token_file = "/etc/cirrusync/token""#,
        r#"api_token_file = "C:\\ProgramData\\cirrusync\\token""#,
    );
    let config = Config::from_toml(&source)
        .expect("the distributed example configuration must parse and validate");

    assert_eq!(config.records.len(), 1);
    assert_eq!(config.records[0].record_type, RecordType::A);
    assert!(!config.records[0].proxied);
}
