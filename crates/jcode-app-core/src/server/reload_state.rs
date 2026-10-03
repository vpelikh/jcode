use super::{has_live_listener, is_server_ready};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[cfg(target_os = "linux")]
const RELOAD_HANDOFF_EVENT_POLL_MS: i32 = 100;

pub fn reload_marker_path() -> PathBuf {
    crate::storage::runtime_dir().join("jcode.reload")
}

pub fn write_reload_marker() {
    ReloadState {
        request_id: "unknown".to_string(),
        hash: "unknown".to_string(),
        phase: ReloadPhase::Starting,
        pid: std::process::id(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        detail: None,
    }
    .write();
}

pub fn clear_reload_marker() {
    let _ = std::fs::remove_file(reload_marker_path());
}

pub(super) fn clear_reload_marker_if_stale_for_pid(current_pid: u32) {
    if let Some(state) = ReloadState::load() {
        if state.phase == ReloadPhase::Starting && state.pid == current_pid {
            return;
        }
        clear_reload_marker();
    }
}

pub fn reload_marker_exists() -> bool {
    reload_marker_path().exists()
}

pub fn reload_marker_active(max_age: Duration) -> bool {
    matches!(
        recent_reload_state(max_age),
        Some(state)
            if matches!(state.phase, ReloadPhase::Starting | ReloadPhase::SocketReady)
    )
}

/// Whether process liveness is actually observable on this platform.
///
/// `reload_process_alive` can only distinguish a live owner from a dead one on
/// unix. Elsewhere it reports every nonzero pid as alive, so a `Starting` marker
/// whose owner died before publishing `SocketReady`/`Failed` would look
/// perpetually live and must NOT be extended past `max_age`: doing so would pin
/// clients in `Waiting` (and keep `server_reload_starting()` true, rejecting new
/// turns) for the full `RELOAD_MARKER_HARD_MAX_AGE` on a failed reload. On unix
/// liveness is real, so a genuinely live owner may legitimately hold the handoff
/// open past `max_age`.
const RELOAD_OWNER_LIVENESS_OBSERVABLE: bool = cfg!(unix);

pub fn recent_reload_state(max_age: Duration) -> Option<ReloadState> {
    match observe_reload_marker(max_age) {
        ReloadMarkerObservation::Usable(state) => Some(state),
        ReloadMarkerObservation::HungLiveOwner | ReloadMarkerObservation::Stale => None,
    }
}

/// Classification of the on-disk reload marker for a given `max_age`, produced
/// from a *single* read+parse so callers never re-read the marker file.
enum ReloadMarkerObservation {
    /// A usable marker: fresh, or (on unix) a live owner still within the hard
    /// cap.
    Usable(ReloadState),
    /// A live `Starting` owner that outlived `RELOAD_MARKER_HARD_MAX_AGE`.
    /// Waiting clients must stop, but the file is deliberately kept (not
    /// deleted) so a genuinely slow owner can still publish `SocketReady` and
    /// finish the handoff.
    HungLiveOwner,
    /// Absent, unreadable, or stale. The file is cleared when present.
    Stale,
}

/// Pure staleness verdict for a marker, decided from values the caller has
/// already read. Kept separate from file I/O and platform liveness so the
/// decision — including the non-unix "liveness is unobservable" branch — can be
/// checked on every platform, independent of the host's `cfg(unix)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkerVerdict {
    /// Within `max_age`: use the marker.
    Fresh,
    /// Past `max_age` but a live owner (liveness observable) is within the hard
    /// cap: keep honoring the marker.
    Extended,
    /// Past the hard cap with a live observable owner: stop waiting but keep the
    /// file.
    HungLiveOwner,
    /// Stale: clear the file and treat as no marker.
    Stale,
}

/// Decide a marker's staleness from already-read values.
///
/// `liveness_observable` mirrors [`RELOAD_OWNER_LIVENESS_OBSERVABLE`] (only unix
/// can tell a live owner from a dead one). When it is false the live-owner
/// extension is skipped entirely, so a failed reload whose owner died cannot pin
/// clients in `Waiting` for the full cap — the exact non-unix regression this
/// guards.
fn classify_marker(
    phase: ReloadPhase,
    elapsed: Duration,
    max_age: Duration,
    liveness_observable: bool,
    owner_alive: bool,
) -> MarkerVerdict {
    if elapsed <= max_age {
        return MarkerVerdict::Fresh;
    }
    // The max-age is only a fallback guard for stale markers. While the reload
    // process that owns the marker is still alive (and that is observable), the
    // handoff is genuinely in progress and the marker must NOT be cleared or
    // treated as stale: a reload that checkpoints and flushes many sessions can
    // hold the `Starting` phase well past `max_age` (observed ~39s vs a 30s TTL).
    // Expiring the marker mid-reload makes every waiting client abandon the
    // handoff, and the replacement server then boots unable to publish a
    // socket-ready state (its `publish_reload_socket_ready` finds "no reload
    // marker").
    //
    // A live owner only extends the marker up to `RELOAD_MARKER_HARD_MAX_AGE`.
    // Past that the owner is presumed hung/deadlocked (a healthy reload finishes
    // in well under a minute), so we stop pinning clients on a stuck handoff and
    // let the normal reconnect path take over.
    if liveness_observable && phase == ReloadPhase::Starting && owner_alive {
        if elapsed <= RELOAD_MARKER_HARD_MAX_AGE {
            return MarkerVerdict::Extended;
        }
        // Alive but past the cap: stop waiting WITHOUT deleting the marker.
        return MarkerVerdict::HungLiveOwner;
    }
    MarkerVerdict::Stale
}

