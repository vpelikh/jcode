//! Notification dispatcher for ambient mode.
//!
//! Sends notifications via:
//! - ntfy.sh (push notifications to phone)
//! - Desktop notifications (notify-send)
//! - Email (SMTP via lettre)
//!
//! All sends are fire-and-forget: errors are logged, never block.

use crate::config::{SafetyConfig, config};
use crate::logging;
use crate::safety::AmbientTranscript;

use jcode_notify_email::{
    ReplyAction, SendEmailRequest, build_permission_email_html, poll_imap_once, send_email,
};
pub use jcode_notify_email::{extract_permission_id, parse_permission_reply};

/// Stable schema version for files handed to the bundled macOS notification
/// broker. The broker ignores payloads with a newer schema instead of guessing
/// at their meaning.
pub const MACOS_NOTIFICATION_SCHEMA_VERSION: u32 = 1;

/// The terminal route attached to a macOS turn notification.
///
/// `tty` is the strongest identifier available across Terminal.app and iTerm2:
/// both expose it in their AppleScript dictionaries, so a notification click
/// can select the exact originating tab/session. Ghostty currently exposes no
/// supported per-surface activation API, so its route intentionally degrades to
/// activating the application.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacosTerminalKind {
    AppleTerminal,
    Iterm2,
    Ghostty,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MacosNotificationOrigin {
    pub terminal: MacosTerminalKind,
    pub bundle_id: Option<String>,
    pub tty: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MacosNotificationEnvelope {
    pub schema_version: u32,
    pub notification_id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub body: String,
    pub sound: Option<String>,
    pub origin: MacosNotificationOrigin,
}

/// Notification priority levels (maps to ntfy priority header).
#[derive(Debug, Clone, Copy)]
pub enum Priority {
    /// Routine cycle summaries
    Default,
    /// Permission requests, errors
    High,
    /// Critical safety issues
    Urgent,
}

impl Priority {
    fn ntfy_value(self) -> &'static str {
        match self {
            Priority::Default => "3",
            Priority::High => "4",
            Priority::Urgent => "5",
        }
    }

    fn ntfy_tags(self) -> &'static str {
        match self {
            Priority::Default => "robot",
            Priority::High => "warning",
            Priority::Urgent => "rotating_light",
        }
    }
}

/// Dispatcher that sends notifications through all configured channels.
#[derive(Clone)]
pub struct NotificationDispatcher {
    client: reqwest::Client,
    config: SafetyConfig,
    channels: crate::channel::ChannelRegistry,
}

impl Default for NotificationDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationDispatcher {
    pub fn new() -> Self {
        let cfg = config().safety.clone();
        Self {
            client: crate::provider::shared_http_client(),
            channels: crate::channel::ChannelRegistry::from_config(&cfg),
            config: cfg,
        }
    }

    #[cfg(test)]
    pub fn from_config(config: SafetyConfig) -> Self {
        Self {
            client: crate::provider::shared_http_client(),
            channels: crate::channel::ChannelRegistry::from_config(&config),
            config,
        }
    }

    /// Send a cycle summary notification (after ambient cycle completes).
    pub fn dispatch_cycle_summary(&self, transcript: &AmbientTranscript) {
        let title = format!(
            "Ambient cycle: {} memories, {} compactions",
            transcript.memories_modified, transcript.compactions
        );
        let safe_body = format_cycle_body_safe(transcript);
        let detailed_body = format_cycle_body_detailed(transcript);

        let priority = if transcript.pending_permissions > 0 {
            Priority::High
        } else {
            Priority::Default
        };

        self.send_all(
            &title,
            &safe_body,
            &detailed_body,
            priority,
            Some(&transcript.session_id),
        );
    }

    /// Send a permission request notification (high priority).
    pub fn dispatch_permission_request(&self, action: &str, description: &str, request_id: &str) {
        let title = format!("jcode: permission needed ({})", action);
        let safe_body = "An ambient action needs your approval. Open jcode to review.".to_string();
        let detailed_body = format!(
            "Action: {}\n{}\n\nRequest ID: {}\nReview in jcode to approve or deny.",
            action, description, request_id
        );

        // Build rich HTML email with approve/deny buttons
        let reply_to = self
            .config
            .email_from
            .as_deref()
            .unwrap_or("jcode@localhost");
        let email_html = build_permission_email_html(action, description, request_id, reply_to);

        self.send_all_with_email_override(
            &title,
            &safe_body,
            &detailed_body,
            Priority::High,
            Some(request_id),
            Some(&email_html),
        );
    }

    /// Send a generic notification to **all** configured backends (ntfy, desktop,
    /// email, and message channels such as Telegram/Discord). Fire-and-forget.
    ///
    /// `body` is used for every backend: it is the safe body for ntfy and the
    /// detailed body for desktop/email/channels. Pass a body that is safe to send
    /// over a potentially public channel (ntfy) — i.e. no ambient secrets.
    ///
    /// This is the general entry point for per-session notifications so they fan
    /// out the same way ambient notifications do. When the body must differ
    /// between ntfy and the private backends (e.g. a full assistant reply that
    /// should not reach a public ntfy topic), use
    /// [`dispatch_rich`](Self::dispatch_rich). A backend that is disabled in
    /// config simply does not fire.
    ///
    /// `session_id` is passed through to the fan-out for reply tracking / tracing.
    pub fn dispatch(
        &self,
        title: &str,
        body: &str,
        priority: Priority,
        session_id: Option<&str>,
    ) {
        self.send_all(title, body, body, priority, session_id);
    }

    /// Like [`dispatch`](Self::dispatch), but keeps the ntfy body separate from
    /// the detailed body.
    ///
    /// Use this when the detailed body carries private content (e.g. a full
    /// assistant reply) that must not be pushed to a potentially public ntfy
    /// topic. `safe_body` goes to ntfy; `detailed_body` goes to desktop, email,
    /// and the message channels.
    pub fn dispatch_rich(
        &self,
        title: &str,
        safe_body: &str,
        detailed_body: &str,
        priority: Priority,
        session_id: Option<&str>,
    ) {
        self.send_all(title, safe_body, detailed_body, priority, session_id);
    }

    /// Send through all configured channels (fire-and-forget).
    ///
    /// `safe_body` is sanitized (no secrets) — used for ntfy (potentially public).
    /// `detailed_body` includes full info — used for email and desktop (private channels).
    /// `cycle_id` is embedded as Message-ID in emails for reply tracking.
    fn send_all(
        &self,
        title: &str,
        safe_body: &str,
        detailed_body: &str,
        priority: Priority,
        cycle_id: Option<&str>,
    ) {
        self.send_all_with_email_override(
            title,
            safe_body,
            detailed_body,
            priority,
            cycle_id,
            None,
        );
    }

