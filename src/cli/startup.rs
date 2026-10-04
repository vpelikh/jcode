use anyhow::Result;
use clap::Parser;
use std::process::Command as ProcessCommand;

use crate::{
    build, logging, perf, server, setup_hints, startup_profile, storage, telemetry, update,
};

use super::{
    args::{Args, Command},
    dispatch, hot_exec, output, terminal,
};

fn sync_output_style_from_config() {
    crate::output_style::set_emoji_enabled(crate::config::config().display.emoji);
}

pub async fn run() -> Result<()> {
    // Parse once, before startup side effects. Invalid arguments and --help
    // must not harden credential files or create configuration/telemetry state.
    let args = Args::parse();
    // Credential import must refuse existing stores without normal startup
    // hardening, migrations, telemetry, or provider discovery touching them.
    if args.ssh.is_none()
        && matches!(
            args.command,
            Some(Command::Auth(super::args::AuthCommand::Import { .. }))
        )
    {
        if let Some(cwd) = &args.cwd {
            std::env::set_current_dir(cwd)?;
        }
        return dispatch::run_main(args).await;
    }
    startup_profile::init();

    terminal::install_panic_hook();
    startup_profile::mark("panic_hook");

    logging::init();
    startup_profile::mark("logging_init");
    // Old log pruning now runs on a background thread inside logging::init(),
    // so it no longer blocks startup. Memory-event logs have a separate,
    // longer (14-day) retention, so prune them on their own background thread.
    std::thread::Builder::new()
        .name("jcode-memlog-cleanup".to_string())
        .spawn(crate::memory_log::cleanup_old_memory_logs)
        .ok();
    // Prune stale per-session `.bak` recovery copies (never the transcripts
    // themselves) so the sessions directory does not grow without bound.
    std::thread::Builder::new()
        .name("jcode-session-bak-prune".to_string())
        .spawn(crate::session::prune_old_session_backups)
        .ok();
    logging::info("jcode starting");

    // Wire config-reload reactions without making config depend on auth/bus:
    // when the config cache reloads, invalidate the auth-status cache and
    // broadcast a models-updated event.
    sync_output_style_from_config();
    crate::config::on_config_reloaded(sync_output_style_from_config);
    crate::config::on_config_reloaded(crate::auth::AuthStatus::invalidate_cache);
    crate::config::on_config_reloaded(|| crate::bus::Bus::global().publish_models_updated());

    // Invert the legacy provider_catalog -> auth dependency: provider_catalog
    // consults registered fallback resolvers, and auth (the higher layer)
    // registers its external-CLI credential scan here.
    crate::provider_catalog::register_api_key_fallback_resolver(
        crate::auth::external::load_api_key_for_env,
    );

    // Register externally-implemented provider runtimes with the base
    // provider registry. These crates sit downstream of jcode-base (so
    // provider edits do not rebuild the app spine), which means base cannot
    // name their concrete types; this composition root wires them up instead.
    register_external_provider_runtimes();

    // Invert the legacy safety -> notifications dependency: safety raises a
    // permission request and the notifications layer (which depends on safety
    // types) delivers it via the dispatcher registered here.
    crate::safety::register_permission_notifier(|action, description, request_id| {
        crate::notifications::NotificationDispatcher::new().dispatch_permission_request(
            action,
            description,
            request_id,
        );
    });

    // Invert the legacy memory -> skill dependency: memory collects synthetic
    // entries from registered providers, and skill (the higher layer that
    // depends on MemoryEntry) registers its registry->memory adapter here.
    // The shared snapshot holds global skills only; memory retrieval is
    // process-scoped, so compose the project overlay from the process cwd
    // (issue #457 keeps session overlays out of the shared registry).
    crate::memory::register_synthetic_entry_provider(|| {
        let global = crate::skill::SkillRegistry::shared_snapshot();
        crate::skill::SkillRegistry::effective_for_working_dir(&global, None)
            .list()
            .into_iter()
            .map(|skill| skill.as_memory_entry())
            .collect()
    });

    // Invert the legacy server -> tui dependency: the TUI session picker owns
    // the session-list cache and registers its invalidator here, so the server
    // can drop the cache (e.g. after a rename) without referencing tui.
    crate::session_list_cache::register_invalidator(
        crate::tui::session_picker::invalidate_session_list_cache,
    );

    // Invert the legacy tui -> cli dependency for shared-server spawning: the
    // CLI owns the provider-bootstrap spawn logic and registers it here, so the
    // TUI reconnect loop can request a replacement server via server_spawn
    // without referencing cli.
    crate::server_spawn::register_default_server_spawner(Box::new(|| {
        Box::pin(async {
            dispatch::spawn_server(&crate::cli::provider_init::ProviderChoice::Auto, None, None)
                .await
        })
    }));

    crate::tui::keybind::log_keybinding_default_warnings();
    crate::platform::raise_nofile_limit_best_effort(8_192);
    startup_profile::mark("nofile_limit");

    storage::harden_user_config_permissions();
    startup_profile::mark("perm_harden");

    perf::init_background();
    startup_profile::mark("perf_init");

    // Telemetry settings commands must run before they can cause telemetry. In
    // particular, a first-ever `jcode telemetry disable` must not emit the
    // install event that the command is trying to opt out of. Keep the normal
    // startup ordering unchanged for every other invocation.
    if !is_telemetry_subcommand_invocation(std::env::args_os()) {
        telemetry::record_install_if_first_run();
        telemetry::record_upgrade_if_needed();
    }
    startup_profile::mark("telemetry_check");

    let args = parse_and_prepare_args(args)?;
    spawn_background_update_check(&args);

    if let Err(e) = dispatch::run_main(args).await {
        report_main_error(&e);
        return Err(e);
    }

    Ok(())
}