/// Read the marker once and classify it, clearing the file only when it is
/// genuinely stale. Shared by [`recent_reload_state`] and
/// [`inspect_reload_wait_status`] so the marker is read and parsed at most once
/// per call (the wait-status path also needs the hung-owner verdict).
fn observe_reload_marker(max_age: Duration) -> ReloadMarkerObservation {
    let path = reload_marker_path();
    let Some(state) = ReloadState::load() else {
        return ReloadMarkerObservation::Stale;
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return ReloadMarkerObservation::Stale;
    };
    let Ok(modified) = metadata.modified() else {
        let _ = std::fs::remove_file(&path);
        return ReloadMarkerObservation::Stale;
    };
    let Ok(elapsed) = modified.elapsed() else {
        // Clock skew or an unreadable mtime: treat as fresh rather than guessing.
        return ReloadMarkerObservation::Usable(state);
    };
    match classify_marker(
        state.phase,
        elapsed,
        max_age,
        RELOAD_OWNER_LIVENESS_OBSERVABLE,
        reload_process_alive(state.pid),
    ) {
        MarkerVerdict::Fresh | MarkerVerdict::Extended => ReloadMarkerObservation::Usable(state),
        MarkerVerdict::HungLiveOwner => ReloadMarkerObservation::HungLiveOwner,
        MarkerVerdict::Stale => {
            let _ = std::fs::remove_file(&path);
            ReloadMarkerObservation::Stale
        }
    }
}

/// Upper bound on how long a live owner's in-progress `Starting` marker is
/// honored beyond the caller's `max_age`. A reload that has not published
/// `SocketReady`/`Failed` within this window is treated as hung so waiting
/// clients fall back to a normal reconnect instead of waiting forever.
pub const RELOAD_MARKER_HARD_MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// Whether an in-progress reload should suppress the server idle-exit timer.
///
/// During a reload the server can transiently have zero connected clients while
/// it still owns the marker and is shutting sessions down; the idle monitor must
/// not exit mid-reload or it races the replacement server (and strands waiting
/// clients that are still parked on the handoff). Only a `Starting` marker means
/// a reload is genuinely in progress, so idle-exit is suppressed.
///
/// A `SocketReady` marker is deliberately *not* considered here: it means the
/// handoff already succeeded (the replacement server is accepting connections).
/// Since reload is exec-based it preserves the pid, and nothing clears that
/// marker in normal operation, so honoring `SocketReady` here would suppress
/// idle-exit for up to `RELOAD_MARKER_HARD_MAX_AGE` after every completed reload
/// — a regression that keeps an otherwise-idle server alive. Once the replacement
/// server is up its own fresh idle timer governs, exactly as before a reload.
///
/// Bounded by [`RELOAD_MARKER_HARD_MAX_AGE`] (10 min): a `Starting` marker whose
/// owner is hung past the cap stops counting as active (`recent_reload_state`
/// hides the hung-live-owner verdict), so a stuck reload cannot suppress
/// idle-exit forever.
pub fn reload_suppresses_idle_shutdown() -> bool {
    matches!(
        recent_reload_state(RELOAD_MARKER_HARD_MAX_AGE),
        Some(state) if state.phase == ReloadPhase::Starting
    )
}

