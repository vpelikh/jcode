use super::ensure_swarm_prompt_edit_path;
use super::parse_diff_mode_name;
use super::parse_manual_subagent_spec;
use super::{create_git_worktree, main_repo_root_for_work_dir, parse_worktree_spec};

#[test]
fn parse_diff_mode_name_maps_known_aliases() {
    use crate::config::DiffDisplayMode;
    assert_eq!(parse_diff_mode_name("off"), Some(DiffDisplayMode::Off));
    assert_eq!(parse_diff_mode_name("none"), Some(DiffDisplayMode::Off));
    assert_eq!(
        parse_diff_mode_name("inline"),
        Some(DiffDisplayMode::Inline)
    );
    assert_eq!(parse_diff_mode_name("on"), Some(DiffDisplayMode::Inline));
    assert_eq!(
        parse_diff_mode_name("full"),
        Some(DiffDisplayMode::FullInline)
    );
    assert_eq!(
        parse_diff_mode_name("pinned"),
        Some(DiffDisplayMode::Pinned)
    );
    assert_eq!(parse_diff_mode_name("file"), Some(DiffDisplayMode::File));
}

#[test]
fn parse_diff_mode_name_is_case_insensitive_and_trims() {
    use crate::config::DiffDisplayMode;
    assert_eq!(
        parse_diff_mode_name("  PINNED "),
        Some(DiffDisplayMode::Pinned)
    );
}

#[test]
fn parse_diff_mode_name_rejects_unknown() {
    assert_eq!(parse_diff_mode_name("sidebyside"), None);
    assert_eq!(parse_diff_mode_name(""), None);
}

