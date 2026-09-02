use anyhow::Result;
use std::io::{self, IsTerminal, Write};
use std::panic;
use std::sync::RwLock;

use crate::{id, session, telemetry, tui};
use jcode_tui::tui::terminal_writer::AppTerminal;

/// A function usable as (part of) a Rust panic hook.
type PanicHook = dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send;

/// The original Rust default panic hook, captured the first time jcode installs
/// its own hook (see [`install_panic_hook`]).
///
/// An `RwLock` rather than a `OnceLock` so that unit tests can install test
/// doubles: `OnceLock::set` silently ignores the second write, which makes
/// sibling tests that each install their own default hook order-dependent.
/// In production the write happens once at startup, and the panic-path read is
/// a cheap read lock that never contends a write (no write occurs during a
/// panic).
static DEFAULT_HOOK: RwLock<Option<Box<PanicHook>>> = RwLock::new(None);

/// True once the TUI has entered raw mode and the alternate screen, i.e. after
/// `ratatui::init()` (or the resume path) has run. The panic hook only restores
/// the terminal from inside a live TUI; outside one there is nothing to restore
/// and `try_restore()` would just emit escape sequences onto stdout.
static TUI_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn tui_is_active() -> bool {
    TUI_ACTIVE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Serializes the tests that read or write the process-global panic-hook / TUI
/// state (`DEFAULT_HOOK`, `TUI_ACTIVE`, the current session). Those globals are
/// shared across the test binaries, so without this lock sibling tests running
/// on parallel threads race each other and flake.
#[cfg(test)]
static GLOBAL_HOOK_TEST_LOCK: RwLock<()> = RwLock::new(());

pub struct TuiRuntimeState {
    mouse_capture: bool,
    keyboard_enhanced: bool,
    focus_change: bool,
}

const INHERITED_MODES_ENV: &str = "JCODE_TUI_INHERITED_MODES";
const INHERITED_THEME_ENV: &str = "JCODE_TUI_INHERITED_THEME";

// Crossterm's Windows implementation enables Win32 console mouse input but does
// not emit the VT mouse-tracking modes. Windows Terminal and other ConPTY hosts
// use those VT modes to decide whether a wheel detent is a mouse event or should
// be translated into Up/Down keys in the alternate screen. Without this second
// signal, wheel scrolling can accidentally browse prompt history instead of the
// chat transcript even though crossterm reports mouse capture as enabled.
#[cfg(any(windows, test))]
const WINDOWS_VT_MOUSE_ENABLE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h";
#[cfg(any(windows, test))]
const WINDOWS_VT_MOUSE_DISABLE: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

#[cfg(windows)]
fn sync_windows_vt_mouse_capture(enabled: bool) -> io::Result<()> {
    let sequence = if enabled {
        WINDOWS_VT_MOUSE_ENABLE
    } else {
        WINDOWS_VT_MOUSE_DISABLE
    };
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(sequence)?;
    stdout.flush()
}

#[cfg(not(windows))]
fn sync_windows_vt_mouse_capture(_enabled: bool) -> io::Result<()> {
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InheritedTerminalModes {
    mouse_capture: bool,
    keyboard_enhanced: bool,
    focus_change: bool,
}

impl InheritedTerminalModes {
    fn encode(self) -> String {
        format!(
            "mouse={},keyboard={},focus={}",
            u8::from(self.mouse_capture),
            u8::from(self.keyboard_enhanced),
            u8::from(self.focus_change)
        )
    }

    fn decode(value: &str) -> Option<Self> {
        let mut modes = Self {
            mouse_capture: false,
            keyboard_enhanced: false,
            focus_change: false,
        };
        let mut seen = 0u8;
        for field in value.split(',') {
            let (name, raw) = field.split_once('=')?;
            let enabled = match raw {
                "0" => false,
                "1" => true,
                _ => return None,
            };
            match name {
                "mouse" => {
                    modes.mouse_capture = enabled;
                    seen |= 1;
                }
                "keyboard" => {
                    modes.keyboard_enhanced = enabled;
                    seen |= 2;
                }
                "focus" => {
                    modes.focus_change = enabled;
                    seen |= 4;
                }
                _ => return None,
            }
        }
        (seen == 7).then_some(modes)
    }
}

fn has_terminal_exec_handoff(
    is_resuming: bool,
    inherited_modes: Option<InheritedTerminalModes>,
) -> bool {
    is_resuming && inherited_modes.is_some()
}

/// RAII guard that guarantees the terminal is restored to a sane state when the
/// TUI runtime ends, even if the run loop returns an error or unwinds via panic.
///
/// Without this guard, an error propagated by `?` (e.g. an I/O error from a
/// `terminal.draw` call, or any other fallible step in the event loop) would
/// skip the explicit `cleanup_tui_runtime` call and leave the terminal in raw
/// mode / alternate screen. That manifests as a corrupted terminal after exit:
/// typed input is invisible because echo and cooked mode were never restored
/// (see issue #214).
///
/// The normal teardown path should call [`TuiRuntimeGuard::finish`] (or
/// [`TuiRuntimeGuard::finish_for_run_result`]) which performs the restore and
/// disarms the guard. If neither is called (error/panic path), `Drop` performs
/// a best-effort full restore.
pub struct TuiRuntimeGuard {
    state: TuiRuntimeState,
    armed: bool,
}

#[cfg(test)]
thread_local! {
    /// Counts how many times the guard's `Drop` performed an emergency restore.
    /// Used by tests to verify the error/panic safety net fires exactly once.
    static GUARD_DROP_RESTORES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

impl TuiRuntimeGuard {
    fn new(state: TuiRuntimeState) -> Self {
        Self { state, armed: true }
    }

    /// Normal teardown for the simple case: restore the terminal and disarm.
    pub fn finish(mut self, restore_terminal: bool) {
        cleanup_tui_runtime(&self.state, restore_terminal);
        self.armed = false;
    }

    /// Normal teardown for the interactive client: restore unless we are about
    /// to exec a follow-up process (reload/rebuild/update), in which case the
    /// next process inherits the terminal modes.
    pub fn finish_for_run_result(mut self, run_result: &crate::tui::RunResult, extra_exec: bool) {
        if run_result_will_exec(run_result, extra_exec) {
            export_tui_exec_handoff(&self.state);
        }
        cleanup_tui_runtime_for_run_result(&self.state, run_result, extra_exec);
        self.armed = false;
    }
}

impl Drop for TuiRuntimeGuard {
    fn drop(&mut self) {
        if self.armed {
            // Reached only on an error/panic path that skipped explicit
            // teardown. Always perform a full restore so the user's terminal is
            // not left corrupted.
            cleanup_tui_runtime(&self.state, true);
            self.armed = false;
            #[cfg(test)]
            GUARD_DROP_RESTORES.with(|c| c.set(c.get() + 1));
        }
    }
}

pub fn set_current_session(session_id: &str) {
    crate::set_current_session(session_id);
}

pub fn get_current_session() -> Option<String> {
    crate::get_current_session()
}

/// Whether a panic in this process should relabel the on-disk session as crashed.
///
/// Only an `Active` session can legitimately make that transition. A dying
/// client (closed terminal window, dropped SSH) must not relabel a session that
/// the shared server still owns, nor write its stale snapshot over the server's
/// newer one. See #599; `mark_current_session_crashed` already had this guard.
fn should_record_panic_as_crash(status: &session::SessionStatus) -> bool {
    matches!(status, session::SessionStatus::Active)
}

pub fn install_panic_hook() {
    let default_hook = panic::take_hook();
    set_default_hook(default_hook);
    reinstall_panic_hook();
}

/// The original Rust default panic hook, captured the first time we install our
/// hook. We keep it so that after `ratatui::init()` wraps the hook, we can
/// rebuild a healthy chain that still prints a backtrace without ratatui's
/// panicking `restore()`.
fn set_default_hook(hook: Box<PanicHook>) {
    *DEFAULT_HOOK.write().expect("DEFAULT_HOOK lock poisoned") = Some(hook);
}

/// Install jcode's panic hook chain: restore the terminal safely (non-panicking),
/// then print the standard backtrace, then mark the current session crashed.
/// Used at startup and re-run after `ratatui::init()`.
fn reinstall_panic_hook() {
    panic::set_hook(Box::new(move |info| {
        // Restore the terminal safely before doing anything that could itself
        // panic again. std's default hook (below) prints through `eprintln!`,
        // which aborts on a dead stderr; restoring first guarantees the terminal
        // is left in a good state even if the backtrace print aborts. Doing it
        // only from inside a live TUI (gated on `tui_is_active()`) keeps the
        // restore off the non-TUI/CLI panic path, where it would just emit escape
        // sequences onto stdout. Covering this here (rather than relying only on
        // `TuiRuntimeGuard::Drop`) also protects the window before the guard is
        // constructed during `init_tui_runtime`.
        if tui_is_active() {
            jcode_tui_style::restore_terminal_quietly();
            TUI_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
        }

        // Printing the backtrace last also means it lands on the cooked screen
        // (after leaving the alternate screen) instead of being torn down with
        // the alternate buffer.
        if let Ok(guard) = DEFAULT_HOOK.read()
            && let Some(default_hook) = guard.as_deref()
        {
            default_hook(info);
        }

        if let Some(session_id) = get_current_session() {
            print_session_resume_hint(&session_id);

            if let Some((provider, model)) = telemetry::current_provider_model() {
                telemetry::record_crash(&provider, &model, telemetry::SessionEndReason::Panic);
            }

            if let Ok(mut session) = session::Session::load(&session_id)
                && should_record_panic_as_crash(&session.status)
            {
                session.mark_crashed(Some(format!("Panic: {}", info)));
                let _ = session.save();
            }
        }
    }));
}

/// Make jcode's panic hook the outermost hook, again.
///
/// `ratatui::init()` installs its own panic hook that runs `ratatui::restore()`
/// before chaining to the previous hook. `restore()` reports failures with
/// `eprintln!`, which panics on a dead terminal (dropped SSH, closed window) —
/// the exact scenario a panic hook is most likely to run in. That second panic
/// aborts the process (SIGABRT) and, worse, can clobber a live session's
/// snapshot (see #599).
///
/// Calling this after `ratatui::init()` (in `init_tui_runtime`) replaces the
/// wrapped hook with our own chain that prints the backtrace and restores the
/// terminal via the safe, non-panicking `restore_terminal_quietly`. The
/// panicking `ratatui::restore()` therefore never runs.
pub fn reinstall_panic_hook_outermost() {
    reinstall_panic_hook();
}

pub fn mark_current_session_crashed(message: String) {
    if let Some(session_id) = get_current_session() {
        if let Some((provider, model)) = telemetry::current_provider_model() {
            telemetry::record_crash(&provider, &model, telemetry::SessionEndReason::Signal);
        }
        if let Ok(mut session) = session::Session::load(&session_id)
            && matches!(session.status, session::SessionStatus::Active)
        {
            session.mark_crashed(Some(message));
            let _ = session.save();
        }
    }
}

pub fn panic_payload_to_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

pub fn show_crash_resume_hint() {
    let crashed = session::find_recent_crashed_sessions();
    if crashed.is_empty() {
        return;
    }

    // Crash hints print outside the TUI, possibly on a console that never had
    // VT processing enabled (issue #498), so gate the color codes.
    let ansi = crate::console::stderr_supports_ansi();
    let (yellow, bold, reset) = if ansi {
        ("\x1b[33m", "\x1b[1m", "\x1b[0m")
    } else {
        ("", "", "")
    };

    for line in crash_resume_hint_lines(&crashed, yellow, bold, reset) {
        crate::cli::output::tolerant_write_line(
            &mut std::io::stderr(),
            &crate::output_style::terminal_text(&line),
        );
    }
    crate::cli::output::tolerant_write(&mut std::io::stderr(), "\n");
}

/// Build the crash-resume hint lines for `crashed`, newest first.
///
/// Pure so the wording is testable: the lines are printed to stderr outside the
/// TUI, where nothing asserts on them, and the bug in issue #690 was purely
/// about wording (the single-session form never mentioned that bare
/// `jcode --resume` opens a searchable picker, so it read as "memorize this ID
/// or lose the session").
fn crash_resume_hint_lines(
    crashed: &[(String, String)],
    yellow: &str,
    bold: &str,
    reset: &str,
) -> Vec<String> {
    let Some((id, name)) = crashed.first() else {
        return Vec::new();
    };
    let session_label = id::extract_session_name(id).unwrap_or(name.as_str());

    if crashed.len() == 1 {
        vec![
            format!(
                "{yellow}💥 Session {bold}{session_label}{reset}{yellow} crashed. Resume with:{reset}  jcode --resume {id}"
            ),
            // Always mention the picker. Showing only the ID form reads as
            // "write this down or lose the session", when bare
            // `jcode --resume` opens a searchable list (issue #690).
            format!("{yellow}   Or browse all:{reset} jcode --resume"),
        ]
    } else {
        vec![
            format!(
                "{yellow}💥 {} sessions crashed recently. Most recent: {bold}{session_label}{reset}",
                crashed.len()
            ),
            format!("{yellow}   Resume with:{reset}  jcode --resume {id}"),
            format!("{yellow}   List all:{reset}     jcode --resume"),
        ]
    }
}

#[cfg(test)]
mod crash_resume_hint_tests {
    use super::crash_resume_hint_lines;

    fn session(id: &str, name: &str) -> (String, String) {
        (id.to_string(), name.to_string())
    }

    #[test]
    fn no_crashed_sessions_produces_no_hint() {
        assert!(crash_resume_hint_lines(&[], "", "", "").is_empty());
    }

    /// Issue #690: a user who cannot memorize the ID must still be told how to
    /// get to the picker.
    #[test]
    fn single_session_hint_also_points_at_the_picker() {
        let lines = crash_resume_hint_lines(&[session("ses_koala_123", "koala")], "", "", "");
        let joined = lines.join("\n");

        assert!(
            joined.contains("jcode --resume ses_koala_123"),
            "the direct resume command must still be offered: {joined}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Or browse all: jcode --resume")),
            "the picker form (bare --resume) must be mentioned too: {joined}"
        );
    }

    #[test]
    fn multiple_sessions_hint_lists_recent_and_the_picker() {
        let lines = crash_resume_hint_lines(
            &[
                session("ses_koala_123", "koala"),
                session("ses_otter_456", "otter"),
            ],
            "",
            "",
            "",
        );
        let joined = lines.join("\n");

        assert!(joined.contains("2 sessions crashed"), "{joined}");
        assert!(joined.contains("jcode --resume ses_koala_123"), "{joined}");
        assert!(joined.contains("List all:"), "{joined}");
    }
}

fn init_tui_terminal(inherited_terminal: bool) -> Result<AppTerminal> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!("jcode TUI requires an interactive terminal (stdin/stdout must be a TTY)");
    }
    if inherited_terminal {
        init_tui_terminal_resume()
    } else {
        init_tui_terminal_fresh()
    }
}