    /// Like `send_all`, but with an optional pre-built HTML body for the email channel.
    /// When `email_html_override` is Some, it's used directly as the email body instead
    /// of converting `detailed_body` through `markdown_to_html_email`.
    fn send_all_with_email_override(
        &self,
        title: &str,
        safe_body: &str,
        detailed_body: &str,
        priority: Priority,
        cycle_id: Option<&str>,
        email_html_override: Option<&str>,
    ) {
        // Guard: only dispatch if inside a tokio runtime
        if tokio::runtime::Handle::try_current().is_err() {
            logging::info("Notification skipped: no tokio runtime");
            return;
        }

        // ntfy.sh — uses SAFE body (may be publicly readable)
        if let Some(ref topic) = self.config.ntfy_topic {
            let client = self.client.clone();
            let url = format!("{}/{}", self.config.ntfy_server, topic);
            let title = title.to_string();
            let body = safe_body.to_string();
            tokio::spawn(async move {
                if let Err(e) = send_ntfy(&client, &url, &title, &body, priority).await {
                    logging::error(&format!("ntfy notification failed: {}", e));
                }
            });
        }

        // Desktop notification — uses DETAILED body (local machine, private)
        if self.config.desktop_notifications {
            let title = title.to_string();
            let body = detailed_body.to_string();
            let urgency = match priority {
                Priority::Default => "normal",
                Priority::High | Priority::Urgent => "critical",
            };
            tokio::spawn(async move {
                send_desktop(&title, &body, urgency);
            });
        }

        // Email — uses DETAILED body (sent to your own address, private)
        // If email_html_override is provided, send it directly as HTML.
        if self.config.email_enabled
            && let (Some(to), Some(host), Some(from)) = (
                &self.config.email_to,
                &self.config.email_smtp_host,
                &self.config.email_from,
            )
        {
            let to = to.clone();
            let host = host.clone();
            let from = from.clone();
            let port = self.config.email_smtp_port;
            let password = self.config.email_password.clone();
            let title = title.to_string();
            let body = detailed_body.to_string();
            let cycle_id = cycle_id.map(|s| s.to_string());
            let html_override = email_html_override.map(|s| s.to_string());
            tokio::spawn(async move {
                if let Err(e) = send_email(SendEmailRequest {
                    smtp_host: &host,
                    smtp_port: port,
                    from: &from,
                    to: &to,
                    password: password.as_deref(),
                    subject: &title,
                    body: &body,
                    cycle_id: cycle_id.as_deref(),
                    html_override: html_override.as_deref(),
                })
                .await
                {
                    logging::error(&format!("Email notification failed: {}", e));
                } else {
                    logging::info(&format!("Email notification sent to {}: {}", to, title));
                }
            });
        }

        // Message channels (Telegram, Discord, etc.) — uses DETAILED body
        let channel_text = format!("*{}*\n\n{}", title, detailed_body);
        self.channels.send_all(&channel_text);
    }
}

// ---------------------------------------------------------------------------
// ntfy.sh
// ---------------------------------------------------------------------------

async fn send_ntfy(
    client: &reqwest::Client,
    url: &str,
    title: &str,
    body: &str,
    priority: Priority,
) -> anyhow::Result<()> {
    let resp = client
        .post(url)
        .header("Title", title)
        .header("Priority", priority.ntfy_value())
        .header("Tags", priority.ntfy_tags())
        .body(body.to_string())
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("ntfy returned {}: {}", status, text);
    }

    logging::info(&format!("ntfy notification sent: {}", title));
    Ok(())
}

// ---------------------------------------------------------------------------
// Desktop (cross-platform, fire-and-forget)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
const MACOS_NOTIFICATION_BROKER_APP_NAME: &str = "Jcode Notifications.app";
#[cfg(target_os = "macos")]
const MACOS_NOTIFICATION_BROKER_EXECUTABLE: &str = "jcode-notification-broker";

impl MacosNotificationOrigin {
    /// Capture the terminal route for the local client which owns this process.
    pub fn detect() -> Self {
        let tty = controlling_tty();
        Self::from_values(
            &std::env::var("TERM_PROGRAM").unwrap_or_default(),
            &std::env::var("TERM").unwrap_or_default(),
            tty.as_deref(),
            std::env::var("TERM_SESSION_ID").ok().as_deref(),
            std::env::var("ITERM_SESSION_ID").ok().as_deref(),
            std::env::var("GHOSTTY_RESOURCES_DIR").is_ok()
                || std::env::var("GHOSTTY_BIN_DIR").is_ok(),
        )
    }

    fn from_values(
        term_program: &str,
        term: &str,
        tty: Option<&str>,
        term_session_id: Option<&str>,
        iterm_session_id: Option<&str>,
        has_ghostty_env: bool,
    ) -> Self {
        let term_program_lower = term_program.to_ascii_lowercase();
        let term_lower = term.to_ascii_lowercase();
        let (terminal, bundle_id, session_id) =
            if term_program_lower == "iterm.app" || iterm_session_id.is_some() {
                (
                    MacosTerminalKind::Iterm2,
                    Some("com.googlecode.iterm2".to_string()),
                    iterm_session_id,
                )
            } else if term_program_lower == "apple_terminal" {
                (
                    MacosTerminalKind::AppleTerminal,
                    Some("com.apple.Terminal".to_string()),
                    term_session_id,
                )
            } else if has_ghostty_env
                || term_program_lower == "ghostty"
                || term_lower.contains("ghostty")
            {
                (
                    MacosTerminalKind::Ghostty,
                    Some("com.mitchellh.ghostty".to_string()),
                    term_session_id,
                )
            } else {
                (MacosTerminalKind::Unknown, None, term_session_id)
            };

        Self {
            terminal,
            bundle_id,
            tty: tty.filter(|value| valid_tty(value)).map(str::to_string),
            session_id: session_id
                .filter(|value| valid_route_identifier(value))
                .map(str::to_string),
        }
    }
}

fn valid_tty(value: &str) -> bool {
    value.starts_with("/dev/tty")
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-'))
}

fn valid_route_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

#[cfg(unix)]
fn controlling_tty() -> Option<String> {
    use std::os::fd::AsRawFd;

    let fd = std::io::stdin().as_raw_fd();
    let mut buffer = vec![0 as libc::c_char; 1024];
    // SAFETY: `buffer` is valid and writable for its full length and `fd` is a
    // live descriptor. `ttyname_r` writes a NUL-terminated string on success.
    let result = unsafe { libc::ttyname_r(fd, buffer.as_mut_ptr(), buffer.len()) };
    if result != 0 {
        return None;
    }
    // SAFETY: successful `ttyname_r` guarantees a NUL terminator in `buffer`.
    let value = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    value.to_str().ok().map(str::to_string)
}

#[cfg(not(unix))]
fn controlling_tty() -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn macos_notification_broker_app_path() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("JCODE_MACOS_NOTIFICATION_BROKER_APP") {
        return Some(path.into());
    }
    dirs::home_dir().map(|home| {
        home.join("Applications")
            .join(MACOS_NOTIFICATION_BROKER_APP_NAME)
    })
}

/// The durable inbox consumed by the bundled macOS broker.
pub fn macos_notification_inbox_dir() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("JCODE_MACOS_NOTIFICATION_INBOX") {
        return Some(path.into());
    }
    dirs::home_dir().map(|home| {
        home.join(".jcode")
            .join("notifications")
            .join("macos")
            .join("inbox")
    })
}

