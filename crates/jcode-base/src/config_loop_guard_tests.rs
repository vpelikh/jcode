//! Config tests for the loop-hygiene guard settings (`[loop_guard]`).
//!
//! Split out of `config_tests.rs` to keep that file under the test-size
//! ratchet. These pin the documented default through both the serde and (real)
//! TOML parse paths, so a partial or absent `[loop_guard]` table cannot
//! silently disable the guard by falling back to 0.

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
fn loop_guard_round_trips_a_custom_repeat_tool_threshold() {
    let cfg = LoopGuardConfig {
        repeat_tool_threshold: 2,
    };
    let json = serde_json::to_string(&cfg).unwrap();
    let back: LoopGuardConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.repeat_tool_threshold, 2);
}

#[test]
fn loop_guard_partial_toml_section_defaults_missing_repeat_tool_threshold() {
    // A `[loop_guard]` table that omits `repeat_tool_threshold` must keep the
    // guard enabled (4), not silently fall back to 0. Exercises the TOML path.
    let cfg: Config =
        toml::from_str("[loop_guard]\n").expect("empty loop_guard table must deserialize");
    assert_eq!(cfg.loop_guard.repeat_tool_threshold, 4);
}

#[test]
fn loop_guard_absent_toml_section_defaults_repeat_tool_threshold() {
    let cfg: Config = toml::from_str("[features]\nmermaid = false\n")
        .expect("config without a loop_guard table must deserialize");
    assert_eq!(cfg.loop_guard.repeat_tool_threshold, 4);
}