pub fn write_reload_state(
    request_id: &str,
    hash: &str,
    phase: ReloadPhase,
    detail: Option<String>,
) {
    ReloadState {
        request_id: request_id.to_string(),
        hash: hash.to_string(),
        phase,
        pid: std::process::id(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        detail,
    }
    .write();
}

/// Like [`write_reload_state`] but with an explicit `pid`. Test-only: lets tests
/// simulate a marker owned by a process that is no longer alive.
#[cfg(test)]
pub fn write_reload_state_with_pid(
    request_id: &str,
    hash: &str,
    phase: ReloadPhase,
    detail: Option<String>,
    pid: u32,
) {
    ReloadState {
        request_id: request_id.to_string(),
        hash: hash.to_string(),
        phase,
        pid,
        timestamp: chrono::Utc::now().to_rfc3339(),
        detail,
    }
    .write();
}

pub fn publish_reload_socket_ready() {
    let Some(state) = ReloadState::load() else {
        crate::logging::warn(
            "Server reached socket-ready publish point, but no reload marker was present",
        );
        return;
    };

    let current_pid = std::process::id();
    if state.phase == ReloadPhase::Starting && state.pid == current_pid {
        super::reload_trace::record_value(
            &state.request_id,
            "socket_ready",
            serde_json::json!({
                "hash": &state.hash,
                "detail": &state.detail,
            }),
        );
        write_reload_state(
            &state.request_id,
            &state.hash,
            ReloadPhase::SocketReady,
            state.detail.clone(),
        );
        crate::logging::info(&format!(
            "Published reload socket-ready state for request {}",
            state.request_id
        ));
    } else if state.phase != ReloadPhase::Starting {
        crate::logging::warn(&format!(
            "Server reached socket-ready publish point, but reload marker phase was {:?} (pid={}, current_pid={})",
            state.phase, state.pid, current_pid
        ));
    } else if state.pid != current_pid {
        crate::logging::warn(&format!(
            "Server reached socket-ready publish point, but reload marker pid {} did not match current pid {}; clearing stale marker",
            state.pid, current_pid
        ));
        clear_reload_marker();
    }
}

pub fn reload_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        matches!(err.raw_os_error(), Some(libc::EPERM))
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReloadWaitStatus {
    Ready,
    Waiting { pid: Option<u32> },
    Failed(Option<String>),
    Idle,
}

pub async fn inspect_reload_wait_status(
    socket_path: &std::path::Path,
    max_age: Duration,
    last_known_pid: Option<u32>,
) -> ReloadWaitStatus {
    // Read+classify the marker exactly once for the whole status decision, so
    // the pid-fallback below never re-reads/re-parses the file.
    let hung_live_owner = match observe_reload_marker(max_age) {
        ReloadMarkerObservation::Usable(state) => {
            let status = match state.phase {
                ReloadPhase::SocketReady => ReloadWaitStatus::Ready,
                ReloadPhase::Failed => ReloadWaitStatus::Failed(state.detail),
                ReloadPhase::Starting => {
                    if reload_process_alive(state.pid) {
                        ReloadWaitStatus::Waiting {
                            pid: Some(state.pid),
                        }
                    } else {
                        ReloadWaitStatus::Failed(Some(format!(
                            "reload process {} exited before becoming ready",
                            state.pid
                        )))
                    }
                }
            };
            crate::logging::info(&format!(
                "inspect_reload_wait_status: socket {} marker-driven status={:?} (last_known_pid={:?}, state={})",
                socket_path.display(),
                status,
                last_known_pid,
                reload_state_summary(max_age)
            ));
            return status;
        }
        // `recent_reload_state` hides a hung live owner's marker; remember that
        // verdict so the pid-fallback does not resurrect `Waiting` for it.
        ReloadMarkerObservation::HungLiveOwner => true,
        ReloadMarkerObservation::Stale => false,
    };

    if is_server_ready(socket_path).await || has_live_listener(socket_path).await {
        if last_known_pid.is_some() {
            crate::logging::info(&format!(
                "inspect_reload_wait_status: socket {} is ready/live without active marker (last_known_pid={:?}, state={})",
                socket_path.display(),
                last_known_pid,
                reload_state_summary(max_age)
            ));
        }
        return ReloadWaitStatus::Ready;
    }

    if let Some(pid) = last_known_pid {
        if reload_process_alive(pid) {
            // Do not resurrect `Waiting` for an owner whose `Starting` marker has
            // outlived the hard cap (verdict captured above from the single marker
            // read): pinning clients here would defeat the hard cap and hang them
            // forever. Fall through to Idle so the client reconnects.
            if hung_live_owner {
                crate::logging::warn(&format!(
                    "inspect_reload_wait_status: socket {} last known pid {} is alive but its reload marker exceeded the hard cap; not waiting",
                    socket_path.display(),
                    pid
                ));
            } else {
                crate::logging::info(&format!(
                    "inspect_reload_wait_status: socket {} waiting on last known pid {} without marker",
                    socket_path.display(),
                    pid
                ));
                return ReloadWaitStatus::Waiting { pid: Some(pid) };
            }
        } else {
            crate::logging::warn(&format!(
                "inspect_reload_wait_status: socket {} last known pid {} is no longer alive and no reload marker remains",
                socket_path.display(),
                pid
            ));
        }
    }

    if last_known_pid.is_some() {
        crate::logging::info(&format!(
            "inspect_reload_wait_status: socket {} is idle after previous reload wait state",
            socket_path.display()
        ));
    }
    ReloadWaitStatus::Idle
}

pub async fn await_reload_handoff(
    socket_path: &std::path::Path,
    max_age: Duration,
) -> ReloadWaitStatus {
    let mut last_known_pid = None;
    crate::logging::info(&format!(
        "await_reload_handoff: begin socket={} max_age_ms={} state={}",
        socket_path.display(),
        max_age.as_millis(),
        reload_state_summary(max_age)
    ));

    loop {
        match inspect_reload_wait_status(socket_path, max_age, last_known_pid).await {
            ReloadWaitStatus::Waiting { pid } => {
                last_known_pid = pid;
                crate::logging::info(&format!(
                    "await_reload_handoff: waiting for reload event socket={} pid={:?}",
                    socket_path.display(),
                    pid
                ));
                wait_for_reload_handoff_event(pid, socket_path).await;
            }
            other => {
                crate::logging::info(&format!(
                    "await_reload_handoff: completed socket={} result={:?} state={}",
                    socket_path.display(),
                    other,
                    reload_state_summary(max_age)
                ));
                return other;
            }
        }
    }
}

pub async fn wait_for_reload_handoff_event(
    reloading_pid: Option<u32>,
    socket_path: &std::path::Path,
) {
    crate::logging::info(&format!(
        "wait_for_reload_handoff_event: start socket={} pid={:?}",
        socket_path.display(),
        reloading_pid
    ));
    #[cfg(target_os = "linux")]
    {
        let marker_path = reload_marker_path();
        let socket_path = socket_path.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            wait_for_reload_handoff_event_blocking(&marker_path, &socket_path, reloading_pid)
        })
        .await;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (reloading_pid, socket_path);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    crate::logging::info(&format!(
        "wait_for_reload_handoff_event: wake socket={} pid={:?}",
        socket_path.display(),
        reloading_pid
    ));
}