#[test]
fn parse_manual_subagent_spec_accepts_flags_and_prompt() {
    let spec = parse_manual_subagent_spec(
        "--type research --model gpt-5.4 --continue session_123 investigate this bug",
    )
    .expect("parse manual subagent spec");

    assert_eq!(spec.subagent_type, "research");
    assert_eq!(spec.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(spec.session_id.as_deref(), Some("session_123"));
    assert_eq!(spec.prompt, "investigate this bug");
}

#[test]
fn parse_manual_subagent_spec_rejects_missing_prompt() {
    let err = parse_manual_subagent_spec("--model gpt-5.4")
        .expect_err("missing prompt should be rejected");
    assert!(err.contains("Missing prompt"));
}

#[test]
fn swarm_prompt_edit_path_prefers_nonblank_project_override() {
    let project = tempfile::tempdir().expect("project tempdir");
    let jcode_home = tempfile::tempdir().expect("jcode tempdir");
    let project_prompt = project.path().join(".jcode/swarm-prompt.md");
    std::fs::create_dir_all(project_prompt.parent().expect("prompt parent"))
        .expect("create project config dir");
    std::fs::write(&project_prompt, "project routing").expect("write project prompt");
    std::fs::write(jcode_home.path().join("swarm-prompt.md"), "global routing")
        .expect("write global prompt");

    let path = ensure_swarm_prompt_edit_path(project.path().to_str(), jcode_home.path())
        .expect("resolve prompt path");
    assert_eq!(path, project_prompt);
}

#[test]
fn swarm_prompt_edit_path_falls_back_to_nonblank_global_override() {
    let project = tempfile::tempdir().expect("project tempdir");
    let jcode_home = tempfile::tempdir().expect("jcode tempdir");
    let project_prompt = project.path().join(".jcode/swarm-prompt.md");
    std::fs::create_dir_all(project_prompt.parent().expect("prompt parent"))
        .expect("create project config dir");
    std::fs::write(&project_prompt, "  \n").expect("write blank project prompt");
    let global_prompt = jcode_home.path().join("swarm-prompt.md");
    std::fs::write(&global_prompt, "global routing").expect("write global prompt");

    let path = ensure_swarm_prompt_edit_path(project.path().to_str(), jcode_home.path())
        .expect("resolve prompt path");
    assert_eq!(path, global_prompt);
}

#[test]
fn swarm_prompt_edit_path_materializes_builtin_default_globally() {
    let project = tempfile::tempdir().expect("project tempdir");
    let jcode_home = tempfile::tempdir().expect("jcode tempdir");

    let path = ensure_swarm_prompt_edit_path(project.path().to_str(), jcode_home.path())
        .expect("create editable prompt");
    assert_eq!(path, jcode_home.path().join("swarm-prompt.md"));
    let content = std::fs::read_to_string(path).expect("read created prompt");
    assert_eq!(content.trim(), crate::prompt::DEFAULT_SWARM_PROMPT.trim());
}

#[test]
fn openrouter_402_payment_required_is_non_retryable() {
    use super::is_non_retryable_auto_poke_error;
    let err = "OpenAI-compatible chat request failed\n  endpoint: \
        https://openrouter.ai/api/v1/chat/completions\n  model: openai/gpt-5.4\n  \
        auth: OPENROUTER_API_KEY\n  status: 402 Payment Required\n  response: \
        {\"error\":{\"message\":\"This request requires more credits, or fewer max_tokens. \
        You requested up to 65536 tokens, but can only afford 34424. To increase, visit \
        https://openrouter.ai/settings/credits and add more credits\",\"code\":402}}";
    assert!(is_non_retryable_auto_poke_error(err));
}

#[test]
fn transient_server_error_remains_retryable_for_auto_poke() {
    use super::is_non_retryable_auto_poke_error;
    let err = "OpenAI-compatible chat request failed\n  status: 503 Service Unavailable";
    assert!(!is_non_retryable_auto_poke_error(err));
}

#[test]
fn openai_usage_limit_reached_is_non_retryable() {
    use super::is_non_retryable_auto_poke_error;
    assert!(is_non_retryable_auto_poke_error(
        "usage_limit_reached: The usage limit has been reached"
    ));
    assert!(is_non_retryable_auto_poke_error(
        "Rate limited: The usage limit has been reached. Plan: team. \
         Resets in 30d 4h 29m (2026-08-21 04:31 UTC)."
    ));
}

#[test]
fn volcengine_ark_unsupported_model_is_fatal_model_endpoint_error() {
    use super::{is_fatal_model_endpoint_error, is_non_retryable_auto_poke_error};
    let err = "OpenAI-compatible chat request failed\n  endpoint: \
        https://ark.cn-beijing.volces.com/api/coding/v3/chat/completions\n  model: \
        volcengine:ark-code-latest\n  auth: ARK_API_KEY\n  status: 404 Not Found\n  response: \
        {\"error\":{\"code\":\"UnsupportedModel\",\"message\":\"The requested model does not \
        support the coding plan feature.\"}}";
    // It is both a fatal model/endpoint error (fail fast, no retries) and a
    // non-retryable auto-poke error (don't keep poking).
    assert!(is_fatal_model_endpoint_error(err));
    assert!(is_non_retryable_auto_poke_error(err));
}

#[test]
fn transient_5xx_is_not_a_fatal_model_endpoint_error() {
    use super::is_fatal_model_endpoint_error;
    let err = "OpenAI-compatible chat request failed\n  status: 503 Service Unavailable";
    assert!(!is_fatal_model_endpoint_error(err));
}

#[test]
fn model_not_found_is_fatal_model_endpoint_error() {
    use super::is_fatal_model_endpoint_error;
    let err = "chat request failed: 404 model_not_found: The model `gpt-foo` does not exist";
    assert!(is_fatal_model_endpoint_error(err));
}

/// Behavioral tests for `/colors`, driven through the real `App` so they cover
/// what a user actually types: dispatch, message text, config persistence, and
/// error handling.
///
/// These use an isolated `JCODE_HOME` (`create_test_app` sets one) so they write
/// to a throwaway config rather than the developer's real one.
mod colors {
    use crate::tui::app::commands_dispatch::dispatch_local_command;
    use crate::tui::app::tests::create_test_app;

    /// These tests write the config file and mutate the process-global palette,
    /// so they must serialize against *every* test touching shared config or
    /// env state, not merely against each other. A module-private lock would
    /// still let them swap the config out from under another module's test
    /// mid-assertion, which is the race class that makes unrelated provider and
    /// header tests fail intermittently.
    fn lock_shared_state() -> std::sync::MutexGuard<'static, ()> {
        crate::storage::lock_test_env()
    }

    /// Take the shared lock and leave the config and palette clean afterwards,
    /// even if the test body panics.
    fn with_clean_config(body: impl FnOnce()) {
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                let mut config = crate::config::Config::load();
                config.display.colors.clear();
                let _ = config.save();
                jcode_tui_style::set_palette(jcode_tui_style::Palette::default());
            }
        }
        let _lock = lock_shared_state();
        let _restore = Restore;
        {
            let mut config = crate::config::Config::load();
            config.display.colors.clear();
            let _ = config.save();
        }
        body();
    }

    /// Text of the last message the app pushed, whatever its role.
    fn last_message(app: &crate::tui::app::App) -> String {
        app.display_messages
            .last()
            .map(|message| message.content.clone())
            .unwrap_or_default()
    }

    #[test]
    fn colors_lists_every_role_with_its_hex_value() {
        let mut app = create_test_app();
        assert!(
            dispatch_local_command(&mut app, "/colors"),
            "/colors should be claimed by the shared dispatch table"
        );
        let output = last_message(&app);
        for role in jcode_tui_style::ALL_ROLES.iter().copied() {
            assert!(
                output.contains(role.key()),
                "listing should mention {}: {output}",
                role.key()
            );
        }
        assert!(
            output.contains("#8ab4f8"),
            "listing should show hex values: {output}"
        );
    }

    #[test]
    fn colors_harmony_reports_a_score_and_named_criteria() {
        let mut app = create_test_app();
        assert!(dispatch_local_command(&mut app, "/colors harmony"));
        let output = last_message(&app);
        assert!(
            output.contains("/100"),
            "harmony should report a score: {output}"
        );
        for criterion in [
            "readability",
            "distinctness",
            "hue harmony",
            "chroma coherence",
            "colorblind safety",
        ] {
            assert!(
                output.contains(criterion),
                "harmony should report {criterion}: {output}"
            );
        }
    }

    #[test]
    fn setting_a_role_persists_it_and_reports_the_new_score() {
        with_clean_config(|| {
            let mut app = create_test_app();
            assert!(dispatch_local_command(&mut app, "/colors error #1050f0"));
            let output = last_message(&app);
            assert!(
                output.contains("#1050f0"),
                "should confirm the new value: {output}"
            );
            assert!(
                output.contains("/100"),
                "should report the resulting harmony: {output}"
            );

            let saved = crate::config::Config::load();
            assert_eq!(
                saved.display.colors.get("error").map(String::as_str),
                Some("#1050f0"),
                "the role should be saved to config"
            );

            // And the live palette must reflect it without a restart.
            assert!(
                jcode_tui_style::palette().is_overridden(jcode_tui_style::Role::Error),
                "the running palette should pick the change up immediately"
            );

            assert!(dispatch_local_command(&mut app, "/colors reset"));
            assert!(
                crate::config::Config::load().display.colors.is_empty(),
                "reset should clear the configured colors"
            );
        });
    }

    #[test]
    fn generate_writes_a_complete_palette_and_scores_it() {
        with_clean_config(|| {
            let mut app = create_test_app();
            assert!(dispatch_local_command(&mut app, "/colors generate #8ab4f8"));
            let output = last_message(&app);
            assert!(
                output.contains("/100"),
                "generate should report the harmony score: {output}"
            );

            let saved = crate::config::Config::load();
            assert_eq!(
                saved.display.colors.len(),
                jcode_tui_style::ALL_ROLES.len(),
                "generate should write every role"
            );
            for role in jcode_tui_style::ALL_ROLES.iter().copied() {
                let value = saved
                    .display
                    .colors
                    .get(role.key())
                    .unwrap_or_else(|| panic!("generate should write {}", role.key()));
                assert!(
                    jcode_tui_style::palette::parse_hex(value).is_some(),
                    "{} should be a valid hex color, got {value}",
                    role.key()
                );
            }

            assert!(dispatch_local_command(&mut app, "/colors reset"));
        });
    }

    #[test]
    fn bad_input_is_rejected_without_touching_the_config() {
        with_clean_config(|| {
            let mut app = create_test_app();
            for (input, expected) in [
                ("/colors bogus-role #ffffff", "Unknown color role"),
                ("/colors error not-a-color", "Invalid color"),
                ("/colors generate nope", "Invalid seed color"),
                ("/colors error", "Missing color value"),
            ] {
                assert!(dispatch_local_command(&mut app, input), "{input}");
                let output = last_message(&app);
                assert!(
                    output.contains(expected),
                    "{input} should report '{expected}', got: {output}"
                );
            }
            assert!(
                crate::config::Config::load().display.colors.is_empty(),
                "rejected input must not write anything"
            );
        });
    }

    #[test]
    fn unrelated_commands_are_not_swallowed() {
        // `/colors` shares a prefix with nothing today, but a future `/color*`
        // command must not be silently captured by this handler.
        let mut app = create_test_app();
        assert!(
            !dispatch_local_command(&mut app, "/colorscheme dracula"),
            "/colorscheme must not be claimed by /colors"
        );
    }
}