fn is_telemetry_subcommand_invocation(
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> bool {
    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        let arg = arg.as_ref();
        if arg == std::ffi::OsStr::new("telemetry") {
            return true;
        }
        let text = arg.to_string_lossy();
        if !text.starts_with('-') {
            return false;
        }
        if text == "--" {
            return args
                .next()
                .is_some_and(|arg| arg.as_ref() == std::ffi::OsStr::new("telemetry"));
        }
        let option = text.split_once('=').map_or(text.as_ref(), |(name, _)| name);
        let takes_separate_value = !text.contains('=')
            && matches!(
                option,
                "-p" | "--provider"
                    | "-C"
                    | "--cwd"
                    | "--remote-working-dir"
                    | "--ssh"
                    | "--ssh-binary"
                    | "--ssh-server-socket"
                    | "--spawn-hotkey"
                    | "--socket"
                    | "-m"
                    | "--model"
                    | "--provider-profile"
                    | "--tool-profile"
                    | "--mcp-tools"
                    | "--mcp-tools-token-threshold"
                    | "--tools"
                    | "--disabled-tools"
            );
        if takes_separate_value && args.next().is_none() {
            return false;
        }
    }
    false
}

/// Register provider runtimes that live downstream of `jcode-base` with the
/// base crate's external provider registry. Keep every downstream runtime
/// registration in this one function so the composition-root wiring stays
/// discoverable as more providers move out of the base crate.
pub fn register_external_provider_runtimes() {
    crate::provider::external::register_external_provider(
        crate::provider::external::GROK_BUILD_RUNTIME,
        || {
            let mut process = jcode_provider_grok_build_runtime::GrokBuildProcess::from_env();
            process.command = crate::auth::grok_build::cli_path();
            std::sync::Arc::new(
                jcode_provider_grok_build_runtime::GrokBuildProvider::with_process(process),
            )
        },
    );
    crate::provider::external::register_external_provider(
        crate::provider::external::GEMINI_RUNTIME,
        || std::sync::Arc::new(jcode_provider_gemini_runtime::GeminiProvider::new()),
    );
    crate::provider::external::register_external_provider(
        crate::provider::external::CURSOR_RUNTIME,
        || std::sync::Arc::new(jcode_provider_cursor_runtime::CursorCliProvider::new()),
    );
    crate::provider::external::register_external_provider(
        crate::provider::external::ANTIGRAVITY_RUNTIME,
        || std::sync::Arc::new(jcode_provider_antigravity_runtime::AntigravityProvider::new()),
    );
    crate::provider::external::register_external_provider(
        crate::provider::external::CLAUDE_CLI_RUNTIME,
        || std::sync::Arc::new(jcode_provider_claude_cli_runtime::ClaudeProvider::new()),
    );
    crate::provider::external::register_external_provider(
        crate::provider::external::ANTHROPIC_RUNTIME,
        || std::sync::Arc::new(jcode_provider_anthropic_runtime::AnthropicProvider::new()),
    );
    // OpenRouter serves several identities (aggregator, pinned API-key
    // runtime, direct OpenAI-compatible profiles, named config profiles)
    // through one concrete type, so it registers a parameterized factory.
    crate::provider::external::register_openrouter_factory(|spec| {
        use crate::provider::external::OpenRouterRuntimeSpec;
        use jcode_provider_openrouter_runtime::OpenRouterProvider;
        let provider: std::sync::Arc<dyn crate::provider::Provider> = match spec {
            OpenRouterRuntimeSpec::Default => std::sync::Arc::new(OpenRouterProvider::new()?),
            OpenRouterRuntimeSpec::OpenRouterApiKey => {
                std::sync::Arc::new(OpenRouterProvider::new_openrouter_api_key_runtime()?)
            }
            OpenRouterRuntimeSpec::CompatibleProfile(profile) => std::sync::Arc::new(
                OpenRouterProvider::new_openai_compatible_profile_runtime(profile)?,
            ),
            OpenRouterRuntimeSpec::NamedProfile { name, config } => std::sync::Arc::new(
                OpenRouterProvider::new_named_openai_compatible(&name, &config)?,
            ),
        };
        Ok(provider)
    });
    crate::provider::external::register_profile_catalog_refresh(
        jcode_provider_openrouter_runtime::maybe_schedule_openai_compatible_profile_catalog_refresh,
    );
    crate::provider::external::register_standard_openrouter_catalog_refresh(
        jcode_provider_openrouter_runtime::maybe_schedule_standard_openrouter_catalog_refresh,
    );
    // API-backed OpenAI routes use Codex/platform credentials. The runtime is
    // still registered without them so browser-backed ChatGPT models remain
    // usable through the logged-in Firefox session.
    crate::provider::external::register_external_provider_fallible(
        crate::provider::external::OPENAI_RUNTIME,
        || {
            let provider = match crate::auth::codex::load_credentials() {
                Ok(credentials) => jcode_provider_openai_runtime::OpenAIProvider::new(credentials),
                Err(_) => jcode_provider_openai_runtime::OpenAIProvider::new_browser_only(),
            };
            Some(std::sync::Arc::new(provider) as std::sync::Arc<dyn crate::provider::Provider>)
        },
    );
    // Copilot's constructor is fallible (needs a GitHub token) and the runtime
    // wants tier detection scheduled right after construction, eagerly for
    // interactive sessions and deferred for non-interactive ones. That policy
    // lives here in the composition root so base stays provider-agnostic.
    crate::provider::external::register_external_provider_fallible(
        crate::provider::external::COPILOT_RUNTIME,
        || {
            let provider = std::sync::Arc::new(
                jcode_provider_copilot_runtime::CopilotApiProvider::new().ok()?,
            );
            let eager_tier_detection = std::env::var("JCODE_NON_INTERACTIVE").is_err();
            if eager_tier_detection && tokio::runtime::Handle::try_current().is_ok() {
                let p_clone = std::sync::Arc::clone(&provider);
                tokio::spawn(async move {
                    p_clone.detect_tier_and_set_default().await;
                });
            } else {
                provider.complete_init_without_tier_detection();
            }
            Some(provider as std::sync::Arc<dyn crate::provider::Provider>)
        },
    );
}