/// Build a broker envelope with banner-appropriate text: Notification Center
/// renders plain text, so a raw Markdown body would show literal
/// `#`/`**`/backticks/fences. The conversion happens here (this is the display
/// sink); callers keep the original full body for other backends.
///
/// The body is bounded to a Notification-Center-safe ceiling: a very large
/// `UNMutableNotificationContent` body is a macOS 27 `NotificationCenter`
/// layout cost and can be rejected outright, which used to strand the payload
/// in a resubmission loop. The ceiling is far above any realistic reply, so the
/// full text still shows on screen for normal turns; only the pathological
/// case is capped. Durable backends (email/chat) always get the full text.
///
/// `notification_id` is supplied by the caller so this stays free of the
/// macOS-only id generator and remains unit-testable on any platform.
#[cfg(any(target_os = "macos", test))]
fn macos_turn_envelope(
    notification_id: String,
    title: &str,
    subtitle: Option<&str>,
    body: &str,
    sound: Option<&str>,
    origin: MacosNotificationOrigin,
) -> MacosNotificationEnvelope {
    MacosNotificationEnvelope {
        schema_version: MACOS_NOTIFICATION_SCHEMA_VERSION,
        notification_id,
        // Bound title, subtitle, and body alike: each is rendered by
        // Notification Center, so an oversized one has the same cost as an
        // oversized body. The title is only capped, not Markdown-converted, to
        // preserve the exact title text (it is not Markdown-formatted).
        title: bound_macos_notification_text(title),
        subtitle: subtitle
            .filter(|value| !value.trim().is_empty())
            .map(|value| bound_macos_notification_text(&markdown_to_plain_text(value))),
        body: bound_macos_notification_text(&markdown_to_plain_text(body)),
        sound: sound
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string),
        origin,
    }
}

/// Queue a turn notification for the bundled LSUIElement broker and wake it.
/// Returns false when the helper is unavailable so the caller can use its
/// terminal-native or `osascript` fallback.
pub fn send_macos_turn_notification(
    title: &str,
    subtitle: Option<&str>,
    body: &str,
    sound: Option<&str>,
) -> bool {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, subtitle, body, sound);
        false
    }

    #[cfg(target_os = "macos")]
    {
        let Some(app_path) = macos_notification_broker_app_path() else {
            return false;
        };
        let executable = app_path
            .join("Contents")
            .join("MacOS")
            .join(MACOS_NOTIFICATION_BROKER_EXECUTABLE);
        if !app_path.is_dir() || !executable.is_file() {
            return false;
        }

        let envelope = macos_turn_envelope(
            next_macos_notification_id(),
            title,
            subtitle,
            body,
            sound,
            MacosNotificationOrigin::detect(),
        );
        let queued_path = match enqueue_macos_notification(&envelope) {
            Ok(path) => path,
            Err(error) => {
                logging::warn(&format!("failed to queue macOS notification: {error}"));
                return false;
            }
        };

        // Launch the broker executable directly rather than via `/usr/bin/open
        // -gj`. LaunchServices resolves the bundle's `jcode-notification-broker`
        // symlink back to the real `.../jcode` binary name, which breaks the
        // argv[0]-based multicall dispatch: `is_invocation()` sees the canonical
        // name and the process exits as a normal CLI without ever posting to
        // Notification Center. Spawning the symlink by name preserves it, so the
        // broker enters its run loop and drains the inbox.
        if macos_notification_broker_is_running() {
            // A broker is already resident and will pick up the queued payload.
            return true;
        }
        match spawn_macos_notification_broker(&executable) {
            Ok(()) => true,
            Err(error) => {
                // Could not launch the broker; remove this payload so the caller
                // can fall back to a generic desktop notification without
                // delivering a duplicate later.
                let _ = std::fs::remove_file(queued_path);
                logging::warn(&format!(
                    "failed to launch macOS notification broker: {error}"
                ));
                false
            }
        }
    }
}

/// Whether a `jcode-notification-broker` process is already alive. Used to avoid
/// stacking a second broker (each owns an NSApplication run loop, so duplicates
/// would race to drain the inbox).
#[cfg(target_os = "macos")]
fn macos_notification_broker_is_running() -> bool {
    use std::process::Command;

    let ps = match Command::new("/bin/ps")
        .args(["-axo", "comm="])
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };
    let haystack = String::from_utf8_lossy(&ps.stdout);
    haystack.lines().any(|line| {
        let name = line.rsplit('/').next().unwrap_or(line.trim());
        name.trim() == MACOS_NOTIFICATION_BROKER_EXECUTABLE
    })
}

/// Spawn the broker executable detached. Returns true when the child was
/// launched (it should stay resident draining the inbox at 0.25s).
#[cfg(target_os = "macos")]
fn spawn_macos_notification_broker(executable: &std::path::Path) -> std::io::Result<()> {
    use std::process::Command;
    use std::os::unix::process::CommandExt as _;

    let mut command = Command::new(executable);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach into a new process group/session so it survives the caller exiting
    // and keeps its own run-loop (mirrors the persistent intent of the
    // LSUIElement bundle without depending on LaunchServices).
    #[cfg(unix)]
    command.process_group(0);
    command.spawn().map(|_| ())
}

#[cfg(target_os = "macos")]
fn next_macos_notification_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "jcode-turn-{timestamp}-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(target_os = "macos")]
fn enqueue_macos_notification(
    envelope: &MacosNotificationEnvelope,
) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write as _;

    let inbox = macos_notification_inbox_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine notification inbox"))?;
    std::fs::create_dir_all(&inbox)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&inbox, std::fs::Permissions::from_mode(0o700))?;
    }

    let final_path = inbox.join(format!("{}.json", envelope.notification_id));
    let temporary_path = inbox.join(format!(".{}.tmp", envelope.notification_id));
    let bytes = serde_json::to_vec(envelope)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary_path, &final_path)?;
    Ok(final_path)
}

fn reap_notification_child(mut child: std::process::Child) {
    let _ = std::thread::Builder::new()
        .name("jcode-notification-child".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
}

/// Build the process invocation used when a broker notification is clicked.
/// Kept pure so routing and escaping are fully testable on non-macOS builders.
pub fn macos_notification_activation_command(
    origin: &MacosNotificationOrigin,
) -> Option<(String, Vec<String>)> {
    fn applescript_string(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }

    match origin.terminal {
        MacosTerminalKind::AppleTerminal => {
            let tty = origin.tty.as_deref().filter(|value| valid_tty(value));
            let script = if let Some(tty) = tty {
                format!(
                    "tell application \"Terminal\"\nrepeat with w in windows\nrepeat with t in tabs of w\nif tty of t is \"{}\" then\nset selected tab of w to t\nset frontmost of w to true\nactivate\nreturn\nend if\nend repeat\nend repeat\nactivate\nend tell",
                    applescript_string(tty)
                )
            } else {
                "tell application \"Terminal\" to activate".to_string()
            };
            Some((
                "/usr/bin/osascript".to_string(),
                vec!["-e".to_string(), script],
            ))
        }
        MacosTerminalKind::Iterm2 => {
            let tty = origin.tty.as_deref().filter(|value| valid_tty(value));
            let script = if let Some(tty) = tty {
                format!(
                    "tell application \"iTerm2\"\nrepeat with w in windows\nrepeat with t in tabs of w\nrepeat with s in sessions of t\nif tty of s is \"{}\" then\nselect s\nselect t\nactivate\nreturn\nend if\nend repeat\nend repeat\nend repeat\nactivate\nend tell",
                    applescript_string(tty)
                )
            } else {
                "tell application \"iTerm2\" to activate".to_string()
            };
            Some((
                "/usr/bin/osascript".to_string(),
                vec!["-e".to_string(), script],
            ))
        }
        MacosTerminalKind::Ghostty => Some((
            "/usr/bin/open".to_string(),
            vec![
                "-b".to_string(),
                origin
                    .bundle_id
                    .as_deref()
                    .filter(|value| *value == "com.mitchellh.ghostty")
                    .unwrap_or("com.mitchellh.ghostty")
                    .to_string(),
            ],
        )),
        MacosTerminalKind::Unknown => origin.bundle_id.as_deref().and_then(|bundle_id| {
            let safe = bundle_id.len() <= 255
                && bundle_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
            safe.then(|| {
                (
                    "/usr/bin/open".to_string(),
                    vec!["-b".to_string(), bundle_id.to_string()],
                )
            })
        }),
    }
}

/// Activate the recorded terminal route without blocking the notification
/// delegate's main run loop.
pub fn activate_macos_notification_origin(origin: &MacosNotificationOrigin) {
    let Some((program, args)) = macos_notification_activation_command(origin) else {
        return;
    };
    if let Ok(child) = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        reap_notification_child(child);
    }
}

