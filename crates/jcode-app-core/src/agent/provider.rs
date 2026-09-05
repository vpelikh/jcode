use super::*;

impl Agent {
    pub fn set_premium_mode(&self, mode: crate::provider::copilot::PremiumMode) {
        self.provider.set_premium_mode(mode);
    }

    pub fn premium_mode(&self) -> crate::provider::copilot::PremiumMode {
        self.provider.premium_mode()
    }

    pub fn provider_fork(&self) -> Arc<dyn Provider> {
        self.provider.fork()
    }

    pub fn provider_handle(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider)
    }

    pub fn available_models(&self) -> Vec<&'static str> {
        self.provider.available_models()
    }

    pub fn available_models_for_switching(&self) -> Vec<String> {
        self.provider.available_models_for_switching()
    }

    pub fn available_models_display(&self) -> Vec<String> {
        self.provider.available_models_display()
    }

    pub fn model_routes(&self) -> Vec<crate::provider::ModelRoute> {
        self.provider.model_routes()
    }

    pub fn model_catalog_snapshot(&self) -> jcode_provider_core::ModelCatalogSnapshot {
        jcode_provider_core::ModelCatalogSnapshot::new(
            Some(self.provider_name()),
            Some(self.provider_model()),
            self.available_models_display(),
            self.model_routes(),
        )
    }

    pub fn registry(&self) -> Registry {
        self.registry.clone()
    }

    pub async fn compaction_mode(&self) -> crate::config::CompactionMode {
        self.registry.compaction().read().await.mode()
    }

    pub async fn set_compaction_mode(&self, mode: crate::config::CompactionMode) -> Result<()> {
        let compaction = self.registry.compaction();
        let mut manager = compaction.write().await;
        manager.set_mode(mode);
        Ok(())
    }

    fn refresh_compaction_budget(&self) {
        let compaction = self.registry.compaction();
        match compaction.try_write() {
            Ok(mut manager) => manager.set_budget(self.provider.context_window()),
            Err(_) => crate::logging::warn(
                "Could not refresh compaction token budget after provider change: compaction manager is busy",
            ),
        }
    }

    #[cfg(test)]
    pub(crate) async fn compaction_token_budget(&self) -> usize {
        self.registry.compaction().read().await.token_budget()
    }

    pub fn provider_messages(&mut self) -> Vec<Message> {
        self.session.messages_for_provider()
    }

    pub fn set_model(&mut self, model: &str) -> Result<()> {
        self.set_model_from_provider_state_event(
            model,
            crate::provider::ProviderModelSelectionSource::User,
        )
    }

    pub fn set_route_selection(
        &mut self,
        selection: &crate::provider::RouteSelection,
    ) -> Result<()> {
        self.set_route_selection_from_provider_state_event(
            selection,
            crate::provider::ProviderModelSelectionSource::User,
        )
    }

    pub(crate) fn set_route_selection_from_auth(
        &mut self,
        selection: &crate::provider::RouteSelection,
    ) -> Result<()> {
        self.set_route_selection_from_provider_state_event(
            selection,
            crate::provider::ProviderModelSelectionSource::Auth,
        )
    }

    fn set_route_selection_from_provider_state_event(
        &mut self,
        selection: &crate::provider::RouteSelection,
        source: crate::provider::ProviderModelSelectionSource,
    ) -> Result<()> {
        self.provider.set_route_selection(selection)?;
        let resolved_model = self.provider.model();
        self.session.provider_key = Some(selection.runtime_key.stable_id());
        self.session.route_api_method = Some(selection.api_method.clone());
        self.session.model = Some(self.provider_model());
        let event = crate::provider::ProviderStateEvent::selected_model(source, resolved_model);
        self.provider_runtime_state.apply(event);
        self.refresh_compaction_budget();
        self.persist_session_best_effort("route selection");
        self.log_env_snapshot("set_route_selection");
        Ok(())
    }

    pub(crate) fn set_model_from_auth(&mut self, model: &str) -> Result<()> {
        self.set_model_from_provider_state_event(
            model,
            crate::provider::ProviderModelSelectionSource::Auth,
        )
    }

    fn set_model_from_provider_state_event(
        &mut self,
        model: &str,
        source: crate::provider::ProviderModelSelectionSource,
    ) -> Result<()> {
        crate::provider::set_model_with_auth_refresh(self.provider.as_ref(), model)?;
        let resolved_model = self.provider.model();
        self.session.provider_key =
            crate::provider::MultiProvider::session_provider_key_after_model_switch(
                model,
                self.provider.name(),
                self.session.provider_key.as_deref(),
            );
        self.session.model = Some(self.provider_model());
        let event = crate::provider::ProviderStateEvent::selected_model(source, resolved_model);
        self.provider_runtime_state.apply(event);
        self.refresh_compaction_budget();
        self.persist_session_best_effort("model selection");
        self.log_env_snapshot("set_model");
        Ok(())
    }

    pub(crate) fn provider_model_selection_generation(&self) -> u64 {
        self.provider_runtime_state.selection_generation()
    }

    pub(crate) fn user_selected_provider_model_after(&self, generation: u64) -> bool {
        self.provider_runtime_state.user_selected_after(generation)
    }

    pub fn restore_reasoning_effort_from_session(&mut self) {
        if let Some(effort) = self.session.reasoning_effort.clone() {
            if let Err(e) = self.provider.set_reasoning_effort(&effort) {
                crate::logging::error(&format!(
                    "Failed to restore session reasoning effort '{}': {}",
                    effort, e
                ));
            }
        } else {
            self.session.reasoning_effort = self.provider.reasoning_effort();
        }
        // Mirror the effort into the deadlock-free side-table so server handlers
        // (e.g. the swarm seed handler) can learn this session's effort without
        // taking the agent lock.
        crate::session_effort::record_session_effort(
            &self.session.id,
            self.session.reasoning_effort.as_deref(),
        );
    }

    pub fn set_reasoning_effort(&mut self, effort: &str) -> Result<Option<String>> {
        self.provider.set_reasoning_effort(effort)?;
        let current = self.provider.reasoning_effort();
        self.session.reasoning_effort = current.clone();
        // Keep the side-table in sync (see `restore_reasoning_effort_from_session`).
        crate::session_effort::record_session_effort(&self.session.id, current.as_deref());
        self.log_env_snapshot("set_reasoning_effort");
        self.session.save()?;
        Ok(current)
    }

    pub fn subagent_model(&self) -> Option<String> {
        self.session.subagent_model.clone()
    }

    pub fn set_subagent_model(&mut self, model: Option<String>) -> Result<()> {
        self.session.subagent_model = model;
        self.log_env_snapshot("set_subagent_model");
        self.session.save()?;
        Ok(())
    }

    pub fn session_provider_key(&self) -> Option<String> {
        self.session.provider_key.clone()
    }

    /// API method/runtime route used to select the active model (e.g.
    /// "openai-api", "claude-oauth", "openai-compatible:nvidia-nim"). Spawned
    /// swarm agents inherit this so they reconstruct the coordinator's exact
    /// auth route instead of falling back to the config default.
    pub fn session_route_api_method(&self) -> Option<String> {
        self.session.route_api_method.clone()
    }

    /// The credential the active provider will use for the next request, when
    /// the provider distinguishes OAuth (subscription) from API key (cost).
    /// Resolved authoritatively here so remote clients can render billing/usage
    /// without re-deriving it from the provider name.
    pub fn active_resolved_credential(&self) -> Option<jcode_provider_core::ResolvedCredential> {
        self.provider.active_resolved_credential()
    }

    pub fn set_session_provider_key(&mut self, provider_key: Option<String>) {
        self.session.provider_key = provider_key;
    }

    pub fn rename_session_title(&mut self, title: Option<String>) -> Result<String> {
        self.session.rename_title(title);
        self.log_env_snapshot("rename_session");
        self.session.save()?;
        Ok(self.session.display_title_or_name().to_string())
    }

    pub fn autoreview_enabled(&self) -> Option<bool> {
        self.session.autoreview_enabled
    }

    pub fn set_autoreview_enabled(&mut self, enabled: bool) -> Result<()> {
        self.session.autoreview_enabled = Some(enabled);
        self.log_env_snapshot("set_autoreview_enabled");
        self.session.save()?;
        Ok(())
    }

    pub fn autojudge_enabled(&self) -> Option<bool> {
        self.session.autojudge_enabled
    }

    pub fn set_autojudge_enabled(&mut self, enabled: bool) -> Result<()> {
        self.session.autojudge_enabled = Some(enabled);
        self.log_env_snapshot("set_autojudge_enabled");
        self.session.save()?;
        Ok(())
    }

    /// Set the working directory for this session
    pub fn set_working_dir(&mut self, dir: &str) {
        if self.session.working_dir.as_deref() == Some(dir) {
            return;
        }
        self.session.working_dir = Some(dir.to_string());
        self.refresh_agents_md_snapshot();
        self.session.refresh_initial_session_context_message();
        self.log_env_snapshot("working_dir");
    }

    /// Grouped working-directory change invoked from a user `/cd` request.
    ///
    /// Beyond [`Self::set_working_dir`], this persists the session, refreshes
    /// project-scoped skills, and appends a model-visible notice about the
    /// change so the agent re-scopes even after a conversation has progressed
    /// (the plain `refresh_initial_session_context_message` no-ops once visible
    /// history exists).
    pub fn set_working_dir_grouped(&mut self, dir: &str) -> anyhow::Result<()> {
        let old_dir = self
            .session
            .working_dir
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let base = self
            .session
            .working_dir
            .as_deref()
            .map(std::path::Path::new)
            .unwrap_or_else(|| std::path::Path::new("."));
        let normalized = resolve_working_dir(base, dir)?;
        if self.session.working_dir.as_deref() == Some(normalized.as_str()) {
            return Ok(());
        }
        self.session.working_dir = Some(normalized.clone());
        self.refresh_agents_md_snapshot();
        // Rebuild the initial context system-reminder when there is still no
        // visible conversation; otherwise the appended notice below carries the
        // change to the model. Best-effort: both are tolerant of a missing
        // initial context message.
        self.session.refresh_initial_session_context_message();
        self.session.append_working_dir_notice(&old_dir, &normalized);
        self.session.save()?;
        self.log_env_snapshot("working_dir");
        Ok(())
    }

    /// Get the working directory for this session
    pub fn working_dir(&self) -> Option<&str> {
        self.session.working_dir.as_deref()
    }

    /// Get the stored messages (for transcript export)
    pub fn messages(&self) -> &[StoredMessage] {
        &self.session.messages
    }
}