fn parse_and_prepare_args(args: Args) -> Result<Args> {
    startup_profile::mark("args_parse");

    if let Some(chord) = args.spawn_hotkey.as_deref() {
        setup_hints::record_launch_hotkey_use(chord);
    }

    output::set_quiet_enabled(args.quiet);

    if let Some(cwd) = &args.cwd {
        std::env::set_current_dir(cwd)?;
        logging::info(&format!("Changed working directory to: {}", cwd));
    }

    validate_remote_working_dir(args.remote_working_dir.as_deref())?;

    if args.trace {
        crate::env::set_var("JCODE_TRACE", "1");
    }

    if let Some(ref socket) = args.socket {
        server::set_socket_path(socket);
    }

    crate::cli::proctitle::set_initial_title(&args);

    Ok(args)
}

fn validate_remote_working_dir(remote_working_dir: Option<&str>) -> Result<()> {
    if let Some(remote_working_dir) = remote_working_dir
        && !remote_working_dir_is_absolute(remote_working_dir)
    {
        anyhow::bail!("--remote-working-dir must be an absolute path");
    }
    Ok(())
}

fn remote_working_dir_is_absolute(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with('\\') {
        return true;
    }

    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
        && bytes[0].is_ascii_alphabetic()
}

fn spawn_background_update_check(args: &Args) {
    let check_updates = should_spawn_background_update_check(args);
    let auto_update = should_auto_install_update(args);

    if !check_updates {
        return;
    }

    if update::is_release_build() {
        std::thread::spawn(move || {
            use crate::bus::{Bus, BusEvent, ClientMaintenanceAction, SessionUpdateStatus};
            match update::check_and_maybe_update(auto_update) {
                update::UpdateCheckResult::UpdateAvailable {
                    current, latest, ..
                } => {
                    logging::info(&format!("Update available: {} -> {}", current, latest));
                }
                update::UpdateCheckResult::UpdateInstalled { version, path } => {
                    // When an interactive TUI session is running, hand the switch
                    // to the app's graceful reload path (saves the input line,
                    // waits for the current turn, resumes the session) instead of
                    // exec-ing over the live UI, which visibly resets the screen.
                    if let Some(session_id) = terminal::get_current_session() {
                        logging::info(&format!(
                            "Updated to {}. Requesting graceful session reload...",
                            version
                        ));
                        Bus::global().publish(BusEvent::SessionUpdateStatus(
                            SessionUpdateStatus::ReadyToReload {
                                session_id,
                                action: ClientMaintenanceAction::Update,
                                version,
                            },
                        ));
                        return;
                    }
                    logging::info(&format!("Updated to {}. Restarting...", version));
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    let args: Vec<String> = std::env::args().skip(1).collect();
                    let exec_path = build::client_update_candidate(false)
                        .map(|(p, _)| p)
                        .unwrap_or(path);
                    let err = crate::platform::replace_process(
                        ProcessCommand::new(&exec_path)
                            .args(&args)
                            .arg("--no-update"),
                    );
                    output::tolerant_write(
                        &mut std::io::stderr(),
                        &format!("Failed to exec new binary: {}\n", err),
                    );
                }
                update::UpdateCheckResult::Error(e) => {
                    logging::info(&format!("Update check failed: {}", e));
                }
                update::UpdateCheckResult::NoUpdate => {}
            }
        });
    } else {
        std::thread::spawn(move || {
            use crate::bus::{Bus, BusEvent, UpdateStatus};

            let start = std::time::Instant::now();
            Bus::global().publish(BusEvent::UpdateStatus(UpdateStatus::Checking));
            let status = source_update_check_status(hot_exec::check_for_updates());
            if matches!(status, UpdateStatus::Available { .. }) {
                let action = source_update_action(
                    hot_exec::can_auto_update_source(),
                    hot_exec::local_commits_ahead_of_upstream(),
                );
                // The user-visible surface for the decision; computed by the
                // same function the UI-facing test pins.
                let published = source_update_publish_status(action, &status);
                match action {
                    // Detached HEAD, or a checkout whose only baseline is an
                    // unrelated ref (for example the remote default branch on a
                    // branch with no counterpart), cannot be fast-forwarded;
                    // report the update and let the user pull manually. This
                    // must be checked before divergence: an auto-updatable
                    // branch is the only kind whose divergence is meaningful,
                    // and a comparison-only baseline would otherwise swallow
                    // the report as a false "diverged".
                    SourceUpdateAction::ManualPull => {
                        // The comparison baseline is only useful for reporting:
                        // `/update` cannot fast-forward this checkout, so do not
                        // publish `Available` (it offers an install that would
                        // fail). Report it as a skipped check and stay quiet.
                        logging::info(
                            "Source update check skipped: the checkout cannot be fast-forwarded \
                             automatically; pull manually to update.",
                        );
                        Bus::global().publish(BusEvent::UpdateStatus(published));
                    }
                    // A checkout with local commits can never fast-forward, so
                    // the pull below would always fail and surface a noisy
                    // "Update diverged. Press Ctrl+Y..." card in every new
                    // session. Developers with local work expect divergence; log
                    // it once and stay quiet in the UI (no Available/Error
                    // cards).
                    SourceUpdateAction::Diverged => {
                        logging::info(
                            "Auto-update skipped: local commits are ahead of upstream (diverged). \
                             Merge or rebase manually when ready.",
                        );
                        Bus::global().publish(BusEvent::UpdateStatus(published));
                    }
                    SourceUpdateAction::Update => {
                        Bus::global().publish(BusEvent::UpdateStatus(published));
                        if auto_update {
                            logging::info("Update available - auto-updating...");
                            Bus::global().publish(BusEvent::UpdateStatus(
                                UpdateStatus::Installing {
                                    version: "latest source".to_string(),
                                },
                            ));
                            if let Err(e) = hot_exec::run_auto_update() {
                                Bus::global().publish(BusEvent::UpdateStatus(UpdateStatus::Error(
                                    e.to_string(),
                                )));
                                logging::error(&format!(
                                    "Auto-update failed: {}. Continuing with current version.",
                                    e
                                ));
                            }
                        } else {
                            logging::info(
                                "Update available! Run `jcode update` or `/reload` to update.",
                            );
                        }
                    }
                }
            } else {
                match &status {
                    UpdateStatus::Error(message) => logging::info(message),
                    UpdateStatus::Skipped { reason } => {
                        logging::info(&format!("Source update check skipped: {reason}"));
                    }
                    _ => {}
                }
                Bus::global().publish(BusEvent::UpdateStatus(status));
            }
            logging::info(&format!(
                "[TIMING] background_update_check: auto_update={}, total={}ms",
                auto_update,
                start.elapsed().as_millis()
            ));
        });
    }
}