/// Build an [`AppTerminal`] whose backend writes through a [`TerminalWriter`]
/// so the render loop never blocks on a wedged pty.
///
/// On unix the writer thread runs over a `dup` of fd 1 (a raw `File`, no
/// process-wide `Stdout` lock), so a wedged pty write never blocks other
/// `io::stdout()` callers on the render loop.
fn build_app_terminal() -> Result<AppTerminal> {
    #[cfg(unix)]
    let writer = jcode_tui::tui::terminal_writer::TerminalWriter::stdout()
        .map_err(|e| anyhow::anyhow!("failed to set up terminal writer: {e}"))?;
    #[cfg(not(unix))]
    let writer = jcode_tui::tui::terminal_writer::TerminalWriter::new(io::stdout());
    let backend = ratatui::backend::CrosstermBackend::new(writer);
    ratatui::Terminal::new(backend).map_err(|e| anyhow::anyhow!("failed to create terminal: {e}"))
}

/// Fresh (non-resume) init: equivalent to `ratatui::init()` but routes output
/// through the non-blocking [`TerminalWriter`].
fn init_tui_terminal_fresh() -> Result<AppTerminal> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crossterm::terminal::enable_raw_mode()
            .map_err(|e| anyhow::anyhow!("failed to enable raw mode: {}", e))?;
        crossterm::execute!(
            io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )
        .map_err(|e| anyhow::anyhow!("failed to enter alternate screen: {}", e))?;
        build_app_terminal()
    }))
    .map_err(|payload| {
        anyhow::anyhow!(
            "failed to initialize terminal: {}",
            panic_payload_to_string(payload.as_ref())
        )
    })?
}