/// Unit tests for the `/worktree` helpers: argument parsing and worktree
/// creation.
mod worktree {
    #[test]
    fn parse_worktree_spec_takes_a_name_and_defaults_the_branch() {
        let spec = super::parse_worktree_spec("panel-settings").unwrap();
        assert_eq!(spec.name, "panel-settings");
        assert_eq!(spec.branch, None);
    }

    #[test]
    fn parse_worktree_spec_accepts_an_explicit_branch() {
        let spec = super::parse_worktree_spec("widgets -b fix/widgets").unwrap();
        assert_eq!(spec.name, "widgets");
        assert_eq!(spec.branch.as_deref(), Some("fix/widgets"));
    }

    #[test]
    fn parse_worktree_spec_accepts_long_branch_flag() {
        let spec = super::parse_worktree_spec("widgets --branch fix/widgets").unwrap();
        assert_eq!(spec.name, "widgets");
        assert_eq!(spec.branch.as_deref(), Some("fix/widgets"));
    }

    #[test]
    fn parse_worktree_spec_rejects_empty_input() {
        let err = super::parse_worktree_spec("").unwrap_err();
        assert!(err.contains("Usage: /worktree"), "{err}");
    }

    #[test]
    fn parse_worktree_spec_requires_name_before_branch() {
        let err = super::parse_worktree_spec("-b fix/x widgets").unwrap_err();
        assert!(err.contains("name must come first"), "{err}");
    }