/// What to do when a source update is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceUpdateAction {
    /// A fast-forward target exists and the branch is not diverged: report the
    /// update (and auto-install it when requested).
    Update,
    /// The checkout cannot be fast-forwarded (detached HEAD, or a
    /// comparison-only baseline such as the remote default branch on a branch
    /// with no counterpart). Report it as a skipped check so the user can pull
    /// manually, rather than as an installable update.
    ManualPull,
    /// The checkout can be fast-forwarded but has local commits ahead of its
    /// upstream, so any pull would fail. Stay quiet in the UI.
    Diverged,
}

/// Decide how to handle an available source update.
///
/// `can_auto_update` and `local_commits_ahead` come from
/// [`hot_exec::can_auto_update_source`] and
/// [`hot_exec::local_commits_ahead_of_upstream`] respectively.
///
/// The manual-pull case is checked first: a checkout with no fast-forward
/// target has no meaningful relationship to its comparison baseline, so
/// treating its local commits as "diverged" would silently swallow a real
/// update. Divergence only suppresses the UI when a fast-forward was actually
/// possible.
fn source_update_action(
    can_auto_update: bool,
    local_commits_ahead: Option<bool>,
) -> SourceUpdateAction {
    if !can_auto_update {
        SourceUpdateAction::ManualPull
    } else if local_commits_ahead == Some(true) {
        SourceUpdateAction::Diverged
    } else {
        SourceUpdateAction::Update
    }
}