pub fn init_tui_runtime() -> Result<(AppTerminal, TuiRuntimeGuard)> {
    let is_resuming = std::env::var_os("JCODE_RESUMING").is_some();
    let inherited_theme = std::env::var(INHERITED_THEME_ENV).ok();
    let inherited_modes_raw = std::env::var(INHERITED_MODES_ENV).ok();
    let inherited_modes = inherited_modes_raw
        .as_deref()
        .and_then(InheritedTerminalModes::decode);
    // JCODE_RESUMING describes the session lifecycle, but only a valid modes
    // handoff proves the previous process deliberately left the terminal live
    // across exec. A restart used to restore the terminal before exec while the
    // new process still took the resume path, leaving it on the primary screen
    // without mouse capture.
    let inherited_terminal = has_terminal_exec_handoff(is_resuming, inherited_modes);
    if inherited_terminal {
        // OSC terminal queries are unsafe here because the previous process
        // deliberately exec'd without leaving raw mode or the alternate screen.
        crate::tui::theme_detect::init_theme_mode_for_resume(inherited_theme.as_deref());
    } else {
        // The OSC 11 query needs the cooked terminal and must happen before init.
        crate::tui::theme_detect::init_theme_mode();
    }
    let terminal = init_tui_terminal(inherited_terminal)?;
    // From here the terminal is in raw mode + alternate screen, so the panic
    // hook's restore (below) should run if we unwind.
    TUI_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
    // `ratatui::init()` installed a panic hook that calls the panicking
    // `restore()`. Put our own hook back as the outermost so any restore runs
    // through the safe, non-panicking `restore_terminal_quietly` path instead
    // of `eprintln!`-ing on a dead terminal and double-panic aborting (see
    // #599).
    reinstall_panic_hook_outermost();
    crate::tui::mermaid::install_jcode_mermaid_hooks();
    crate::tui::markdown::install_jcode_markdown_hooks();
    crate::tui::mermaid::init_picker();

    let perf_policy = crate::perf::tui_policy();
    // These private handoff values apply only to this exec boundary. Avoid
    // leaking them into tools or unrelated child jcode processes.
    crate::env::remove_var(INHERITED_MODES_ENV);
    crate::env::remove_var(INHERITED_THEME_ENV);

    let fallback_modes = InheritedTerminalModes {
        mouse_capture: perf_policy.enable_mouse_capture,
        keyboard_enhanced: perf_policy.enable_keyboard_enhancement,
        focus_change: perf_policy.enable_focus_change,
    };
    let modes = if inherited_terminal {
        // The previous process intentionally preserved these modes across exec.
        // Reassert idempotent modes because terminals, multiplexers, or an older
        // process may have cleared them during the handoff. Do not push Kitty's
        // stack-based keyboard enhancement flags again. A later normal exit must
        // still disable every inherited mode, so retain them in the guard.
        let modes = inherited_modes.unwrap_or(fallback_modes);
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)?;
        if modes.focus_change {
            crossterm::execute!(std::io::stdout(), crossterm::event::EnableFocusChange)?;
        }
        if modes.mouse_capture {
            crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
            if let Err(err) = sync_windows_vt_mouse_capture(true) {
                crate::logging::warn(&format!(
                    "failed to enable Windows VT mouse tracking: {err}"
                ));
            }
        }
        modes
    } else {
        let keyboard_enhanced = if perf_policy.enable_keyboard_enhancement {
            tui::enable_keyboard_enhancement()
        } else {
            false
        };
        let modes = InheritedTerminalModes {
            mouse_capture: perf_policy.enable_mouse_capture,
            keyboard_enhanced,
            focus_change: perf_policy.enable_focus_change,
        };
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)?;
        if modes.focus_change {
            crossterm::execute!(std::io::stdout(), crossterm::event::EnableFocusChange)?;
        }
        if modes.mouse_capture {
            crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
            if let Err(err) = sync_windows_vt_mouse_capture(true) {
                crate::logging::warn(&format!(
                    "failed to enable Windows VT mouse tracking: {err}"
                ));
            }
        }
        modes
    };

    crate::logging::info(&format!(
        "EVENT event=TUI_TERMINAL_MODES phase=initialized pid={} resuming={} handoff={} handoff_raw={} raw_mode={} mouse_capture={} keyboard_enhanced={} focus_change={} idempotent_modes_reasserted={}",
        std::process::id(),
        is_resuming,
        inherited_terminal,
        inherited_modes_raw.as_deref().unwrap_or("none"),
        crossterm::terminal::is_raw_mode_enabled().unwrap_or(false),
        modes.mouse_capture,
        modes.keyboard_enhanced,
        modes.focus_change,
        inherited_terminal,
    ));

    Ok((
        terminal,
        TuiRuntimeGuard::new(TuiRuntimeState {
            mouse_capture: modes.mouse_capture,
            keyboard_enhanced: modes.keyboard_enhanced,
            focus_change: modes.focus_change,
        }),
    ))
}