/// Resolve a user-supplied working directory (from `/cd`) to an absolute path.
///
/// Supports `~`/`~/...` home expansion (for the *current* user) and relative
/// paths, which are resolved against `base` (the session's current working
/// directory, not the daemon's process cwd so the result is stable regardless
/// of where the server launched).
fn resolve_working_dir(base: &std::path::Path, dir: &str) -> anyhow::Result<String> {
    let dir = dir.trim();
    if dir.is_empty() {
        anyhow::bail!("working directory must not be empty");
    }
    let expanded = if let Some(rest) = dir.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => std::path::PathBuf::from(dir),
        }
    } else if dir == "~" {
        dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from(dir))
    } else {
        std::path::PathBuf::from(dir)
    };

    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };

    if !candidate.exists() || !candidate.is_dir() {
        anyhow::bail!("directory does not exist: {}", candidate.display());
    }

    // Canonicalize to collapse `.`/`..` and resolve symlinks, matching how the
    // git info cache and compass derive their keys. Fall back to a lexical
    // cleanup when canonicalization fails so the path stays usable.
    match std::fs::canonicalize(&candidate) {
        Ok(canonical) => Ok(canonical.to_string_lossy().into_owned()),
        Err(_) => Ok(candidate.to_string_lossy().into_owned()),
    }
}