    #[test]
    fn parse_worktree_spec_rejects_multi_segment_names() {
        for bad in ["a/b", "a\\b", ".", ".."] {
            let err = super::parse_worktree_spec(bad).unwrap_err();
            assert!(err.contains("single path segment"), "{bad}: {err}");
        }
    }

    #[test]
    fn parse_worktree_spec_rejects_option_like_names() {
        let err = super::parse_worktree_spec("-x").unwrap_err();
        assert!(err.contains("cannot start with '-'"), "{err}");
    }

    #[test]
    fn parse_worktree_spec_rejects_branch_missing_value() {
        let err = super::parse_worktree_spec("widgets -b").unwrap_err();
        assert!(err.contains("branch missing after -b"), "{err}");
    }

    #[test]
    fn parse_worktree_spec_rejects_unexpected_arguments() {
        let err = super::parse_worktree_spec("widgets extra").unwrap_err();
        assert!(err.contains("Unexpected argument 'extra'"), "{err}");
    }

    #[test]
    fn parse_worktree_spec_rejects_option_like_branch() {
        let err = super::parse_worktree_spec("widgets -b -x").unwrap_err();
        assert!(
            err.contains("not a valid git branch name") || err.contains("valid"),
            "{err}"
        );
    }

    #[test]
    fn parse_worktree_spec_rejects_invalid_worktree_name_for_branch() {
        // A name that fails as a git branch component (would become feat/<name>)
        // must be rejected up front.
        for bad in ["foo..bar", "foo@{x", "leading-dot.", "trailing~", "web.git"] {
            let err = super::parse_worktree_spec(bad).unwrap_err();
            assert!(
                err.contains("invalid git branch"),
                "{bad}: expected a git-branch error, got: {err}"
            );
        }
    }

    #[test]
    fn parse_worktree_spec_rejects_invalid_explicit_branch() {
        for bad in ["bad..branch", "colon:name", "@", "-dash", "x.git/y"] {
            let err = super::parse_worktree_spec(&format!("widgets -b {bad}")).unwrap_err();
            assert!(
                err.contains("not a valid git branch name"),
                "{bad}: expected a git-branch error, got: {err}"
            );
        }
    }