/// Send a local desktop notification without blocking.
///
/// Uses Notification Center via `osascript` on macOS and `notify-send` on
/// Linux. The child process is reaped on a background thread; failures are
/// ignored (a missing notifier is not an error).
pub fn send_desktop_notification(title: &str, body: &str) {
    send_desktop_notification_rich(title, None, body, None);
}

/// Character budget for a notification body handed to a banner transport that
/// passes the text as a process argument (`osascript` / `notify-send`).
///
/// Those fallbacks are bounded by the OS `execve` argv limit, so a full
/// assistant reply (now that turn notifications carry the whole message) can
/// make `Command::spawn` fail with `E2BIG`; the spawn error is best-effort and
/// ignored, which would drop the banner silently. Banner daemons truncate long
/// bodies visually anyway, so bound here. The durable surfaces (email and the
/// chat channels) still receive the unbounded body, and the macOS broker
/// envelope is bounded separately to [`MACOS_NOTIFICATION_MAX_CHARS`].
pub const DESKTOP_BANNER_MAX_CHARS: usize = 4096;

/// Safety ceiling for each text field (title, subtitle, and body) of a macOS
/// Notification Center notification.
///
/// Notification Center is a durable surface: `cc2720058` deliberately made it
/// keep the whole assistant reply, so this ceiling is set far above any
/// realistic reply and only exists to guard the pathological case. macOS
/// rejects a *very* large `UNMutableNotificationContent` body outright (and an
/// oversized body is also a known macOS 27 `NotificationCenter` layout cost).
/// Local probes saw intermittent rejection only from roughly a few hundred KB
/// up and content-dependently, so this is a conservative ceiling rather than a
/// measured limit; keeping it well below that range preserves the full text for
/// any realistic reply while still capping a pathological body. The durable
/// backends (email, Telegram/Discord, ntfy) always receive the unbounded body.
pub const MACOS_NOTIFICATION_MAX_CHARS: usize = 100_000;

/// Truncate `text` to at most `max` characters, appending an ellipsis when cut
/// so a bounded surface still signals that more text exists. Never splits a
/// character.
fn truncate_with_ellipsis(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Bound banner text to a transport-safe size, appending an ellipsis when cut
/// so the rendered banner signals that more text exists.
///
/// Shared with the macOS notification broker, whose `osascript` fallback passes
/// the envelope fields (title, subtitle, body) as process arguments and so has
/// the same argv limit.
pub fn bound_banner_text(text: &str) -> String {
    truncate_with_ellipsis(text, DESKTOP_BANNER_MAX_CHARS)
}

/// Prepare a desktop-banner body/subtitle for an argv transport: strip Markdown
/// markers so the banner shows plain text, then bound it to the `execve`-safe
/// size.
fn banner_body(text: &str) -> String {
    bound_banner_text(&markdown_to_plain_text(text))
}

/// Prepare a desktop-banner title for an argv transport. The title is not
/// Markdown-formatted, so it is only bounded, never converted, preserving the
/// exact text; a long title must still be capped or the spawn would fail.
fn banner_title(text: &str) -> String {
    bound_banner_text(text)
}

/// Bound the text delivered to macOS Notification Center to
/// [`MACOS_NOTIFICATION_MAX_CHARS`].
///
/// The broker's `osascript` fallback additionally bounds to
/// [`DESKTOP_BANNER_MAX_CHARS`] for argv, so this only needs to make the
/// `UNMutableNotificationContent` path safe.
pub fn bound_macos_notification_text(text: &str) -> String {
    truncate_with_ellipsis(text, MACOS_NOTIFICATION_MAX_CHARS)
}

/// Build the AppleScript `display notification` command for a desktop banner.
///
/// All three text fields are expected to be already bounded by the caller (the
/// `execve` argv limit), so this only escapes them for the script. Kept as a
/// free function so the escaping and field layout are unit-testable on any
/// platform.
#[cfg(any(target_os = "macos", test))]
fn macos_display_notification_script(
    title: &str,
    subtitle: Option<&str>,
    body: &str,
    sound: Option<&str>,
) -> String {
    fn applescript_escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => {}
                _ => out.push(ch),
            }
        }
        out
    }

    let mut script = format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_escape(body),
        applescript_escape(title)
    );
    if let Some(subtitle) = subtitle.filter(|s| !s.trim().is_empty()) {
        script.push_str(&format!(" subtitle \"{}\"", applescript_escape(subtitle)));
    }
    if let Some(sound) = sound.filter(|s| !s.trim().is_empty()) {
        script.push_str(&format!(" sound name \"{}\"", applescript_escape(sound)));
    }
    script
}

/// Send a local desktop notification with optional macOS subtitle and sound.
///
/// `subtitle` renders as a second bold line on macOS (ignored elsewhere).
/// `sound` is a Notification Center sound name such as "Glass" or "Ping"
/// (macOS only). Both are best-effort; a missing notifier is not an error.
pub fn send_desktop_notification_rich(
    title: &str,
    subtitle: Option<&str>,
    body: &str,
    sound: Option<&str>,
) {
    // Desktop banners render plain text: a raw Markdown body would show literal
    // `#`, `**`, backticks, and fence markers. Convert to plain text for display
    // while the stored/original body stays intact for other backends, then bound
    // it: the `osascript` / `notify-send` paths below pass the text as a process
    // argument, which has a hard OS limit (see `DESKTOP_BANNER_MAX_CHARS`).
    let body = banner_body(body);
    let title = banner_title(title);
    let subtitle = subtitle.map(banner_body);
    let subtitle = subtitle.as_deref();
    #[cfg(target_os = "macos")]
    {
        let script = macos_display_notification_script(&title, subtitle, &body, sound);
        if let Ok(child) = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            reap_notification_child(child);
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = (subtitle, sound);
        if let Ok(child) = std::process::Command::new("notify-send")
            .arg("--app-name=jcode")
            .arg(&title)
            .arg(&body)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            reap_notification_child(child);
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (title, subtitle, body, sound);
    }
}

