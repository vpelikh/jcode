//! Config tests for the loop-hygiene guard settings (`[loop_guard]`).
//!
//! Split out of `config_tests.rs` to keep that file under the test-size
//! ratchet. These pin one coherent contract: the guard thresholds keep their
//! documented defaults through both the serde and (real) TOML parse paths, so
//! a partial or absent `[loop_guard]` table cannot silently disable a guard by
//! falling back to 0.

use super::{Config, LoopGuardConfig};

#[test]
fn loop_guard_config_defaults_repeat_tool_threshold_to_four() {
    assert_eq!(LoopGuardConfig::default().repeat_tool_threshold, 4);
    // A Config default carries the same threshold, and it round-trips through
    // serde so a user can configure it without breaking the rest of the file.
    assert_eq!(Config::default().loop_guard.repeat_tool_threshold, 4);
    let cfg = Config::default();
    let json = serde_json::to_string(&cfg.loop_guard).unwrap();
    let back: LoopGuardConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.repeat_tool_threshold, 4);
}

#[test]
fn loop_guard_round_trips_a_custom_threshold() {
    let cfg = LoopGuardConfig {
        repeat_tool_threshold: 2,
        todo_stale_threshold: 5,
    };
    let json = serde_json::to_string(&cfg).unwrap();
    let back: LoopGuardConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.repeat_tool_threshold, 2);
}

#[test]
fn loop_guard_config_defaults_todo_stale_threshold_to_five() {
    assert_eq!(LoopGuardConfig::default().todo_stale_threshold, 5);
    assert_eq!(Config::default().loop_guard.todo_stale_threshold, 5);
    let cfg = Config::default();
    let json = serde_json::to_string(&cfg.loop_guard).unwrap();
    let back: LoopGuardConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.todo_stale_threshold, 5);
}

#[test]
fn loop_guard_round_trips_a_custom_todo_stale_threshold() {
    let cfg = LoopGuardConfig {
        repeat_tool_threshold: 4,
        todo_stale_threshold: 9,
    };
    let json = serde_json::to_string(&cfg).unwrap();
    let back: LoopGuardConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.todo_stale_threshold, 9);
}

#[test]
fn loop_guard_todo_stale_threshold_defaults_when_absent_from_config() {
    // An older config.toml written before this field existed must still load,
    // with the guard on by default rather than silently disabled at 0.
    let back: LoopGuardConfig = serde_json::from_str(r#"{"repeat_tool_threshold":3}"#).unwrap();
    assert_eq!(back.repeat_tool_threshold, 3);
    assert_eq!(back.todo_stale_threshold, 5);
}

#[test]
fn loop_guard_partial_toml_section_defaults_missing_fields() {
    // A real config.toml with a `[loop_guard]` table that predates
    // `todo_stale_threshold` must keep the guard enabled (5), not silently
    // fall back to 0. This exercises the actual TOML parse path Config uses.
    let cfg: Config = toml::from_str("[loop_guard]\nrepeat_tool_threshold = 2\n")
        .expect("partial loop_guard table must deserialize");
    assert_eq!(cfg.loop_guard.repeat_tool_threshold, 2);
    assert_eq!(cfg.loop_guard.todo_stale_threshold, 5);
}

#[test]
fn loop_guard_absent_toml_section_defaults_both_fields() {
    let cfg: Config = toml::from_str("[features]\nmermaid = false\n")
        .expect("config without a loop_guard table must deserialize");
    assert_eq!(cfg.loop_guard.repeat_tool_threshold, 4);
    assert_eq!(cfg.loop_guard.todo_stale_threshold, 5);
}