/// The status published to the UI for a decided update [`SourceUpdateAction`].
///
/// This is the single production mapping from the internal decision to the
/// user-visible surface, so the UI-facing contract can be pinned by a test:
///
/// - `Update` publishes the original `Available` (or install when auto-update).
/// - `Diverged` publishes `UpToDate`: the pull would fail, so stay quiet.
/// - `ManualPull` publishes `Skipped`: `/update` cannot fast-forward a
///   comparison-only baseline, so offering `Available` would promise an install
///   that is guaranteed to fail. The UI renders `Skipped` as quietly as
///   `UpToDate`.
fn source_update_publish_status(
    action: SourceUpdateAction,
    available: &crate::bus::UpdateStatus,
) -> crate::bus::UpdateStatus {
    use crate::bus::UpdateStatus;

    match action {
        SourceUpdateAction::Update => available.clone(),
        SourceUpdateAction::Diverged => UpdateStatus::UpToDate,
        SourceUpdateAction::ManualPull => UpdateStatus::skipped_manual_pull_source_update(),
    }
}

fn source_update_check_status(result: Option<bool>) -> crate::bus::UpdateStatus {
    use crate::bus::UpdateStatus;

    match result {
        Some(true) => UpdateStatus::Available {
            current: jcode_build_meta::version().to_string(),
            latest: "latest source".to_string(),
        },
        Some(false) => UpdateStatus::UpToDate,
        None => UpdateStatus::Error(
            "Source update check failed: unable to compare the source checkout with its upstream. \
             The repository or upstream may be unavailable, or git fetch may have failed."
                .to_string(),
        ),
    }
}

fn should_spawn_background_update_check(args: &Args) -> bool {
    should_spawn_background_update_check_with_config(
        args,
        crate::config::config().features.check_updates,
    )
}

fn should_spawn_background_update_check_with_config(args: &Args, check_updates: bool) -> bool {
    check_updates
        && args.ssh.is_none()
        && !args.quiet
        && !args.no_update
        && !matches!(
            args.command,
            Some(Command::Update)
                | Some(Command::Serve { .. })
                | Some(Command::Server { .. })
                | Some(Command::Acp)
        )
        && args.resume.is_none()
}

fn should_auto_install_update(args: &Args) -> bool {
    args.auto_update
}