#[cfg(target_os = "linux")]
fn wait_for_reload_handoff_event_blocking(
    marker_path: &std::path::Path,
    socket_path: &std::path::Path,
    reloading_pid: Option<u32>,
) {
    use std::collections::HashSet;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let mut watch_paths: HashSet<std::path::PathBuf> = HashSet::new();
    if let Some(parent) = marker_path.parent() {
        watch_paths.insert(parent.to_path_buf());
    }
    if let Some(parent) = socket_path.parent() {
        watch_paths.insert(parent.to_path_buf());
    }
    if let Some(pid) = reloading_pid {
        let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
        if proc_path.exists() {
            watch_paths.insert(proc_path);
        }
    }

    if watch_paths.is_empty() {
        crate::logging::warn("wait_for_reload_handoff_event_blocking: no watch paths available");
        return;
    }

    crate::logging::info(&format!(
        "wait_for_reload_handoff_event_blocking: marker={} socket={} pid={:?} watch_paths={:?}",
        marker_path.display(),
        socket_path.display(),
        reloading_pid,
        watch_paths
    ));

    unsafe {
        let fd = libc::inotify_init1(libc::IN_CLOEXEC);
        if fd < 0 {
            crate::logging::warn(&format!(
                "wait_for_reload_handoff_event_blocking: inotify_init1 failed: {} ({})",
                std::io::Error::last_os_error(),
                crate::util::process_fd_diagnostic_snapshot()
            ));
            return;
        }

        let mask = libc::IN_CREATE
            | libc::IN_MOVED_TO
            | libc::IN_ATTRIB
            | libc::IN_MODIFY
            | libc::IN_CLOSE_WRITE
            | libc::IN_DELETE
            | libc::IN_MOVE_SELF
            | libc::IN_DELETE_SELF;

        let mut has_watch = false;
        for path in watch_paths {
            let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
                continue;
            };
            if libc::inotify_add_watch(fd, path.as_ptr(), mask) >= 0 {
                has_watch = true;
            }
        }

        if !has_watch {
            crate::logging::warn(
                "wait_for_reload_handoff_event_blocking: failed to register any inotify watches",
            );
            let _ = libc::close(fd);
            return;
        }

        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };

        loop {
            let ready = libc::poll(&mut poll_fd, 1, RELOAD_HANDOFF_EVENT_POLL_MS);
            if ready > 0 && (poll_fd.revents & libc::POLLIN) != 0 {
                let mut buf = [0u8; 512];
                let _ = libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len());
                crate::logging::info(
                    "wait_for_reload_handoff_event_blocking: observed filesystem/process event",
                );
                break;
            }
            if ready == 0 {
                crate::logging::info(
                    "wait_for_reload_handoff_event_blocking: timed poll elapsed; rechecking reload state",
                );
                break;
            }
            if ready < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                crate::logging::warn(&format!(
                    "wait_for_reload_handoff_event_blocking: poll failed: {}",
                    err
                ));
                break;
            }
        }

        let _ = libc::close(fd);
    }
}

#[derive(Clone, Debug)]
pub struct ReloadSignal {
    pub hash: String,
    pub triggering_session: Option<String>,
    pub prefer_selfdev_binary: bool,
    pub request_id: String,
}

#[derive(Clone, Debug)]
pub struct ReloadAck {
    pub hash: String,
    pub request_id: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReloadPhase {
    Starting,
    SocketReady,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReloadState {
    pub request_id: String,
    pub hash: String,
    pub phase: ReloadPhase,
    pub pid: u32,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ReloadState {
    fn path() -> PathBuf {
        reload_marker_path()
    }

    pub(crate) fn write(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = crate::storage::ensure_dir(parent);
        }
        let _ = crate::storage::write_json(&path, self);
    }

    pub fn load() -> Option<Self> {
        let path = Self::path();
        if !path.exists() {
            return None;
        }
        crate::storage::read_json(&path).ok()
    }
}

pub fn reload_state_summary(max_age: Duration) -> String {
    match recent_reload_state(max_age) {
        Some(state) => format!(
            "request={} hash={} phase={:?} pid={} detail={}",
            state.request_id,
            state.hash,
            state.phase,
            state.pid,
            state.detail.unwrap_or_else(|| "<none>".to_string())
        ),
        None => "no recent reload state".to_string(),
    }
}

type ReloadSignalChannel = (
    tokio::sync::watch::Sender<Option<ReloadSignal>>,
    tokio::sync::watch::Receiver<Option<ReloadSignal>>,
);

type ReloadAckChannel = (
    tokio::sync::watch::Sender<Option<ReloadAck>>,
    tokio::sync::watch::Receiver<Option<ReloadAck>>,
);

/// Global reload signal channel. The selfdev tool and debug commands fire this;
/// the server awaits it instead of polling the filesystem.
static RELOAD_SIGNAL: std::sync::OnceLock<ReloadSignalChannel> = std::sync::OnceLock::new();

static RELOAD_ACK: std::sync::OnceLock<ReloadAckChannel> = std::sync::OnceLock::new();

pub(super) fn reload_signal() -> &'static ReloadSignalChannel {
    RELOAD_SIGNAL.get_or_init(|| tokio::sync::watch::channel(None))
}

#[cfg(test)]
pub(crate) fn subscribe_reload_signal_for_tests()
-> tokio::sync::watch::Receiver<Option<ReloadSignal>> {
    // The signal and ack channels are process globals that are never reset in
    // production. Across tests in one binary a prior test can leave a signal
    // value behind; a test that acknowledges whatever it reads would then ack
    // that stale signal and never see its own. Clear both so a subscriber
    // always starts from a clean channel. (`send_replace` works with no live
    // receivers, unlike `send`.)
    reset_reload_channels_for_tests();
    reload_signal().1.clone()
}

/// Test-only: clear the global reload signal and ack channels so subscribers
/// start from a clean state.
#[cfg(test)]
pub(crate) fn reset_reload_channels_for_tests() {
    reload_signal().0.send_replace(None);
    reload_ack().0.send_replace(None);
}

pub(super) fn reload_ack() -> &'static ReloadAckChannel {
    RELOAD_ACK.get_or_init(|| tokio::sync::watch::channel(None))
}