#[cfg(test)]
mod resolve_working_dir_tests {
    use super::resolve_working_dir;

    #[test]
    fn absolute_path_is_normalized() {
        let dir = std::env::temp_dir().join("jcode-wd-absolute-test");
        std::fs::create_dir_all(&dir).unwrap();
        let result = resolve_working_dir(std::path::Path::new("/tmp"), dir.to_str().unwrap()).unwrap();
        assert!(result.ends_with("jcode-wd-absolute-test"));
        assert!(std::path::Path::new(&result).is_absolute());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn relative_path_resolves_against_base() {
        let base = std::env::temp_dir().join("jcode-wd-base-test");
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let result = resolve_working_dir(&base, "sub").unwrap();
        assert!(result.ends_with("jcode-wd-base-test/sub"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn missing_directory_is_rejected() {
        let base = std::env::temp_dir().join("jcode-wd-missing-base");
        std::fs::create_dir_all(&base).unwrap();
        let err = resolve_working_dir(&base, "does-not-exist").unwrap_err();
        assert!(err.to_string().contains("does not exist"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn empty_path_is_rejected() {
        let base = std::env::temp_dir().join("jcode-wd-empty-base");
        std::fs::create_dir_all(&base).unwrap();
        let err = resolve_working_dir(&base, "   ").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn dotdot_is_normalized_away() {
        let base = std::env::temp_dir().join("jcode-wd-dotdot-base").join("nested");
        std::fs::create_dir_all(&base).unwrap();
        let result = resolve_working_dir(&base, "../").unwrap();
        let expected = std::fs::canonicalize(std::env::temp_dir().join("jcode-wd-dotdot-base")).unwrap();
        assert_eq!(std::path::Path::new(&result), expected.as_path());
        std::fs::remove_dir_all(std::env::temp_dir().join("jcode-wd-dotdot-base")).unwrap();
    }
}