fn cleanup_tui_runtime(state: &TuiRuntimeState, restore_terminal: bool) {
    crate::logging::info(&format!(
        "EVENT event=TUI_TERMINAL_MODES phase=cleanup pid={} restore_terminal={} raw_mode={} mouse_capture={} keyboard_enhanced={} focus_change={}",
        std::process::id(),
        restore_terminal,
        crossterm::terminal::is_raw_mode_enabled().unwrap_or(false),
        state.mouse_capture,
        state.keyboard_enhanced,
        state.focus_change,
    ));
    crate::tui::mermaid::clear_image_state();
    let image_cleanup = crate::tui::mermaid::take_terminal_image_cleanup_payload();
    if !image_cleanup.is_empty() {
        use std::io::Write as _;
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(image_cleanup.as_bytes());
        let _ = stdout.flush();
    }

    if restore_terminal {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
        if state.focus_change {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableFocusChange);
        }
        if state.mouse_capture {
            if let Err(error) = sync_windows_vt_mouse_capture(false) {
                crate::logging::warn(&format!(
                    "failed to disable Windows VT mouse capture: {error}"
                ));
            }
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
        }
        if state.keyboard_enhanced {
            tui::disable_keyboard_enhancement();
        }
        jcode_tui_style::restore_terminal_quietly();
        // The terminal is back to a fully cooked state, so a later panic (e.g. in
        // CLI code after the TUI exited, or before a re-entered TUI is re-
        // initialized) must not try to restore it again. Re-entry sets this back
        // to true when ratatui::init() runs again.
        TUI_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

fn cleanup_tui_runtime_for_run_result(
    state: &TuiRuntimeState,
    run_result: &crate::tui::RunResult,
    extra_exec: bool,
) {
    cleanup_tui_runtime(state, !run_result_will_exec(run_result, extra_exec));
}

fn run_result_will_exec(run_result: &crate::tui::RunResult, extra_exec: bool) -> bool {
    extra_exec
        || run_result.reload_session.is_some()
        || run_result.rebuild_session.is_some()
        || run_result.update_session.is_some()
        || run_result.restart_session.is_some()
}

fn export_tui_exec_handoff(state: &TuiRuntimeState) {
    let modes = InheritedTerminalModes {
        mouse_capture: state.mouse_capture,
        keyboard_enhanced: state.keyboard_enhanced,
        focus_change: state.focus_change,
    };
    crate::env::set_var(INHERITED_MODES_ENV, modes.encode());
    let theme = crate::tui::theme_detect::current_theme_label();
    crate::env::set_var(INHERITED_THEME_ENV, theme);
    crate::logging::info(&format!(
        "EVENT event=TUI_TERMINAL_MODES phase=exec_handoff pid={} raw_mode={} modes={} theme={}",
        std::process::id(),
        crossterm::terminal::is_raw_mode_enabled().unwrap_or(false),
        modes.encode(),
        theme,
    ));
}

pub fn print_session_resume_hint(session_id: &str) {
    let _ = write_session_resume_hint(io::stderr().lock(), session_id);
}

fn write_session_resume_hint(mut writer: impl Write, session_id: &str) -> io::Result<()> {
    let session_name = id::extract_session_name(session_id).unwrap_or(session_id);
    writeln!(writer)?;
    writeln!(
        writer,
        "\x1b[33mSession \x1b[1m{}\x1b[0m\x1b[33m - to resume:\x1b[0m",
        session_name
    )?;
    writeln!(writer, "  jcode --resume {}", session_id)?;
    writeln!(writer)?;
    Ok(())
}

fn init_tui_terminal_resume() -> Result<AppTerminal> {
    crossterm::terminal::enable_raw_mode()
        .map_err(|e| anyhow::anyhow!("failed to enable raw mode on resume: {}", e))?;

    let mut terminal = build_app_terminal()
        .map_err(|e| anyhow::anyhow!("failed to create terminal on resume: {}", e))?;

    terminal
        .clear()
        .map_err(|e| anyhow::anyhow!("failed to clear terminal on resume: {}", e))?;

    Ok(terminal)
}

#[cfg(unix)]
pub fn signal_name(sig: i32) -> &'static str {
    match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => "unknown",
    }
}