/// Send a reload signal to the server (called by selfdev tool / debug commands).
pub fn send_reload_signal(
    hash: String,
    triggering_session: Option<String>,
    prefer_selfdev_binary: bool,
) -> String {
    let request_id = crate::id::new_id("reload");
    crate::logging::info(&format!(
        "send_reload_signal: request={} hash={} triggering_session={:?} prefer_selfdev_binary={} current_pid={}",
        request_id,
        hash,
        triggering_session,
        prefer_selfdev_binary,
        std::process::id()
    ));
    let (tx, _) = reload_signal();
    let _ = tx.send(Some(ReloadSignal {
        hash,
        triggering_session,
        prefer_selfdev_binary,
        request_id: request_id.clone(),
    }));
    request_id
}

pub fn acknowledge_reload_signal(signal: &ReloadSignal) {
    crate::logging::info(&format!(
        "acknowledge_reload_signal: request={} hash={} triggering_session={:?} prefer_selfdev_binary={}",
        signal.request_id, signal.hash, signal.triggering_session, signal.prefer_selfdev_binary
    ));
    let (tx, _) = reload_ack();
    let _ = tx.send(Some(ReloadAck {
        hash: signal.hash.clone(),
        request_id: signal.request_id.clone(),
    }));
}

pub async fn wait_for_reload_ack(
    request_id: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<ReloadAck> {
    let mut rx = reload_ack().1.clone();
    let started = std::time::Instant::now();
    crate::logging::info(&format!(
        "wait_for_reload_ack: waiting request={} timeout_ms={}",
        request_id,
        timeout.as_millis()
    ));

    if let Some(ack) = rx.borrow_and_update().clone()
        && ack.request_id == request_id
    {
        crate::logging::info(&format!(
            "wait_for_reload_ack: immediate ack request={} after {}ms",
            request_id,
            started.elapsed().as_millis()
        ));
        return Ok(ack);
    }

    let request_id = request_id.to_string();
    tokio::time::timeout(timeout, async move {
        loop {
            rx.changed()
                .await
                .map_err(|_| anyhow::anyhow!("reload acknowledgement channel closed"))?;
            if let Some(ack) = rx.borrow_and_update().clone()
                && ack.request_id == request_id
            {
                crate::logging::info(&format!(
                    "wait_for_reload_ack: received ack request={} after {}ms",
                    request_id,
                    started.elapsed().as_millis()
                ));
                return Ok(ack);
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "timed out waiting for reload acknowledgement after {}ms (state={})",
            started.elapsed().as_millis(),
            reload_state_summary(Duration::from_secs(60))
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_runtime_dir(path: &std::path::Path) -> Self {
            let key = "JCODE_RUNTIME_DIR";
            let old = std::env::var_os(key);
            crate::env::set_var(key, path);
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(old) = &self.old {
                crate::env::set_var(self.key, old);
            } else {
                crate::env::remove_var(self.key);
            }
        }
    }

    /// A fresh `Starting` marker must make the marker→idle integration helper
    /// true, so the idle monitor is actually suppressed on disk state (not just
    /// in the pure `idle_monitor_should_start` unit test). This is the concrete
    /// link the requirement "server doesn't idle-exit mid-reload" depends on.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn reload_suppresses_idle_shutdown_tracks_the_on_disk_marker() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        // No marker: idle-exit is allowed.
        assert!(
            !reload_suppresses_idle_shutdown(),
            "without a marker the idle monitor must be free to run"
        );

        // A fresh Starting marker (this process owns it) suppresses idle-exit.
        write_reload_state("req-idle", "hash-idle", ReloadPhase::Starting, None);
        assert!(
            reload_suppresses_idle_shutdown(),
            "a fresh Starting marker must suppress idle-exit mid-reload"
        );

        // SocketReady means the handoff COMPLETED (the replacement server is up
        // and accepting connections). It must NOT suppress idle-exit: the marker
        // is never cleared in normal operation, so honoring it would keep an
        // otherwise-idle server alive for the full hard cap after every reload.
        write_reload_state("req-idle", "hash-idle", ReloadPhase::SocketReady, None);
        assert!(
            !reload_suppresses_idle_shutdown(),
            "a completed (SocketReady) reload must not suppress idle-exit"
        );

        // A Failed marker likewise does not suppress idle-exit.
        write_reload_state("req-idle", "hash-idle", ReloadPhase::Failed, None);
        assert!(
            !reload_suppresses_idle_shutdown(),
            "a failed reload must not suppress idle-exit"
        );

        clear_reload_marker();
        assert!(
            !reload_suppresses_idle_shutdown(),
            "clearing the marker must re-enable idle-exit"
        );
    }

    /// The idle suppression is bounded: an over-age marker past the hard cap must
    /// stop suppressing idle-exit, so a hung reload cannot keep the server alive
    /// (and unkillable) forever.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn reload_suppresses_idle_shutdown_expires_past_the_hard_cap() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        ReloadState {
            request_id: "req-idle-hung".to_string(),
            hash: "hash-idle-hung".to_string(),
            phase: ReloadPhase::Starting,
            pid: std::process::id(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            detail: None,
        }
        .write();
        let old =
            std::time::SystemTime::now() - (RELOAD_MARKER_HARD_MAX_AGE + Duration::from_secs(60));
        filetime::set_file_mtime(
            reload_marker_path(),
            filetime::FileTime::from_system_time(old),
        )
        .expect("backdate marker");

        assert!(
            !reload_suppresses_idle_shutdown(),
            "an over-age marker past the hard cap must no longer suppress idle-exit"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn inspect_reload_wait_status_returns_failed_with_marker_detail() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        write_reload_state(
            "req-test",
            "hash-test",
            ReloadPhase::Failed,
            Some("reload failed for test".to_string()),
        );

        let status = inspect_reload_wait_status(
            &temp.path().join("jcode.sock"),
            Duration::from_secs(5),
            None,
        )
        .await;

        assert_eq!(
            status,
            ReloadWaitStatus::Failed(Some("reload failed for test".to_string()))
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn inspect_reload_wait_status_returns_ready_for_socket_ready_marker() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        write_reload_state(
            "req-ready",
            "hash-ready",
            ReloadPhase::SocketReady,
            Some("ready for handoff".to_string()),
        );

        let status = inspect_reload_wait_status(
            &temp.path().join("jcode.sock"),
            Duration::from_secs(5),
            None,
        )
        .await;

        assert_eq!(status, ReloadWaitStatus::Ready);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn wait_for_reload_ack_returns_matching_ack() {
        let _lock = crate::storage::lock_test_env();
        let request_id = crate::id::new_id("reload-test");
        let ack = ReloadAck {
            hash: "hash-test".to_string(),
            request_id: request_id.clone(),
        };
        let (tx, _) = reload_ack();
        let _ = tx.send(Some(ack.clone()));

        let received = wait_for_reload_ack(&request_id, Duration::from_millis(50))
            .await
            .expect("ack should be received");

        assert_eq!(received.request_id, ack.request_id);
        assert_eq!(received.hash, ack.hash);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn wait_for_reload_ack_handles_repeated_unique_requests() {
        let _lock = crate::storage::lock_test_env();
        let (tx, _) = reload_ack();

        for _ in 0..5 {
            let request_id = crate::id::new_id("reload-repeat");
            let ack = ReloadAck {
                hash: format!("hash-{}", request_id),
                request_id: request_id.clone(),
            };
            let _ = tx.send(Some(ack.clone()));

            let received = wait_for_reload_ack(&request_id, Duration::from_millis(50))
                .await
                .expect("ack should be received for repeated request");

            assert_eq!(received.request_id, ack.request_id);
            assert_eq!(received.hash, ack.hash);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn inspect_reload_wait_status_handles_repeated_ready_markers() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());
        let socket_path = temp.path().join("jcode.sock");

        for idx in 0..5 {
            write_reload_state(
                &format!("req-{idx}"),
                &format!("hash-{idx}"),
                ReloadPhase::SocketReady,
                Some(format!("ready-{idx}")),
            );

            let status =
                inspect_reload_wait_status(&socket_path, Duration::from_secs(5), None).await;
            assert_eq!(status, ReloadWaitStatus::Ready);
        }
    }

    /// Spawn a short-lived child and reap it so we hold a pid that is reliably
    /// dead at the moment of selection. Retries guard against the (extremely
    /// rare) case where the kernel immediately recycles the pid for another
    /// test thread's process.
    #[cfg(unix)]
    fn spawn_and_reap_dead_pid() -> u32 {
        use std::process::Command;
        for _ in 0..16 {
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg("exit 0")
                .spawn()
                .expect("spawn short-lived child");
            let pid = child.id();
            let _ = child.wait();
            if !reload_process_alive(pid) {
                return pid;
            }
        }
        panic!("could not obtain a reliably-dead pid");
    }

    // Note: the marker-driven Ready/Idle/Failed/Waiting verdicts and the
    // foreign-pid socket-ready clearing are already covered in
    // `server::socket_tests`. The tests below intentionally cover only the
    // gaps not exercised there: stale-marker cleanup, Failed-marker
    // preservation, corrupt-marker tolerance, and the dead last-known-pid
    // fallback.

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn inspect_reload_wait_status_idle_when_last_known_pid_is_dead_without_marker() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());
        clear_reload_marker();

        let dead_pid = spawn_and_reap_dead_pid();
        let status = inspect_reload_wait_status(
            &temp.path().join("missing.sock"),
            Duration::from_secs(5),
            Some(dead_pid),
        )
        .await;
        assert_eq!(status, ReloadWaitStatus::Idle);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn clear_reload_marker_if_stale_for_pid_keeps_own_starting_marker() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        let current = std::process::id();
        ReloadState {
            request_id: "req-keep".to_string(),
            hash: "hash-keep".to_string(),
            phase: ReloadPhase::Starting,
            pid: current,
            timestamp: chrono::Utc::now().to_rfc3339(),
            detail: None,
        }
        .write();

        clear_reload_marker_if_stale_for_pid(current);
        assert!(
            reload_marker_exists(),
            "an in-flight Starting marker owned by this pid must survive cleanup"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn clear_reload_marker_if_stale_for_pid_clears_foreign_or_completed_markers() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        let current = std::process::id();

        // Foreign pid still in Starting -> stale, must be cleared.
        ReloadState {
            request_id: "req-foreign".to_string(),
            hash: "hash-foreign".to_string(),
            phase: ReloadPhase::Starting,
            pid: current.wrapping_add(1),
            timestamp: chrono::Utc::now().to_rfc3339(),
            detail: None,
        }
        .write();
        clear_reload_marker_if_stale_for_pid(current);
        assert!(
            !reload_marker_exists(),
            "a foreign Starting marker must be cleared"
        );

        // Own pid but already completed (SocketReady) -> not an in-flight boot.
        ReloadState {
            request_id: "req-ready".to_string(),
            hash: "hash-ready".to_string(),
            phase: ReloadPhase::SocketReady,
            pid: current,
            timestamp: chrono::Utc::now().to_rfc3339(),
            detail: None,
        }
        .write();
        clear_reload_marker_if_stale_for_pid(current);
        assert!(
            !reload_marker_exists(),
            "a completed marker must be cleared on stale check"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn publish_reload_socket_ready_leaves_failed_marker_untouched() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        write_reload_state(
            "req-failed",
            "hash-failed",
            ReloadPhase::Failed,
            Some("boom".to_string()),
        );
        publish_reload_socket_ready();

        let state = ReloadState::load().expect("marker should still exist");
        assert_eq!(
            state.phase,
            ReloadPhase::Failed,
            "publish must not overwrite a Failed marker"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn recent_reload_state_ignores_corrupt_marker() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        let path = reload_marker_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create runtime dir");
        }
        std::fs::write(&path, b"{ this is not valid json").expect("write corrupt marker");

        assert!(
            ReloadState::load().is_none(),
            "corrupt marker should not deserialize"
        );
        assert!(
            recent_reload_state(Duration::from_secs(5)).is_none(),
            "corrupt marker should be treated as no recent state"
        );
        assert!(
            !reload_marker_active(Duration::from_secs(5)),
            "corrupt marker must not be reported as active"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn reload_marker_active_treats_failed_phase_as_inactive() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        write_reload_state("req", "hash", ReloadPhase::Failed, Some("x".to_string()));
        assert!(
            !reload_marker_active(Duration::from_secs(5)),
            "a Failed reload must not look active"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reload_process_alive_handles_zero_and_dead_pids() {
        assert!(!reload_process_alive(0), "pid 0 is never a live reload pid");
        let dead = spawn_and_reap_dead_pid();
        assert!(
            !reload_process_alive(dead),
            "a reaped child pid must be reported dead"
        );
        assert!(
            reload_process_alive(std::process::id()),
            "the current process must be reported alive"
        );
    }

    /// A reload whose shutdown outlives the nominal marker max-age must still be
    /// treated as in-progress while its process is alive. Otherwise clients
    /// abandon the handoff mid-reload and the replacement server cannot publish a
    /// socket-ready state (it finds "no reload marker"), leaving every session to
    /// reconnect from scratch.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn recent_reload_state_keeps_a_live_starting_marker_past_max_age() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        // A `Starting` marker owned by this (live) process, backdated so it is
        // well past the max-age window.
        ReloadState {
            request_id: "req-aging".to_string(),
            hash: "hash-aging".to_string(),
            phase: ReloadPhase::Starting,
            pid: std::process::id(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            detail: None,
        }
        .write();
        let old = std::time::SystemTime::now() - Duration::from_secs(120);
        filetime::set_file_mtime(
            reload_marker_path(),
            filetime::FileTime::from_system_time(old),
        )
        .expect("backdate marker");

        assert!(
            recent_reload_state(Duration::from_secs(2)).is_some(),
            "a live Starting marker must survive max-age expiry"
        );
        assert!(
            reload_marker_exists(),
            "the live marker file must not be deleted"
        );

        // The client-visible consequence: a waiting client must still see the
        // reload as in-progress (Waiting), not Idle/Failed. This is the exact
        // field scenario where a ~39s shutdown outlived the 30s client TTL and
        // clients abandoned the handoff.
        assert_eq!(
            inspect_reload_wait_status(
                &temp.path().join("missing.sock"),
                Duration::from_secs(2),
                None,
            )
            .await,
            ReloadWaitStatus::Waiting {
                pid: Some(std::process::id())
            },
            "an over-age marker with a live owner must keep clients waiting"
        );
    }

    /// The marker staleness decision, checked purely (no file I/O, no platform
    /// liveness) so every branch — including the off-unix "liveness is
    /// unobservable" case — executes on every platform, including the Linux and
    /// macOS CI jobs that actually run this module's tests.
    ///
    /// This is the regression guard for the off-unix fix: with
    /// `liveness_observable = false`, an over-age `Starting` marker with a live pid
    /// must be `Stale` (expire at `max_age`), NOT `Extended` (pinned to the hard
    /// cap). `reload_process_alive` reports every nonzero pid as alive off-unix, so
    /// before the gate a failed reload would keep clients in `Waiting` (and
    /// `server_reload_starting()` true) for up to 10 minutes.
    #[test]
    fn classify_marker_expires_where_liveness_is_unobservable() {
        let max_age = Duration::from_secs(30);
        let over_age = Duration::from_secs(120);
        let past_hard_cap = RELOAD_MARKER_HARD_MAX_AGE + Duration::from_secs(60);

        // Liveness unobservable (the off-unix path): a live-looking over-age marker
        // must expire at max_age rather than being extended to the hard cap.
        assert_eq!(
            classify_marker(ReloadPhase::Starting, over_age, max_age, false, true),
            MarkerVerdict::Stale,
            "without observable liveness an over-age Starting marker must expire"
        );
        assert_eq!(
            classify_marker(ReloadPhase::Starting, past_hard_cap, max_age, false, true),
            MarkerVerdict::Stale,
            "unobservable liveness must never reach the hung/hard-cap branches"
        );

        // A marker within max_age is fresh regardless of liveness observability.
        assert_eq!(
            classify_marker(
                ReloadPhase::Starting,
                Duration::from_secs(5),
                max_age,
                false,
                true
            ),
            MarkerVerdict::Fresh
        );

        // Liveness observable (the unix path): the live-owner extension applies.
        assert_eq!(
            classify_marker(ReloadPhase::Starting, over_age, max_age, true, true),
            MarkerVerdict::Extended,
            "with observable liveness a live owner extends past max_age"
        );
        assert_eq!(
            classify_marker(ReloadPhase::Starting, past_hard_cap, max_age, true, true),
            MarkerVerdict::HungLiveOwner,
            "a live owner past the hard cap is hung"
        );
        // A dead owner is stale regardless of the cap window.
        assert_eq!(
            classify_marker(ReloadPhase::Starting, over_age, max_age, true, false),
            MarkerVerdict::Stale,
            "a dead owner's over-age marker is stale"
        );
        // Only the Starting phase gets the live-owner extension.
        assert_eq!(
            classify_marker(ReloadPhase::SocketReady, over_age, max_age, true, true),
            MarkerVerdict::Stale,
            "phase must be Starting to extend"
        );
    }

    /// Once the owning process is gone, an over-age `Starting` marker is stale
    /// and must be reclaimed as before.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn recent_reload_state_expires_a_starting_marker_when_owner_is_dead() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        let dead_pid = spawn_and_reap_dead_pid();
        ReloadState {
            request_id: "req-dead".to_string(),
            hash: "hash-dead".to_string(),
            phase: ReloadPhase::Starting,
            pid: dead_pid,
            timestamp: chrono::Utc::now().to_rfc3339(),
            detail: None,
        }
        .write();
        let old = std::time::SystemTime::now() - Duration::from_secs(120);
        filetime::set_file_mtime(
            reload_marker_path(),
            filetime::FileTime::from_system_time(old),
        )
        .expect("backdate marker");

        assert!(
            recent_reload_state(Duration::from_secs(2)).is_none(),
            "a dead owner's over-age marker must be cleared"
        );
    }

    /// A live owner only extends an in-progress marker up to
    /// `RELOAD_MARKER_HARD_MAX_AGE`; past that the owner is presumed hung so
    /// waiting clients are not pinned forever on a stuck handoff.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn recent_reload_state_expires_a_live_starting_marker_past_the_hard_cap() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        ReloadState {
            request_id: "req-hung".to_string(),
            hash: "hash-hung".to_string(),
            phase: ReloadPhase::Starting,
            pid: std::process::id(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            detail: None,
        }
        .write();
        let old =
            std::time::SystemTime::now() - (RELOAD_MARKER_HARD_MAX_AGE + Duration::from_secs(60));
        filetime::set_file_mtime(
            reload_marker_path(),
            filetime::FileTime::from_system_time(old),
        )
        .expect("backdate marker");

        assert!(
            recent_reload_state(Duration::from_secs(2)).is_none(),
            "a live owner beyond the hard cap must be treated as hung, not honored forever"
        );
        // The marker is kept (not deleted) so a genuinely slow owner that later
        // publishes socket-ready can still complete the handoff.
        assert!(
            reload_marker_exists(),
            "the over-cap marker must be retained for a late socket-ready publish"
        );
    }

    /// The pid-fallback path must not resurrect `Waiting` for a hung owner.
    /// Without this, a client that passes the live owner pid would keep waiting
    /// forever even though `recent_reload_state` hid the over-cap marker.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn inspect_reload_wait_status_does_not_wait_on_a_hung_owner() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        ReloadState {
            request_id: "req-hung".to_string(),
            hash: "hash-hung".to_string(),
            phase: ReloadPhase::Starting,
            pid: std::process::id(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            detail: None,
        }
        .write();
        let old =
            std::time::SystemTime::now() - (RELOAD_MARKER_HARD_MAX_AGE + Duration::from_secs(60));
        filetime::set_file_mtime(
            reload_marker_path(),
            filetime::FileTime::from_system_time(old),
        )
        .expect("backdate marker");

        assert!(
            matches!(
                observe_reload_marker(Duration::from_secs(2)),
                ReloadMarkerObservation::HungLiveOwner
            ),
            "fixture must classify as a hung live owner"
        );
        let socket_path = temp.path().join("missing.sock");
        let status = inspect_reload_wait_status(
            &socket_path,
            Duration::from_secs(2),
            Some(std::process::id()),
        )
        .await;
        assert_eq!(
            status,
            ReloadWaitStatus::Idle,
            "a hung owner must not keep clients waiting via the pid fallback"
        );
    }
}