fn report_main_error(error: &anyhow::Error) {
    let error_str = format!("{:?}", error);
    logging::error(&error_str);

    if let Some(session_id) = terminal::get_current_session() {
        output::stderr_blank_line();
        output::stderr_info("\x1b[33mTo restore this session, run:\x1b[0m");
        output::stderr_info(format!("  jcode --resume {}", session_id));
        output::stderr_blank_line();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{Args, Command};
    use clap::Parser;

    fn parse_args(argv: &[&str]) -> Args {
        Args::parse_from(argv)
    }

    #[test]
    fn source_update_check_unknown_reports_comparison_error() {
        let crate::bus::UpdateStatus::Error(message) = source_update_check_status(None) else {
            panic!("an indeterminate source comparison must report an error");
        };
        assert!(message.contains("unable to compare the source checkout with its upstream"));
        assert!(message.contains("git fetch may have failed"));
    }

    /// A comparison-only baseline (no fast-forward target) reports for a manual
    /// pull even when the checkout has local commits; divergence must not
    /// swallow the report.
    #[test]
    fn comparison_only_baseline_reports_manual_pull_even_when_ahead() {
        assert_eq!(
            source_update_action(false, Some(true)),
            SourceUpdateAction::ManualPull
        );
        assert_eq!(
            source_update_action(false, Some(false)),
            SourceUpdateAction::ManualPull
        );
        assert_eq!(
            source_update_action(false, None),
            SourceUpdateAction::ManualPull
        );
    }

    /// A comparison-only baseline must be reported as a skipped check, never as
    /// an installable `Available` (which `/update` cannot actually apply).
    #[test]
    fn manual_pull_reports_skipped_not_available() {
        let available = source_update_check_status(Some(true));
        assert!(matches!(available, crate::bus::UpdateStatus::Available { .. }));

        let status = source_update_publish_status(SourceUpdateAction::ManualPull, &available);
        let crate::bus::UpdateStatus::Skipped { reason } = &status else {
            panic!("comparison-only baseline must not offer an install: {status:?}");
        };
        assert_eq!(
            reason.as_str(),
            match crate::bus::UpdateStatus::skipped_manual_pull_source_update() {
                crate::bus::UpdateStatus::Skipped { reason } => reason,
                _ => unreachable!(),
            }
            .as_str()
        );
        assert!(
            !matches!(status, crate::bus::UpdateStatus::Available { .. }),
            "a checkout that cannot fast-forward must not be reported as available"
        );

        // The other decisions keep their expected surfaces.
        assert!(matches!(
            source_update_publish_status(SourceUpdateAction::Diverged, &available),
            crate::bus::UpdateStatus::UpToDate
        ));
        assert!(matches!(
            source_update_publish_status(SourceUpdateAction::Update, &available),
            crate::bus::UpdateStatus::Available { .. }
        ));
    }

    #[test]
    fn auto_updatable_checkout_with_local_commits_is_diverged() {
        assert_eq!(
            source_update_action(true, Some(true)),
            SourceUpdateAction::Diverged
        );
    }

    #[test]
    fn auto_updatable_checkout_without_local_commits_updates() {
        assert_eq!(
            source_update_action(true, Some(false)),
            SourceUpdateAction::Update
        );
        // An indeterminate ahead-count must not block a fast-forwardable update.
        assert_eq!(source_update_action(true, None), SourceUpdateAction::Update);
    }

    /// Regression: a worktree-style checkout whose only baseline is the remote
    /// default branch (no counterpart) with local commits must report for a
    /// manual pull, not be swallowed as "diverged".
    #[test]
    fn worktree_branch_with_local_commits_reports_manual_pull() {
        let root = tempfile::tempdir().expect("temporary source checkout");
        let remote = root.path().join("origin.git");
        let work = root.path().join("work");
        let git_at = |dir: &std::path::Path, args: &[&str]| {
            let output = ProcessCommand::new("git")
                .args([
                    "-c",
                    "user.name=Update Test",
                    "-c",
                    "user.email=update-test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .args(args)
                .current_dir(dir)
                .output()
                .expect("run git fixture command");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        std::fs::create_dir_all(&remote).unwrap();
        git_at(&remote, &["init", "--bare", "-b", "master"]);
        std::fs::create_dir_all(&work).unwrap();
        git_at(&work, &["init", "-b", "master"]);
        git_at(&work, &["commit", "--allow-empty", "-m", "initial"]);
        git_at(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git_at(&work, &["push", "-q", "-u", "origin", "master"]);
        git_at(&work, &["fetch", "-q", "origin"]);
        // Branch with no counterpart: its baseline is origin/master only.
        git_at(&work, &["checkout", "-q", "-b", "feature"]);
        git_at(&work, &["commit", "--allow-empty", "-m", "local work"]);

        assert!(!crate::cli::hot_exec::can_auto_update_source_at(&work));
        assert_eq!(
            crate::cli::hot_exec::local_commits_ahead_of(&work),
            Some(true)
        );
        assert_eq!(
            source_update_action(
                crate::cli::hot_exec::can_auto_update_source_at(&work),
                crate::cli::hot_exec::local_commits_ahead_of(&work),
            ),
            SourceUpdateAction::ManualPull
        );
        // The reported surface must be a quiet skip, not an `Available` install
        // that `/update` cannot apply to this comparison-only baseline.
        // Advance the unrelated baseline so the check genuinely detects an
        // update it cannot fast-forward (the reviewer's scenario).
        let other = root.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        git_at(
            root.path(),
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        git_at(&other, &["commit", "-q", "--allow-empty", "-m", "advance"]);
        git_at(&other, &["push", "-q", "origin", "master"]);
        git_at(&work, &["fetch", "-q", "origin"]);
        let available =
            source_update_check_status(crate::cli::hot_exec::source_update_available(&work));
        assert!(
            matches!(available, crate::bus::UpdateStatus::Available { .. }),
            "the comparison detects that the unrelated baseline advanced"
        );
        assert!(!crate::cli::hot_exec::can_auto_update_source_at(&work));
        // The production decision for this real checkout yields ManualPull, whose
        // published surface is a quiet skip rather than an installable Available.
        let action = source_update_action(
            crate::cli::hot_exec::can_auto_update_source_at(&work),
            crate::cli::hot_exec::local_commits_ahead_of(&work),
        );
        assert!(matches!(
            source_update_publish_status(action, &available),
            crate::bus::UpdateStatus::Skipped { .. }
        ));
    }

    #[test]
    fn source_update_check_false_reports_up_to_date() {
        assert!(matches!(
            source_update_check_status(Some(false)),
            crate::bus::UpdateStatus::UpToDate
        ));
    }

    #[test]
    fn source_update_check_true_reports_available() {
        let crate::bus::UpdateStatus::Available { current, latest } =
            source_update_check_status(Some(true))
        else {
            panic!("a source update must remain available");
        };
        assert_eq!(current, jcode_build_meta::version());
        assert_eq!(latest, "latest source");
    }

    #[test]
    fn source_update_check_real_git_upstream_states() {
        let repo = tempfile::tempdir().expect("temporary source checkout");
        let git = |args: &[&str]| {
            let output = ProcessCommand::new("git")
                .args([
                    "-c",
                    "user.name=Update Test",
                    "-c",
                    "user.email=update-test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .args(args)
                .current_dir(repo.path())
                .output()
                .expect("run git fixture command");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "source"]);
        git(&["commit", "--allow-empty", "-m", "initial"]);

        // A valid checkout with neither a tracking branch nor an
        // `origin/HEAD` remote cannot be compared.
        let result = hot_exec::source_update_available(repo.path());
        assert_eq!(result, None);
        assert!(matches!(
            source_update_check_status(result),
            crate::bus::UpdateStatus::Error(_)
        ));

        // Local branches supply tracking controls without fetching or networking.
        git(&["branch", "upstream"]);
        git(&["branch", "--set-upstream-to=upstream", "source"]);
        let result = hot_exec::source_update_available(repo.path());
        assert_eq!(result, Some(false));
        assert!(matches!(
            source_update_check_status(result),
            crate::bus::UpdateStatus::UpToDate
        ));

        git(&["checkout", "upstream"]);
        git(&["commit", "--allow-empty", "-m", "upstream update"]);
        git(&["checkout", "source"]);
        let result = hot_exec::source_update_available(repo.path());
        assert_eq!(result, Some(true));
        assert!(matches!(
            source_update_check_status(result),
            crate::bus::UpdateStatus::Available { .. }
        ));
    }

    #[test]
    fn source_update_falls_back_to_origin_head_for_untracked_branch() {
        let root = tempfile::tempdir().expect("temporary source checkout");
        let remote = root.path().join("origin.git");
        let repo = root.path().join("work");
        std::fs::create_dir_all(&repo).unwrap();
        let git_at = |dir: &std::path::Path, args: &[&str]| {
            let output = ProcessCommand::new("git")
                .args([
                    "-c",
                    "user.name=Update Test",
                    "-c",
                    "user.email=update-test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .args(args)
                .current_dir(dir)
                .output()
                .expect("run git fixture command");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        std::fs::create_dir_all(&remote).unwrap();
        git_at(&remote, &["init", "-q", "--bare", "-b", "master"]);
        git_at(&repo, &["init", "-q", "-b", "master"]);
        git_at(&repo, &["commit", "-q", "--allow-empty", "-m", "initial"]);
        git_at(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git_at(&repo, &["push", "-q", "-u", "origin", "master"]);
        // An untracked branch sharing the remote-tracking refs: the exact shape
        // of a worktree checkout.
        git_at(&repo, &["fetch", "-q", "origin"]);
        git_at(&repo, &["checkout", "-q", "-b", "feature"]);

        // No configured upstream, but origin/HEAD makes the comparison work.
        let result = hot_exec::source_update_available(&repo);
        assert_eq!(result, Some(false));
        assert!(matches!(
            source_update_check_status(result),
            crate::bus::UpdateStatus::UpToDate
        ));

        // An advance on the remote's default branch makes the untracked branch
        // behind, and reporting it as available must not require a tracking
        // branch.
        let other = root.path().join("other");
        git_at(
            root.path(),
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        git_at(&other, &["commit", "-q", "--allow-empty", "-m", "advance"]);
        git_at(&other, &["push", "-q", "origin", "master"]);
        git_at(&repo, &["fetch", "-q", "origin"]);

        let result = hot_exec::source_update_available(&repo);
        assert_eq!(result, Some(true));
        assert!(matches!(
            source_update_check_status(result),
            crate::bus::UpdateStatus::Available { .. }
        ));
    }

    #[test]
    fn telemetry_subcommand_skips_startup_telemetry() {
        assert!(is_telemetry_subcommand_invocation([
            "jcode",
            "telemetry",
            "disable"
        ]));
        assert!(is_telemetry_subcommand_invocation([
            "jcode",
            "--no-update",
            "telemetry",
            "disable"
        ]));
        assert!(is_telemetry_subcommand_invocation([
            "jcode",
            "--provider",
            "openai",
            "telemetry",
            "disable"
        ]));
    }

    #[test]
    fn telemetry_prompt_does_not_skip_normal_startup_telemetry() {
        assert!(!is_telemetry_subcommand_invocation([
            "jcode",
            "run",
            "telemetry"
        ]));
    }

    #[test]
    fn parses_mcp_tool_exposure_flags() {
        let args = parse_args(&[
            "jcode",
            "--mcp-tools",
            "deferred",
            "--mcp-tools-token-threshold",
            "4321",
            "run",
            "hello",
        ]);
        assert_eq!(args.mcp_tools.as_deref(), Some("deferred"));
        assert_eq!(args.mcp_tools_token_threshold, Some(4_321));
    }

    #[test]
    fn auto_install_allowed_without_live_terminal() {
        let args = parse_args(&["jcode", "login"]);
        assert!(should_auto_install_update(&args));
    }

    #[test]
    fn auto_install_allowed_with_live_terminal_attached() {
        let args = parse_args(&["jcode", "login"]);
        assert!(should_auto_install_update(&args));
    }

    #[test]
    fn auto_install_respects_explicit_disable_even_without_terminal() {
        let mut args = parse_args(&["jcode", "login"]);
        args.auto_update = false;
        assert!(!should_auto_install_update(&args));
    }

    #[test]
    fn remote_working_dir_validation_requires_absolute_path() {
        assert!(validate_remote_working_dir(Some("/home/agent/project")).is_ok());
        assert!(validate_remote_working_dir(Some("C:\\Users\\agent\\project")).is_ok());
        assert!(validate_remote_working_dir(None).is_ok());

        let error = validate_remote_working_dir(Some("relative/project")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("--remote-working-dir must be an absolute path")
        );
    }

    #[test]
    fn update_command_still_skips_background_check_before_auto_install_logic() {
        let args = parse_args(&["jcode", "update"]);
        assert!(matches!(args.command, Some(Command::Update)));
        assert!(!should_spawn_background_update_check(&args));
        assert!(should_auto_install_update(&args));
    }

    #[test]
    fn config_can_permanently_disable_background_update_checks() {
        let args = parse_args(&["jcode", "login"]);
        assert!(should_spawn_background_update_check_with_config(
            &args, true
        ));
        assert!(!should_spawn_background_update_check_with_config(
            &args, false
        ));
    }

    #[test]
    fn hidden_spawn_hotkey_argument_is_global_and_preserves_canonical_text() {
        let args = parse_args(&["jcode", "--spawn-hotkey", "shift+cmd+'", "self-dev"]);
        assert_eq!(args.spawn_hotkey.as_deref(), Some("shift+cmd+'"));
        assert!(matches!(args.command, Some(Command::SelfDev { .. })));
    }
    #[test]
    fn external_provider_runtimes_register_and_instantiate() {
        register_external_provider_runtimes();
        for (key, expected_name) in [
            (crate::provider::external::GEMINI_RUNTIME, "gemini"),
            (crate::provider::external::CURSOR_RUNTIME, "cursor"),
            (
                crate::provider::external::ANTIGRAVITY_RUNTIME,
                "antigravity",
            ),
        ] {
            assert!(
                crate::provider::external::external_provider_registered(key),
                "{key} runtime should be registered"
            );
            let provider = crate::provider::external::instantiate_external_provider(key)
                .unwrap_or_else(|| panic!("{key} runtime factory should instantiate"));
            assert_eq!(provider.name(), expected_name);
            assert!(!provider.model().is_empty());
        }

        // Copilot's factory is fallible (requires a GitHub token), so only
        // assert registration; instantiation legitimately returns None when no
        // Copilot credentials exist on the machine running the tests.
        assert!(crate::provider::external::external_provider_registered(
            crate::provider::external::COPILOT_RUNTIME
        ));
    }
}