#[cfg(not(unix))]
pub fn signal_name(_sig: i32) -> &'static str {
    "unknown"
}

#[cfg(unix)]
fn signal_crash_reason(sig: i32) -> String {
    match sig {
        libc::SIGHUP => "Terminal or window closed (SIGHUP)".to_string(),
        libc::SIGTERM => "Terminated (SIGTERM)".to_string(),
        libc::SIGINT => "Interrupted (SIGINT)".to_string(),
        libc::SIGQUIT => "Quit signal (SIGQUIT)".to_string(),
        _ => format!("Terminated by signal {} ({})", signal_name(sig), sig),
    }
}

#[cfg(unix)]
fn handle_termination_signal(sig: i32) -> ! {
    mark_current_session_crashed(signal_crash_reason(sig));

    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stderr(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    );

    if let Some(session_id) = get_current_session() {
        print_session_resume_hint(&session_id);
    }

    std::process::exit(128 + sig);
}

#[cfg(unix)]
pub fn spawn_session_signal_watchers() {
    use tokio::signal::unix::{SignalKind, signal};

    fn spawn_one(sig: i32, kind: SignalKind) {
        tokio::spawn(async move {
            let mut stream = match signal(kind) {
                Ok(s) => s,
                Err(e) => {
                    crate::logging::error(&format!(
                        "Failed to install {} handler: {}",
                        signal_name(sig),
                        e
                    ));
                    return;
                }
            };
            if stream.recv().await.is_some() {
                crate::logging::info(&format!("Received {} in TUI process", signal_name(sig)));
                handle_termination_signal(sig);
            }
        });
    }

    spawn_one(libc::SIGHUP, SignalKind::hangup());
    spawn_one(libc::SIGTERM, SignalKind::terminate());
    spawn_one(libc::SIGINT, SignalKind::interrupt());
    spawn_one(libc::SIGQUIT, SignalKind::quit());
}