    /// Real `git worktree add` against a throwaway repo: proves the helper
    /// derives the repo root from the session working dir, creates
    /// `/.worktrees/<name>` on a `feat/<name>` branch, and returns the path.
    /// Cleanup removes the linked worktree and its branch so the temp repo can
    /// be torn down.
    #[test]
    fn create_git_worktree_makes_and_returns_the_worktree() {
        use crate::tui::app::tests::create_test_app;
        use std::process::Command;

        // Stand up a throwaway repo with one commit so `git worktree add` has a
        // branch tip to check out.
        let home = tempfile::tempdir().expect("temp home");
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for (args, envs) in [
            (vec!["init", "-b", "main"], vec![]),
            (vec!["add", "."], vec![]),
            (vec!["commit", "-m", "init"], vec![("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"), ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t")]),
        ] {
            let mut cmd = Command::new("git");
            cmd.args(&args).current_dir(&repo);
            for (k, v) in envs {
                cmd.env(k, v);
            }
            if args[0] == "add" {
                std::fs::write(repo.join("file.txt"), "hi\n").unwrap();
            }
            assert!(cmd.output().unwrap().status.success(), "git {args:?}");
        }

        // Point the session at the repo root: `create_git_worktree` must derive
        // the repo from it.
        let mut app = create_test_app();
        app.session.working_dir = Some(repo.display().to_string());

        let spec = super::parse_worktree_spec("feat-panel").unwrap();
        let wt_path = super::create_git_worktree(&app, &spec).unwrap();
        let wanted = repo.join(".worktrees").join("feat-panel");
        assert_eq!(wt_path, wanted);
        assert!(wanted.join("file.txt").exists(), "worktree should be populated");

        // The created worktree is on feat/feat-panel.
        let branch = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&wanted)
            .output()
            .unwrap();
        let branch = String::from_utf8_lossy(&branch.stdout);
        assert_eq!(branch.trim(), "feat/feat-panel");

        // A second call with the same name must report it already exists.
        let err = super::create_git_worktree(&app, &spec).unwrap_err();
        assert!(err.contains("already exists"), "{err}");

        // Cleanup: remove the worktree and its branch so the temp dir can drop.
        let cleanup = Command::new("git")
            .args(["worktree", "remove", "--force", &wt_path.display().to_string()])
            .current_dir(&repo)
            .output();
        if let Ok(out) = cleanup {
            let _ = Command::new("git")
                .args(["branch", "-D", "feat/feat-panel"])
                .current_dir(&repo)
                .output();
            drop(out);
        }
    }

    #[test]
    fn main_repo_root_from_a_linked_worktree_points_to_the_main_checkout() {
        use std::process::Command;

        let home = tempfile::tempdir().expect("temp home");
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for (args, envs) in [
            (vec!["init", "-b", "main"], vec![]),
            (vec!["add", "."], vec![]),
            (vec!["commit", "-m", "init"], vec![("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"), ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t")]),
        ] {
            let mut cmd = Command::new("git");
            cmd.args(&args).current_dir(&repo);
            for (k, v) in envs {
                cmd.env(k, v);
            }
            if args[0] == "add" {
                std::fs::write(repo.join("file.txt"), "hi\n").unwrap();
            }
            assert!(cmd.output().unwrap().status.success(), "git {args:?}");
        }

        // Create a linked worktree, then verify the main root is resolved from
        // inside it (mimicking a session already working in a worktree).
        let linked = repo.join(".worktrees").join("existing");
        let ok = Command::new("git")
            .args(["worktree", "add", "-b", "feat/existing", &linked.display().to_string()])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(ok.status.success(), "seed worktree failed");

        let root = super::main_repo_root_for_work_dir(&linked).unwrap();
        assert_eq!(root, repo);

        let _ = Command::new("git")
            .args(["worktree", "remove", "--force", &linked.display().to_string()])
            .current_dir(&repo)
            .output();
        let _ = Command::new("git")
            .args(["branch", "-D", "feat/existing"])
            .current_dir(&repo)
            .output();
    }

    #[test]
    fn create_git_worktree_in_non_git_dir_reports_no_repo() {
        use crate::tui::app::tests::create_test_app;

        // A temp dir that is not inside a git repository.
        let home = tempfile::tempdir().expect("temp home");
        let non_repo = home.path().join("not-a-repo");
        std::fs::create_dir_all(&non_repo).unwrap();

        let mut app = create_test_app();
        app.session.working_dir = Some(non_repo.display().to_string());

        let spec = super::parse_worktree_spec("panel").unwrap();
        let err = super::create_git_worktree(&app, &spec).unwrap_err();
        assert!(
            err.contains("No git repository found") || err.contains("not accessible"),
            "expected a missing-repo error, got: {err}"
        );
    }

    #[test]
    fn create_git_worktree_attaches_an_existing_branch() {
        use crate::tui::app::tests::create_test_app;
        use std::process::Command;

        // Throwaway repo with a pre-existing `feat/used` branch so that
        // `git worktree add -b feat/used <dir>` fails on the branch-collision;
        // the helper must fall back to attaching the existing branch.
        let home = tempfile::tempdir().expect("temp home");
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for (args, envs) in [
            (vec!["init", "-b", "main"], vec![]),
            (vec!["add", "."], vec![]),
            (vec!["commit", "-m", "init"], vec![("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"), ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t")]),
            (vec!["branch", "feat/used"], vec![]),
        ] {
            let mut cmd = Command::new("git");
            cmd.args(&args).current_dir(&repo);
            for (k, v) in envs {
                cmd.env(k, v);
            }
            if args[0] == "add" {
                std::fs::write(repo.join("file.txt"), "hi\n").unwrap();
            }
            assert!(cmd.output().unwrap().status.success(), "git {args:?}");
        }

        let mut app = create_test_app();
        app.session.working_dir = Some(repo.display().to_string());

        // A worktree name whose default branch already exists succeeds by
        // attaching the existing branch rather than failing.
        let spec = super::parse_worktree_spec("used").unwrap();
        let wt_path = super::create_git_worktree(&app, &spec).expect("should attach existing branch");
        assert_eq!(wt_path, repo.join(".worktrees").join("used"));
        assert!(wt_path.join("file.txt").exists(), "worktree should be populated");

        // The checked-out branch is the pre-existing one.
        let branch = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&wt_path)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), "feat/used");

        // Cleanup.
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force", &wt_path.display().to_string()])
            .current_dir(&repo)
            .output();
        let _ = Command::new("git")
            .args(["branch", "-D", "feat/used"])
            .current_dir(&repo)
            .output();
    }

    #[test]
    fn create_git_worktree_reports_branch_checked_out_elsewhere() {
        use crate::tui::app::tests::create_test_app;
        use std::process::Command;

        // Throwaway repo with a branch checked out in a linked worktree, so the
        // branch exists but cannot be attached to a new worktree.
        let home = tempfile::tempdir().expect("temp home");
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for (args, envs) in [
            (vec!["init", "-b", "main"], vec![]),
            (vec!["add", "."], vec![]),
            (vec!["commit", "-m", "init"], vec![("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"), ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t")]),
        ] {
            let mut cmd = Command::new("git");
            cmd.args(&args).current_dir(&repo);
            for (k, v) in envs {
                cmd.env(k, v);
            }
            if args[0] == "add" {
                std::fs::write(repo.join("file.txt"), "hi\n").unwrap();
            }
            assert!(cmd.output().unwrap().status.success(), "git {args:?}");
        }
        // Create a linked worktree that checks out `feat/taken`.
        let taken = repo.join(".worktrees").join("taken");
        let ok = Command::new("git")
            .args(["worktree", "add", "-b", "feat/taken", &taken.display().to_string()])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(ok.status.success(), "seed worktree failed");

        let mut app = create_test_app();
        app.session.working_dir = Some(repo.display().to_string());

        // A worktree named `new-slot` whose requested branch (`feat/taken`) is
        // checked out in the linked worktree must report a clear error. The dir
        // `repo/.worktrees/new-slot` does not exist, so the branch-check runs.
        let spec = super::parse_worktree_spec("new-slot -b feat/taken").unwrap();
        let err = super::create_git_worktree(&app, &spec).unwrap_err();
        assert!(
            err.contains("already checked out in another worktree"),
            "expected a checked-out-elsewhere error, got: {err}"
        );

        // Cleanup.
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force", &taken.display().to_string()])
            .current_dir(&repo)
            .output();
        let _ = Command::new("git")
            .args(["branch", "-D", "feat/taken"])
            .current_dir(&repo)
            .output();
    }

    #[test]
    fn create_git_worktree_reports_main_checkout_branch_reuse() {
        use crate::tui::app::tests::create_test_app;
        use std::process::Command;

        // Throwaway repo whose main checkout is on `main`.
        let home = tempfile::tempdir().expect("temp home");
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for (args, envs) in [
            (vec!["init", "-b", "main"], vec![]),
            (vec!["add", "."], vec![]),
            (vec!["commit", "-m", "init"], vec![("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"), ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t")]),
        ] {
            let mut cmd = Command::new("git");
            cmd.args(&args).current_dir(&repo);
            for (k, v) in envs {
                cmd.env(k, v);
            }
            if args[0] == "add" {
                std::fs::write(repo.join("file.txt"), "hi\n").unwrap();
            }
            assert!(cmd.output().unwrap().status.success(), "git {args:?}");
        }

        let mut app = create_test_app();
        app.session.working_dir = Some(repo.display().to_string());

        // Attaching a branch that is already the main checkout's branch cannot
        // be checked into a second worktree; report it precisely.
        let spec = super::parse_worktree_spec("new-slot -b main").unwrap();
        let err = super::create_git_worktree(&app, &spec).unwrap_err();
        assert!(
            err.contains("already checked out in the main worktree"),
            "expected a main-worktree reuse error, got: {err}"
        );
    }
}