// ---------------------------------------------------------------------------
// Desktop (notify-send)
// ---------------------------------------------------------------------------

fn send_desktop(title: &str, body: &str, urgency: &str) {
    // On macOS notify-send does not exist; route through Notification Center.
    #[cfg(target_os = "macos")]
    {
        let _ = urgency;
        send_desktop_notification(title, body);
    }
    #[cfg(not(target_os = "macos"))]
    {
        // notify-send renders plain text, so strip Markdown markers here too;
        // the callers pass the detailed (Markdown) body. Bound it: `notify-send`
        // receives the text as a process argument, which has a hard OS limit.
        let body = bound_banner_text(&markdown_to_plain_text(body));
        let result = std::process::Command::new("notify-send")
            .arg("--app-name=jcode")
            .arg(format!("--urgency={}", urgency))
            .arg("--icon=dialog-information")
            .arg(title)
            .arg(&body)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        match result {
            Ok(status) if status.success() => {
                logging::info(&format!("Desktop notification sent: {}", title));
            }
            Ok(status) => {
                logging::warn(&format!("notify-send exited with {}", status));
            }
            Err(e) => {
                // notify-send not available - not an error, just skip
                logging::info(&format!("notify-send unavailable: {}", e));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// IMAP reply polling
// ---------------------------------------------------------------------------

/// Run an IMAP polling loop checking for replies to ambient emails.
/// Should be spawned as a tokio task alongside the ambient runner.
pub async fn imap_reply_loop(config: SafetyConfig) {
    let host = match config.email_imap_host.as_ref() {
        Some(h) => h.clone(),
        None => {
            logging::error("IMAP reply loop: no imap_host configured");
            return;
        }
    };
    let port = config.email_imap_port;
    let user = match config.email_from.as_ref() {
        Some(u) => u.clone(),
        None => {
            logging::error("IMAP reply loop: no email_from configured");
            return;
        }
    };
    let pass = match config.email_password.as_ref() {
        Some(p) => p.clone(),
        None => {
            logging::error("IMAP reply loop: no email password configured");
            return;
        }
    };

    logging::info(&format!(
        "IMAP reply loop: starting ({}:{}, user: {})",
        host, port, user
    ));

    loop {
        // Run synchronous IMAP in a blocking task
        let h = host.clone();
        let u = user.clone();
        let p = pass.clone();
        let pt = port;
        let result = tokio::task::spawn_blocking(move || poll_imap_once(&h, pt, &u, &p)).await;

        match result {
            Ok(Ok(actions)) => {
                for action in &actions {
                    match action {
                        ReplyAction::PermissionDecision {
                            request_id,
                            approved,
                            message,
                        } => {
                            if let Err(e) = crate::safety::record_permission_via_file(
                                request_id,
                                *approved,
                                "email_reply",
                                message.clone(),
                            ) {
                                logging::error(&format!(
                                    "Failed to record permission decision for {}: {}",
                                    request_id, e
                                ));
                            } else {
                                logging::info(&format!(
                                    "Permission {} via email: {}",
                                    if *approved { "approved" } else { "denied" },
                                    request_id
                                ));
                            }
                        }
                        ReplyAction::DirectiveReply { cycle_id, text } => {
                            if let Err(e) =
                                crate::ambient::add_directive(text.clone(), cycle_id.clone())
                            {
                                logging::error(&format!("Failed to save directive: {}", e));
                            }
                        }
                    }
                }

                if !actions.is_empty() {
                    logging::info(&format!("IMAP: processed {} email replies", actions.len()));
                }
            }
            Ok(Err(e)) => {
                logging::error(&format!("IMAP poll error: {}", e));
            }
            Err(e) => {
                logging::error(&format!("IMAP poll task panicked: {}", e));
            }
        }

        // Poll every 60 seconds
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// Best-effort Markdown to plain text for banner surfaces (macOS Notification
/// Center, Linux `notify-send`, terminal-native notifications) that cannot
/// render Markdown.
///
/// This deliberately does not aim for full Markdown fidelity; it removes the
/// noise that would otherwise show up literally: ATX heading markers, emphasis
/// and inline-code markers, link syntax (keeping the label), and code-fence
/// delimiters. Content is otherwise preserved (including line breaks).
pub fn markdown_to_plain_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_fence = false;
    for raw_line in input.split('\n') {
        if !out.is_empty() {
            out.push('\n');
        }
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            // Drop fence delimiters entirely; keep the code lines between them.
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push_str(raw_line);
            continue;
        }
        out.push_str(&strip_inline_markdown(raw_line));
    }
    out
}

/// Upper bound on how far the link lookahead scans for a `]` / `)`. A real
/// Markdown link label or URL is far shorter than this; bounding the scan keeps
/// bracket-heavy text linear (`markdown_to_plain_text` runs over the full,
/// unbounded assistant reply on the TUI thread).
const MAX_LINK_LABEL_SCAN: usize = 512;
const MAX_LINK_URL_SCAN: usize = 2048;

/// Strip block/inline Markdown markers from a single line.
fn strip_inline_markdown(line: &str) -> String {
    // Leading ATX heading markers: "### Title" -> "Title".
    let mut s = line.trim_start();
    let hashes = s.len() - s.trim_start_matches('#').len();
    if hashes > 0 && s[hashes..].starts_with(' ') {
        s = s[hashes..].trim_start();
    }
    // Blockquote and list markers at the start of the line.
    for marker in ["> ", "- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(marker) {
            s = rest;
            break;
        }
    }
    let chars: Vec<char> = s.chars().collect();
    // A marker is only formatting when it hugs a word: opening markers are
    // preceded by a boundary and followed by an alphanumeric; closing markers
    // are the reverse. This preserves literal markers in content such as
    // `2*3`, `snake_case`, or `a|b` while still dropping real `**bold**`.
    let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if matches!(ch, '*' | '_' | '`') {
            // Consume the whole run of the same marker (e.g. `**`).
            let start = i;
            while i < chars.len() && chars[i] == ch {
                i += 1;
            }
            let prev = if start == 0 {
                None
            } else {
                Some(chars[start - 1])
            };
            let next = chars.get(i).copied();
            let opening = boundary(prev) && next.is_some_and(|c| c.is_alphanumeric());
            let closing = prev.is_some_and(|c| c.is_alphanumeric()) && boundary(next);
            if !(opening || closing) {
                // Not formatting in this context: keep the literal run.
                for _ in start..i {
                    out.push(ch);
                }
            }
            continue;
        }
        if ch == '[' {
            // A link keeps its label and drops the URL: "[label](url)" -> "label".
            // The lookahead is bounded (and stops at a newline) so a reply made
            // of many `[` cannot scan to the end of the string on every bracket
            // and go quadratic: `markdown_to_plain_text` runs over the full,
            // unbounded assistant reply on the TUI thread.
            let label_start = i + 1;
            let label_scan_end = label_start.saturating_add(MAX_LINK_LABEL_SCAN).min(chars.len());
            let mut j = label_start;
            while j < label_scan_end && chars[j] != ']' && chars[j] != '\n' {
                j += 1;
            }
            let closed = j < chars.len() && chars[j] == ']';
            if closed && chars.get(j + 1) == Some(&'(') {
                let url_start = j + 2;
                let url_scan_end = url_start.saturating_add(MAX_LINK_URL_SCAN).min(chars.len());
                let mut k = url_start;
                while k < url_scan_end && chars[k] != ')' && chars[k] != '\n' {
                    k += 1;
                }
                let closed_url = k < chars.len() && chars[k] == ')';
                // Mirror the old behavior: an image keeps only its alt text and
                // a `(` with no closing `)` drops the remainder.
                if closed_url || k >= chars.len() {
                    // Image syntax `![alt](url)` reads best as just the alt text:
                    // drop a leading `!` we already emitted for it.
                    if out.ends_with('!') {
                        out.pop();
                    }
                    out.extend(chars[label_start..j].iter());
                    i = if closed_url { k + 1 } else { chars.len() };
                    continue;
                }
            }
            // Not a link: emit a literal '['.
            out.push('[');
            i += 1;
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Sanitized body for potentially public channels (ntfy.sh).
/// Only includes counts and status — no model-generated text.
fn format_cycle_body_safe(transcript: &AmbientTranscript) -> String {
    let mut lines = Vec::new();

    lines.push(format!("Status: {:?}", transcript.status));
    lines.push(format!(
        "Memories modified: {}",
        transcript.memories_modified
    ));
    lines.push(format!("Compactions: {}", transcript.compactions));

    if transcript.pending_permissions > 0 {
        lines.push(format!(
            "{} permission request(s) pending",
            transcript.pending_permissions
        ));
    }

    lines.push("Check jcode for full details.".to_string());
    lines.join("\n")
}

/// Full detailed body for private channels (email, desktop).
/// Includes the model-generated summary and provider info.
/// Output is markdown — rendered to HTML for email, plain text for desktop.
fn format_cycle_body_detailed(transcript: &AmbientTranscript) -> String {
    let mut lines = Vec::new();

    if let Some(ref summary) = transcript.summary {
        lines.push("# Summary".to_string());
        lines.push(String::new());
        lines.push(summary.clone());
        lines.push(String::new());
    }

    lines.push("---".to_string());
    lines.push(String::new());
    lines.push(format!(
        "**Status:** {:?} · **Provider:** {} ({}) · **Memories:** {} · **Compactions:** {}",
        transcript.status,
        transcript.provider,
        transcript.model,
        transcript.memories_modified,
        transcript.compactions,
    ));

    if transcript.pending_permissions > 0 {
        lines.push(String::new());
        lines.push(format!(
            "**⚠ {} permission request(s) pending** — review in jcode",
            transcript.pending_permissions
        ));
    }

    // Include full conversation transcript if available
    if let Some(ref conversation) = transcript.conversation {
        lines.push(String::new());
        lines.push("---".to_string());
        lines.push(String::new());
        lines.push("# Full Transcript".to_string());
        lines.push(String::new());
        lines.push(conversation.clone());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_cycle_body_safe() {
        let transcript = AmbientTranscript {
            session_id: "test_001".to_string(),
            started_at: chrono::Utc::now(),
            ended_at: Some(chrono::Utc::now()),
            status: crate::safety::TranscriptStatus::Complete,
            provider: "claude".to_string(),
            model: "claude-sonnet-4".to_string(),
            actions: Vec::new(),
            pending_permissions: 0,
            summary: Some("Cleaned up 3 stale memories.".to_string()),
            compactions: 1,
            memories_modified: 3,
            conversation: None,
        };

        let body = format_cycle_body_safe(&transcript);
        assert!(body.contains("Memories modified: 3"));
        assert!(body.contains("Compactions: 1"));
        assert!(body.contains("Check jcode for full details"));
        // Safe body must NOT include model-generated summary
        assert!(!body.contains("Cleaned up"));
        assert!(!body.contains("permission"));
    }

    #[test]
    fn test_format_cycle_body_detailed() {
        let transcript = AmbientTranscript {
            session_id: "test_001".to_string(),
            started_at: chrono::Utc::now(),
            ended_at: Some(chrono::Utc::now()),
            status: crate::safety::TranscriptStatus::Complete,
            provider: "claude".to_string(),
            model: "claude-sonnet-4".to_string(),
            actions: Vec::new(),
            pending_permissions: 0,
            summary: Some("Cleaned up 3 stale memories.".to_string()),
            compactions: 1,
            memories_modified: 3,
            conversation: Some("### User\n\nBegin cycle.\n\n### Assistant\n\nDone.\n".to_string()),
        };

        let body = format_cycle_body_detailed(&transcript);
        // Detailed body SHOULD include the summary
        assert!(body.contains("Cleaned up 3 stale memories."));
        assert!(body.contains("**Memories:** 3"));
        assert!(body.contains("claude"));
        // Should include conversation transcript
        assert!(body.contains("# Full Transcript"));
        assert!(body.contains("### User"));
        assert!(body.contains("Begin cycle."));
    }

    #[test]
    fn test_format_cycle_body_with_pending_permissions() {
        let transcript = AmbientTranscript {
            session_id: "test_002".to_string(),
            started_at: chrono::Utc::now(),
            ended_at: Some(chrono::Utc::now()),
            status: crate::safety::TranscriptStatus::Complete,
            provider: "claude".to_string(),
            model: "claude-sonnet-4".to_string(),
            actions: Vec::new(),
            pending_permissions: 2,
            summary: None,
            compactions: 0,
            memories_modified: 0,
            conversation: None,
        };

        let safe = format_cycle_body_safe(&transcript);
        assert!(safe.contains("2 permission request(s) pending"));
        assert!(safe.contains("Check jcode for full details"));

        let detailed = format_cycle_body_detailed(&transcript);
        assert!(detailed.contains("2 permission request(s) pending"));
    }

    #[test]
    fn markdown_to_plain_text_strips_banner_noise() {
        let md = "# Title\n\n**bold** and `code`\n\n- item\n> quote\n[link](https://x)\n\n```rust\nlet a = 1;\n```";
        let plain = markdown_to_plain_text(md);
        assert!(plain.contains("Title"));
        assert!(plain.contains("bold and code"));
        assert!(!plain.contains("- item"), "list marker stripped");
        assert!(plain.contains("item"));
        assert!(plain.contains("quote"));
        assert!(plain.contains("link"), "keeps link label");
        assert!(!plain.contains("https://x"), "drops link URL");
        assert!(!plain.contains("**") && !plain.contains('`'));
        assert!(!plain.contains("```"));
        assert!(plain.contains("let a = 1;"), "keeps fenced code content");
    }

    #[test]
    fn markdown_to_plain_text_keeps_literal_markers_in_content() {
        // Markers that are not formatting (no hugging alphanumerics) must be
        // preserved so formulas, identifiers, and citations are not corrupted.
        assert_eq!(markdown_to_plain_text("2*3 = 6"), "2*3 = 6");
        assert_eq!(markdown_to_plain_text("snake_case_name"), "snake_case_name");
        assert_eq!(markdown_to_plain_text("see [1] and [2]"), "see [1] and [2]");
        assert_eq!(markdown_to_plain_text("a ` b"), "a ` b");
        // Real formatting is still stripped.
        assert_eq!(markdown_to_plain_text("**bold**"), "bold");
        assert_eq!(markdown_to_plain_text("`code`"), "code");
        assert_eq!(markdown_to_plain_text("_italic_"), "italic");
        // Links keep their label; images show their alt text without the `![`.
        assert_eq!(markdown_to_plain_text("see [docs](https://x)"), "see docs");
        assert_eq!(markdown_to_plain_text("![diagram](https://y)"), "diagram");
    }

    #[test]
    fn markdown_to_plain_text_stays_linear_on_bracket_heavy_reply() {
        // A reply that is wall-to-wall `[` (or `](`) must not make the link
        // lookahead quadratic. This guards the TUI-thread stall regression: a
        // large reply is converted for the banner/terminal sinks in one call.
        let huge = "[".repeat(200_000);
        let start = std::time::Instant::now();
        let plain = markdown_to_plain_text(&huge);
        assert_eq!(plain, huge);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "bracket-heavy strip took {:?}, expected linear time",
            start.elapsed()
        );

        // `](`-heavy input exercises the URL lookahead the same way.
        let huge = "[a](".repeat(50_000);
        let start = std::time::Instant::now();
        let _ = markdown_to_plain_text(&huge);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "url-heavy strip took {:?}, expected linear time",
            start.elapsed()
        );
    }

    #[test]
    fn markdown_to_plain_text_preserves_plain_text() {
        let plain = "Just a normal reply.\nSecond line.";
        assert_eq!(markdown_to_plain_text(plain), plain);
    }

    #[test]
    fn macos_envelope_converts_body_to_plain_text() {
        let envelope = macos_turn_envelope(
            "jcode-turn-test-id".to_string(),
            "jcode · done",
            Some("2/2 todos"),
            "# Result\n\n**Bold** and `code` here",
            None,
            MacosNotificationOrigin {
                terminal: MacosTerminalKind::Unknown,
                bundle_id: None,
                tty: None,
                session_id: None,
            },
        );
        assert!(!envelope.body.contains('#'), "heading marker stripped");
        assert!(!envelope.body.contains('*'), "emphasis marker stripped");
        assert!(!envelope.body.contains('`'), "code marker stripped");
        assert!(envelope.body.contains("Result"));
        assert!(envelope.body.contains("Bold and code here"));
        assert_eq!(envelope.subtitle.as_deref(), Some("2/2 todos"));
    }

    #[test]
    fn macos_envelope_bounds_body_for_notification_center() {
        // A pathologically large assistant reply must not reach
        // `UNMutableNotificationContent` unbounded: on macOS 27 an oversized
        // body is a NotificationCenter layout cost and can be rejected
        // outright. The ceiling is far above any realistic reply, so only the
        // pathological case is capped; the durable backends keep the full text.
        let huge = "x".repeat(MACOS_NOTIFICATION_MAX_CHARS + 10_000);
        let envelope = macos_turn_envelope(
            "jcode-turn-test-id".to_string(),
            &huge,
            Some(&huge),
            &huge,
            None,
            MacosNotificationOrigin {
                terminal: MacosTerminalKind::Unknown,
                bundle_id: None,
                tty: None,
                session_id: None,
            },
        );
        assert_eq!(envelope.body.chars().count(), MACOS_NOTIFICATION_MAX_CHARS);
        assert!(envelope.body.ends_with('…'));
        // The title is rendered by Notification Center too, so it is bounded
        // the same way.
        assert_eq!(envelope.title.chars().count(), MACOS_NOTIFICATION_MAX_CHARS);
        assert!(envelope.title.ends_with('…'));
        assert_eq!(
            envelope
                .subtitle
                .as_deref()
                .map(str::chars)
                .map(Iterator::count),
            Some(MACOS_NOTIFICATION_MAX_CHARS)
        );
        // A short body is untouched.
        let short = macos_turn_envelope(
            "jcode-turn-test-id".to_string(),
            "jcode · done",
            None,
            "Fixed the parser bug.",
            None,
            MacosNotificationOrigin {
                terminal: MacosTerminalKind::Unknown,
                bundle_id: None,
                tty: None,
                session_id: None,
            },
        );
        assert_eq!(short.body, "Fixed the parser bug.");
    }

    #[test]
    fn macos_envelope_keeps_a_realistic_reply_uncapped() {
        // Notification Center is a durable surface: a long-but-realistic reply
        // (well past the old 120-char snippet, and past any small banner cap)
        // must pass through in full. Only a pathological size is capped.
        let reply = format!(
            "{}\n\n{}",
            "## Summary".repeat(50),
            "detail line.\n".repeat(2000)
        );
        assert!(reply.chars().count() > 20_000);
        assert!(reply.chars().count() < MACOS_NOTIFICATION_MAX_CHARS);
        let envelope = macos_turn_envelope(
            "jcode-turn-test-id".to_string(),
            "jcode · fox · done in 1m",
            None,
            &reply,
            None,
            MacosNotificationOrigin {
                terminal: MacosTerminalKind::Unknown,
                bundle_id: None,
                tty: None,
                session_id: None,
            },
        );
        // No ellipsis: the whole reply survived the envelope.
        assert!(!envelope.body.ends_with('…'));
        assert!(!envelope.body.contains('…'));
        assert_eq!(envelope.body, markdown_to_plain_text(&reply));
    }

    #[test]
    fn macos_notification_bound_never_splits_multibyte() {
        let unicode = "é😀中".repeat(MACOS_NOTIFICATION_MAX_CHARS);
        let bounded = bound_macos_notification_text(&unicode);
        assert_eq!(bounded.chars().count(), MACOS_NOTIFICATION_MAX_CHARS);
        assert!(bounded.ends_with('…'));
    }

    #[test]
    fn dispatch_rich_keeps_safe_and_detailed_bodies_separate() {
        // Structural check: the rich entry point exists and delegates with two
        // distinct bodies. Actual delivery is exercised via the channel tests.
        let cfg = SafetyConfig::default();
        let dispatcher = NotificationDispatcher::from_config(cfg);
        // No runtime + no backends configured: must not panic.
        dispatcher.dispatch_rich(
            "title",
            "safe body",
            "detailed body",
            Priority::Default,
            Some("session"),
        );
    }

    #[test]
    fn bound_banner_text_caps_argv_transport() {
        // A short body is unchanged.
        assert_eq!(bound_banner_text("hello"), "hello");
        // A body at the limit is unchanged; one over is cut with an ellipsis,
        // so the execve argv passed to osascript/notify-send stays bounded.
        let at_limit = "x".repeat(DESKTOP_BANNER_MAX_CHARS);
        assert_eq!(bound_banner_text(&at_limit), at_limit);
        let over = "x".repeat(DESKTOP_BANNER_MAX_CHARS + 500);
        let bounded = bound_banner_text(&over);
        assert_eq!(bounded.chars().count(), DESKTOP_BANNER_MAX_CHARS);
        assert!(bounded.ends_with('…'));
        // Multibyte text must not be split mid-char.
        let unicode = "é😀中".repeat(DESKTOP_BANNER_MAX_CHARS);
        let bounded = bound_banner_text(&unicode);
        assert_eq!(bounded.chars().count(), DESKTOP_BANNER_MAX_CHARS);
        assert!(bounded.ends_with('…'));
    }

    #[test]
    fn macos_banner_script_layout_and_escaping() {
        // Build the script from already-bounded pieces: body first, title
        // second, optional subtitle and sound appended in order.
        let script = macos_display_notification_script(
            "turn complete",
            Some("session 1"),
            "all done",
            Some("Glass"),
        );
        assert_eq!(
            script,
            "display notification \"all done\" with title \"turn complete\" \
             subtitle \"session 1\" sound name \"Glass\""
        );
        // Quotes, backslashes, and newlines must be escaped so the embedded
        // string cannot break out of the AppleScript literal.
        let script = macos_display_notification_script("a\"b\\c\nd", None, "body", None);
        assert_eq!(
            script,
            "display notification \"body\" with title \"a\\\"b\\\\c\\nd\""
        );
        // An empty subtitle (or whitespace-only) is omitted entirely rather
        // than emitted as an empty literal.
        let script = macos_display_notification_script("t", Some("   "), "b", None);
        assert_eq!(script, "display notification \"b\" with title \"t\"");
    }

    #[test]
    fn desktop_banner_title_is_bounded() {
        // The desktop banner passes the title as a process argument, so a huge
        // title must be bounded the same way the body is; otherwise the
        // osascript/notify-send spawn fails with E2BIG and drops the banner.
        // Exercise the real builder so this cannot pass while the production
        // path forgets to bound the title.
        let huge_title = "T".repeat(DESKTOP_BANNER_MAX_CHARS + 1000);
        let script =
            macos_display_notification_script(&banner_title(&huge_title), None, "body", None);
        // The rendered title literal is bounded: the script length stays well
        // under the argv limit even though the input was far over it.
        assert!(script.chars().count() < DESKTOP_BANNER_MAX_CHARS + 64);
        assert!(script.contains('…'));
        // A short title is preserved verbatim (not Markdown-converted).
        assert_eq!(
            banner_title("jcode · fox · done in 12m 34s"),
            "jcode · fox · done in 12m 34s"
        );
        assert_eq!(banner_title("a_b_c"), "a_b_c");
    }

    #[test]
    fn test_priority_values() {
        assert_eq!(Priority::Default.ntfy_value(), "3");
        assert_eq!(Priority::High.ntfy_value(), "4");
        assert_eq!(Priority::Urgent.ntfy_value(), "5");
    }

    #[test]
    fn test_dispatcher_creation() {
        // Just verify it doesn't panic
        let cfg = SafetyConfig::default();
        let _dispatcher = NotificationDispatcher::from_config(cfg);
    }

    #[test]
    fn macos_origin_detects_terminal_identifiers() {
        let terminal = MacosNotificationOrigin::from_values(
            "Apple_Terminal",
            "xterm-256color",
            Some("/dev/ttys007"),
            Some("4F3C"),
            None,
            false,
        );
        assert_eq!(terminal.terminal, MacosTerminalKind::AppleTerminal);
        assert_eq!(terminal.bundle_id.as_deref(), Some("com.apple.Terminal"));
        assert_eq!(terminal.tty.as_deref(), Some("/dev/ttys007"));
        assert_eq!(terminal.session_id.as_deref(), Some("4F3C"));

        let iterm = MacosNotificationOrigin::from_values(
            "iTerm.app",
            "xterm-256color",
            Some("/dev/ttys011"),
            None,
            Some("w0t1p0:ABC"),
            false,
        );
        assert_eq!(iterm.terminal, MacosTerminalKind::Iterm2);
        assert_eq!(iterm.session_id.as_deref(), Some("w0t1p0:ABC"));

        let ghostty = MacosNotificationOrigin::from_values(
            "",
            "xterm-ghostty",
            Some("/dev/ttys019"),
            None,
            None,
            true,
        );
        assert_eq!(ghostty.terminal, MacosTerminalKind::Ghostty);
        assert_eq!(ghostty.bundle_id.as_deref(), Some("com.mitchellh.ghostty"));
    }

    #[test]
    fn macos_origin_rejects_untrusted_route_values() {
        let origin = MacosNotificationOrigin::from_values(
            "Apple_Terminal",
            "",
            Some("/dev/ttys001\"\nrun script"),
            Some("bad\nidentifier"),
            None,
            false,
        );
        assert_eq!(origin.tty, None);
        assert_eq!(origin.session_id, None);

        let (_, args) = macos_notification_activation_command(&origin).expect("Terminal route");
        assert_eq!(
            args,
            vec!["-e", "tell application \"Terminal\" to activate"]
        );
    }

    #[test]
    fn macos_activation_targets_terminal_and_iterm_ttys() {
        let terminal = MacosNotificationOrigin {
            terminal: MacosTerminalKind::AppleTerminal,
            bundle_id: Some("com.apple.Terminal".to_string()),
            tty: Some("/dev/ttys003".to_string()),
            session_id: Some("session-a".to_string()),
        };
        let (program, args) =
            macos_notification_activation_command(&terminal).expect("Terminal command");
        assert_eq!(program, "/usr/bin/osascript");
        assert!(args[1].contains("if tty of t is \"/dev/ttys003\""));
        assert!(args[1].contains("set selected tab of w to t"));

        let iterm = MacosNotificationOrigin {
            terminal: MacosTerminalKind::Iterm2,
            bundle_id: Some("com.googlecode.iterm2".to_string()),
            tty: Some("/dev/ttys004".to_string()),
            session_id: Some("w0t0p0:guid".to_string()),
        };
        let (_, args) = macos_notification_activation_command(&iterm).expect("iTerm command");
        assert!(args[1].contains("if tty of s is \"/dev/ttys004\""));
        assert!(args[1].contains("select s"));
    }

    #[test]
    fn macos_ghostty_activation_is_application_scoped() {
        let origin = MacosNotificationOrigin {
            terminal: MacosTerminalKind::Ghostty,
            bundle_id: Some("evil.bundle".to_string()),
            tty: Some("/dev/ttys005".to_string()),
            session_id: None,
        };
        assert_eq!(
            macos_notification_activation_command(&origin),
            Some((
                "/usr/bin/open".to_string(),
                vec!["-b".to_string(), "com.mitchellh.ghostty".to_string()]
            ))
        );
    }

    #[test]
    fn macos_envelope_roundtrip_preserves_origin_metadata() {
        let envelope = MacosNotificationEnvelope {
            schema_version: MACOS_NOTIFICATION_SCHEMA_VERSION,
            notification_id: "jcode-turn-test".to_string(),
            title: "jcode · done".to_string(),
            subtitle: Some("2/2 todos".to_string()),
            body: "Finished broker".to_string(),
            sound: Some("Glass".to_string()),
            origin: MacosNotificationOrigin {
                terminal: MacosTerminalKind::Iterm2,
                bundle_id: Some("com.googlecode.iterm2".to_string()),
                tty: Some("/dev/ttys009".to_string()),
                session_id: Some("w1t2p0:route".to_string()),
            },
        };
        let encoded = serde_json::to_vec(&envelope).expect("encode envelope");
        let decoded: MacosNotificationEnvelope =
            serde_json::from_slice(&encoded).expect("decode envelope");
        assert_eq!(decoded, envelope);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod notification_process_tests {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/support/notification_reaping.rs"
    ));
}
