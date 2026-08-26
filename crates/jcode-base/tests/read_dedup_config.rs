//! Validation that the shipped config template and the `read_dedup` tool flag
//! are wired correctly through the public `Config` interface.
//!
//! This is an integration test (separate from the `config` unit-tests module)
//! so it compiles even if the unit test module is temporarily broken, and it
//! exercises the same public surface a user hits: parse the shipped template,
//! confirm the default, and confirm a user override round-trips.

#[test]
fn default_config_template_parses_with_read_dedup() {
    use jcode_base::config::Config;
    let template = Config::default_config_file_contents();
    let config =
        toml::from_str::<Config>(&template).expect("the shipped config template must parse");
    // Default is on (the shipped template carries true).
    assert!(config.tools.read_dedup, "read_dedup must default to true");
}

#[test]
fn read_dedup_false_override_round_trips() {
    use jcode_base::config::Config;
    // A standalone `[tools]` section overriding read_dedup to false (turning the
    // default-on feature off) must parse and be honored through the public Config.
    let standalone = "[tools]\nread_dedup = false\n";
    let parsed = toml::from_str::<Config>(standalone)
        .expect("standalone [tools] read_dedup=false must parse");
    assert!(!parsed.tools.read_dedup, "read_dedup=false must be honored");
}