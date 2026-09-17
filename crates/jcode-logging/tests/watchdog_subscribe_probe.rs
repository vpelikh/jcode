//! End-to-end probe: a real stalled thread must deliver a `WatchdogEvent::Stall`
//! to a [`watchdog::subscribe`] receiver, proving the log event and the live
//! subscription stay in lockstep.
//!
//! Run with: cargo test -p jcode-logging --test watchdog_subscribe_probe -- --ignored

use std::time::{Duration, Instant};

#[test]
#[ignore = "takes ~15s; exercises the real monitor thread and broadcast channel"]
fn stalled_process_delivers_watchdog_stall_to_subscriber() {
    let dir = std::env::temp_dir().join(format!("jcode-wd-sub-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    unsafe {
        std::env::set_var("HOME", &dir);
        std::env::set_var("JCODE_WATCHDOG_STALL_SECS", "5");
        std::env::set_var("JCODE_WATCHDOG_HEARTBEAT_SECS", "5");
    }

    jcode_logging::init();
    // Subscribe BEFORE beginning the work, so no stall event published earlier
    // is missed.
    let mut rx = jcode_logging::watchdog::subscribe();
    let _work = jcode_logging::watchdog::begin_work("probe.subscribe.phase");
    jcode_logging::watchdog::set_detail("probe subscribe detail");

    // Simulate the hang: work in flight, no beats follow.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut found = false;
    while Instant::now() < deadline {
        if let Ok(jcode_logging::watchdog::WatchdogEvent::Stall {
            phase,
            detail,
            stalled_secs,
            ..
        }) = rx.try_recv()
        {
            assert_eq!(phase, "probe.subscribe.phase");
            assert_eq!(detail, "probe subscribe detail");
            assert!(stalled_secs >= 5, "stall should be >= threshold");
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(found, "subscriber never received a WatchdogEvent::Stall");
}