#[cfg(not(unix))]
pub fn spawn_session_signal_watchers() {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_SESSION_LOCK: Mutex<()> = Mutex::new(());

    fn test_guard() -> TuiRuntimeGuard {
        // All terminal-mode flags disabled so teardown only performs the minimal
        // (and TTY-safe) restore path during tests.
        TuiRuntimeGuard::new(TuiRuntimeState {
            mouse_capture: false,
            keyboard_enhanced: false,
            focus_change: false,
        })
    }

    #[test]
    fn inherited_terminal_modes_roundtrip() {
        let modes = InheritedTerminalModes {
            mouse_capture: true,
            keyboard_enhanced: false,
            focus_change: true,
        };
        assert_eq!(InheritedTerminalModes::decode(&modes.encode()), Some(modes));
    }

    #[test]
    fn windows_vt_mouse_modes_enable_and_disable_the_same_tracking_protocols() {
        let enable = String::from_utf8_lossy(WINDOWS_VT_MOUSE_ENABLE);
        let disable = String::from_utf8_lossy(WINDOWS_VT_MOUSE_DISABLE);
        for mode in ["1000", "1002", "1003", "1015", "1006"] {
            assert!(
                enable.contains(&format!("?{mode}h")),
                "enable sequence must turn on VT mouse mode {mode}"
            );
            assert!(
                disable.contains(&format!("?{mode}l")),
                "disable sequence must turn off VT mouse mode {mode}"
            );
        }
    }

    #[test]
    fn inherited_terminal_modes_reject_malformed_values() {
        assert_eq!(InheritedTerminalModes::decode("mouse=1,keyboard=1"), None);
        assert_eq!(
            InheritedTerminalModes::decode("mouse=yes,keyboard=1,focus=1"),
            None
        );
    }

    #[test]
    fn resume_requires_valid_terminal_handoff_metadata() {
        let modes = InheritedTerminalModes {
            mouse_capture: true,
            keyboard_enhanced: true,
            focus_change: true,
        };
        assert!(has_terminal_exec_handoff(true, Some(modes)));
        assert!(!has_terminal_exec_handoff(true, None));
        assert!(!has_terminal_exec_handoff(false, Some(modes)));
    }

    #[test]
    fn every_exec_action_preserves_terminal_modes() {
        let with = |field: &str| {
            let mut result = crate::tui::RunResult::default();
            match field {
                "reload" => result.reload_session = Some("session_test".into()),
                "rebuild" => result.rebuild_session = Some("session_test".into()),
                "update" => result.update_session = Some("session_test".into()),
                "restart" => result.restart_session = Some("session_test".into()),
                _ => unreachable!(),
            }
            result
        };

        for field in ["reload", "rebuild", "update", "restart"] {
            assert!(
                run_result_will_exec(&with(field), false),
                "{field} must preserve terminal modes across exec"
            );
        }
        assert!(run_result_will_exec(
            &crate::tui::RunResult::default(),
            true
        ));
        assert!(!run_result_will_exec(
            &crate::tui::RunResult::default(),
            false
        ));
    }

    #[test]
    fn guard_drop_restores_terminal_when_not_finished() {
        // Simulates the error/panic path where explicit teardown is skipped:
        // the guard must restore the terminal exactly once on drop (issue #214).
        GUARD_DROP_RESTORES.with(|c| c.set(0));
        {
            let _guard = test_guard();
        }
        let restores = GUARD_DROP_RESTORES.with(|c| c.get());
        assert_eq!(
            restores, 1,
            "dropping an un-finished guard must restore the terminal once"
        );
    }

    #[test]
    fn guard_finish_disarms_drop_restore() {
        // The happy path calls finish(); the drop safety net must NOT fire again.
        GUARD_DROP_RESTORES.with(|c| c.set(0));
        let guard = test_guard();
        guard.finish(true);
        let restores = GUARD_DROP_RESTORES.with(|c| c.get());
        assert_eq!(
            restores, 0,
            "finish() should disarm the guard so drop does not double-restore"
        );
    }

    #[test]
    fn cleanup_clears_tui_active_after_full_restore() {
        let _lock = GLOBAL_HOOK_TEST_LOCK.write().unwrap();
        // The panic-hook restore only runs while `TUI_ACTIVE` is set, so a full
        // teardown must clear it; otherwise a later panic in CLI code would try
        // to restore a no-longer-live terminal and emit escape sequences.
        let saved = TUI_ACTIVE.load(std::sync::atomic::Ordering::SeqCst);
        TUI_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        let state = TuiRuntimeState {
            mouse_capture: false,
            keyboard_enhanced: false,
            focus_change: false,
        };
        cleanup_tui_runtime(&state, true);
        assert!(
            !tui_is_active(),
            "a full terminal restore must clear TUI_ACTIVE"
        );
        TUI_ACTIVE.store(saved, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn cleanup_keeps_tui_active_on_exec_handoff() {
        let _lock = GLOBAL_HOOK_TEST_LOCK.write().unwrap();
        // When the terminal is handed off across exec (reload/rebuild/update),
        // the restore does not run and `TUI_ACTIVE` must stay set so the next
        // process's early panics still attempt a safe restore.
        TUI_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        let state = TuiRuntimeState {
            mouse_capture: false,
            keyboard_enhanced: false,
            focus_change: false,
        };
        cleanup_tui_runtime_for_run_result(
            &state,
            &crate::tui::RunResult {
                reload_session: Some("session_test".into()),
                ..Default::default()
            },
            false,
        );
        assert!(
            tui_is_active(),
            "an exec handoff must keep TUI_ACTIVE set for the next process"
        );
        TUI_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn test_session_recovery_tracking() {
        let _guard = TEST_SESSION_LOCK.lock().unwrap();
        set_current_session("test_session_123");

        let stored = get_current_session();
        assert_eq!(stored.as_deref(), Some("test_session_123"));
    }

    #[test]
    fn test_session_recovery_message_format() {
        let _guard = TEST_SESSION_LOCK.lock().unwrap();
        let test_session = "session_format_test_12345";
        set_current_session(test_session);

        if let Some(session_id) = get_current_session() {
            let mut output = Vec::new();
            write_session_resume_hint(&mut output, &session_id).unwrap();
            let output = String::from_utf8(output).unwrap();
            let expected_cmd = format!("jcode --resume {}", session_id);
            assert!(output.contains(&expected_cmd));
            assert!(output.contains("to resume"));
            assert!(!session_id.is_empty());
        } else {
            panic!("Session ID should be set");
        }
    }

    #[test]
    fn session_resume_hint_writer_reports_closed_stderr_without_panicking() {
        struct ClosedWriter;

        impl Write for ClosedWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "stderr closed"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let error = write_session_resume_hint(ClosedWriter, "session_closed_pipe")
            .expect_err("closed stderr should be reported as an I/O error");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}

#[cfg(test)]
mod panic_crash_labeling_tests {
    //! Regression coverage for #599.
    //!
    //! Closing a terminal (or dropping SSH) makes `ratatui::restore()`'s
    //! internal `eprintln!` panic with EIO. The panic hook then relabeled the
    //! session as `Crashed` and saved the dying client's stale snapshot over the
    //! server's newer one. Only an `Active` session may be relabeled.
    use super::*;

    #[test]
    fn active_session_is_still_labeled_crashed_on_panic() {
        assert!(should_record_panic_as_crash(
            &session::SessionStatus::Active
        ));
    }

    #[test]
    fn already_crashed_session_is_not_relabeled() {
        assert!(!should_record_panic_as_crash(
            &session::SessionStatus::Crashed {
                message: Some("earlier crash".to_string())
            }
        ));
    }

    #[test]
    fn completed_session_is_not_relabeled_by_a_dying_client() {
        // The exact #599 shape: the session lives on in the shared server and is
        // no longer Active locally, so a dead-terminal panic must leave it alone.
        for status in [
            session::SessionStatus::Closed,
            session::SessionStatus::Reloaded,
            session::SessionStatus::Compacted,
            session::SessionStatus::RateLimited,
            session::SessionStatus::Error {
                message: "unrelated".to_string(),
            },
        ] {
            assert!(
                !should_record_panic_as_crash(&status),
                "non-active status {status:?} must not be relabeled as crashed"
            );
        }
    }

    #[test]
    fn reinstall_panic_hook_outermost_keeps_default_backtrace_chain() {
        // Serialize against sibling tests that touch the shared DEFAULT_HOOK /
        // TUI_ACTIVE globals.
        let _lock = GLOBAL_HOOK_TEST_LOCK.write().unwrap();
        // Simulate ratatui::init() wrapping our hook: install our hook first,
        // then call reinstall_panic_hook_outermost() the way init_tui_runtime
        // does after ratatui::init(). A panicking closure must unwind normally
        // (no double-panic abort) and the captured default hook must run.
        //
        // Force `TUI_ACTIVE` to a known state so this test is independent of
        // ordering with sibling tests, and verify the hook does not touch it
        // when there is no live TUI (so no escape sequences reach the console).
        let saved = TUI_ACTIVE.load(std::sync::atomic::Ordering::SeqCst);
        TUI_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
        let default_hook_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = default_hook_called.clone();
        let synthetic_default = Box::new(move |_info: &panic::PanicHookInfo<'_>| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        set_default_hook(synthetic_default);
        reinstall_panic_hook_outermost();

        let result = std::panic::catch_unwind(|| panic!("test panic"));
        assert!(result.is_err(), "the panic must still propagate normally");
        assert!(
            default_hook_called.load(std::sync::atomic::Ordering::SeqCst),
            "the captured default hook must run so backtraces still print"
        );
        // With no live TUI the hook must not have flipped the terminal-active
        // flag (it would only do so after restoring an actual TUI).
        assert!(
            !tui_is_active(),
            "a non-TUI panic must not flip the TUI-active flag on"
        );

        TUI_ACTIVE.store(saved, std::sync::atomic::Ordering::SeqCst);
        // Restore a sane default hook so later tests keep their own backtrace.
        std::panic::set_hook(Box::new(|_| {}));
    }

    #[test]
    fn tui_active_flag_toggles_with_restore() {
        // The flag should not be toggled concurrently by sibling tests, which
        // would race on this shared global.
        let _lock = GLOBAL_HOOK_TEST_LOCK.write().unwrap();
        // The flag starts false, is set true on TUI init, and is cleared when
        // the terminal is restored. On restore it ends false again.
        let save = TUI_ACTIVE.load(std::sync::atomic::Ordering::SeqCst);
        TUI_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(tui_is_active());
        TUI_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(!tui_is_active());
        TUI_ACTIVE.store(save, std::sync::atomic::Ordering::SeqCst);
    }
}
