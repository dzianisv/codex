use super::cache::ModelsCacheManager;
use crate::api_bridge::CoreAuthProvider;
use crate::api_bridge::auth_provider_from_auth;
use crate::api_bridge::map_api_error;
use crate::auth::AuthManager;
use crate::auth::AuthMode;
use crate::config::Config;
use crate::default_client::build_reqwest_client;
use crate::error::CodexErr;
use crate::error::Result as CoreResult;
use crate::model_provider_info::ModelProviderInfo;
use crate::models_manager::collaboration_mode_presets::CollaborationModesConfig;
use crate::models_manager::collaboration_mode_presets::builtin_collaboration_mode_presets;
use crate::models_manager::model_info;
use codex_api::AuthProvider;
use codex_api::ModelsClient;
use codex_api::ReqwestTransport;
use codex_api::is_azure_responses_wire_base_url;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use http::HeaderMap;
use http::header::ETAG;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::TryLockError;
use tokio::time::timeout;
use tracing::error;
use tracing::info;
use tracing::warn;

const MODEL_CACHE_FILE: &str = "models_cache.json";
const DEFAULT_MODEL_CACHE_TTL: Duration = Duration::from_secs(300);
const MODELS_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MODELS_DEV_CATALOG_URL: &str = "https://models.dev/api.json";
const MODELS_DEV_URL_ENV_KEY: &str = "CODEX_MODELS_DEV_URL";

#[derive(Debug, Deserialize)]
struct OpenAiCompatModelsResponse {
    data: Vec<OpenAiCompatModel>,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompatModel {
    id: String,
    #[serde(default)]
    model_picker_enabled: Option<bool>,
    #[serde(default)]
    supported_endpoints: Vec<String>,
}

impl OpenAiCompatModel {
    fn is_picker_enabled(&self) -> bool {
        !matches!(self.model_picker_enabled, Some(false))
    }

    fn supports_responses_endpoint(&self) -> bool {
        self.supported_endpoints.is_empty()
            || self
                .supported_endpoints
                .iter()
                .any(|endpoint| endpoint.trim_end_matches('/').ends_with("/responses"))
    }
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaTagsModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagsModel {
    name: String,
    #[serde(default)]
    model_picker_enabled: Option<bool>,
}

impl OllamaTagsModel {
    fn is_picker_enabled(&self) -> bool {
        !matches!(self.model_picker_enabled, Some(false))
    }
}

#[derive(Debug, Deserialize)]
struct ModelsDevProvider {
    #[serde(default)]
    name: String,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    models: HashMap<String, ModelsDevModel>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevModel {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    model_picker_enabled: Option<bool>,
    #[serde(default)]
    options: HashMap<String, JsonValue>,
}

impl ModelsDevModel {
    fn resolved_id(&self, fallback: &str) -> Option<String> {
        let id = self.id.as_deref().unwrap_or(fallback).trim();
        if id.is_empty() {
            None
        } else {
            Some(id.to_string())
        }
    }

    fn is_picker_enabled(&self) -> bool {
        let option_flag = self
            .options
            .get("model_picker_enabled")
            .and_then(JsonValue::as_bool);
        !matches!(self.model_picker_enabled.or(option_flag), Some(false))
    }

    fn is_deprecated(&self) -> bool {
        self.status
            .as_deref()
            .is_some_and(|status| status.eq_ignore_ascii_case("deprecated"))
    }
}

/// Strategy for refreshing available models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStrategy {
    /// Always fetch from the network, ignoring cache.
    Online,
    /// Only use cached data, never fetch from the network.
    Offline,
    /// Use cache if available and fresh, otherwise fetch from the network.
    OnlineIfUncached,
}

/// How the manager's base catalog is sourced for the lifetime of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatalogMode {
    /// Start from bundled `models.json` and allow cache/network refresh updates.
    Default,
    /// Use a caller-provided catalog as authoritative and do not mutate it via refresh.
    Custom,
}

/// Coordinates remote model discovery plus cached metadata on disk.
#[derive(Debug)]
pub struct ModelsManager {
    remote_models: RwLock<Vec<ModelInfo>>,
    catalog_mode: CatalogMode,
    collaboration_modes_config: CollaborationModesConfig,
    auth_manager: Arc<AuthManager>,
    etag: RwLock<Option<String>>,
    cache_manager: ModelsCacheManager,
    provider: ModelProviderInfo,
    models_dev_api_url_override: Option<String>,
}

impl ModelsManager {
    /// Construct a manager scoped to the provided `AuthManager`.
    ///
    /// Uses `codex_home` to store cached model metadata and initializes with bundled catalog
    /// When `model_catalog` is provided, it becomes the authoritative remote model list and
    /// background refreshes from `/models` are disabled.
    pub fn new(
        codex_home: PathBuf,
        auth_manager: Arc<AuthManager>,
        model_catalog: Option<ModelsResponse>,
        collaboration_modes_config: CollaborationModesConfig,
        provider: ModelProviderInfo,
    ) -> Self {
        let cache_path = codex_home.join(MODEL_CACHE_FILE);
        let cache_manager = ModelsCacheManager::new(cache_path, DEFAULT_MODEL_CACHE_TTL);
        let catalog_mode = if model_catalog.is_some() {
            CatalogMode::Custom
        } else {
            CatalogMode::Default
        };
        let remote_models = model_catalog
            .map(|catalog| catalog.models)
            .unwrap_or_else(|| {
                Self::load_remote_models_from_file()
                    .unwrap_or_else(|err| panic!("failed to load bundled models.json: {err}"))
            });
        Self {
            remote_models: RwLock::new(remote_models),
            catalog_mode,
            collaboration_modes_config,
            auth_manager,
            etag: RwLock::new(None),
            cache_manager,
            provider,
            models_dev_api_url_override: None,
        }
    }

    /// Construct a manager with an explicit provider used for remote model refreshes.
    pub fn new_with_provider(
        codex_home: PathBuf,
        auth_manager: Arc<AuthManager>,
        model_catalog: Option<ModelsResponse>,
        collaboration_modes_config: CollaborationModesConfig,
        provider: ModelProviderInfo,
    ) -> Self {
        Self::new(
            codex_home,
            auth_manager,
            model_catalog,
            collaboration_modes_config,
            provider,
        )
    }

    /// List all available models, refreshing according to the specified strategy.
    ///
    /// Returns model presets sorted by priority and filtered by auth mode and visibility.
    pub async fn list_models(&self, refresh_strategy: RefreshStrategy) -> Vec<ModelPreset> {
        if let Err(err) = self.refresh_available_models(refresh_strategy).await {
            error!("failed to refresh available models: {err}");
        }
        let remote_models = self.get_remote_models().await;
        self.build_available_models(remote_models)
    }

    /// List collaboration mode presets.
    ///
    /// Returns a static set of presets seeded with the configured model.
    pub fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        self.list_collaboration_modes_for_config(self.collaboration_modes_config)
    }

    pub fn list_collaboration_modes_for_config(
        &self,
        collaboration_modes_config: CollaborationModesConfig,
    ) -> Vec<CollaborationModeMask> {
        builtin_collaboration_mode_presets(collaboration_modes_config)
    }

    /// Attempt to list models without blocking, using the current cached state.
    ///
    /// Returns an error if the internal lock cannot be acquired.
    pub fn try_list_models(&self) -> Result<Vec<ModelPreset>, TryLockError> {
        let remote_models = self.try_get_remote_models()?;
        Ok(self.build_available_models(remote_models))
    }

    // todo(aibrahim): should be visible to core only and sent on session_configured event
    /// Get the model identifier to use, refreshing according to the specified strategy.
    ///
    /// If `model` is provided, returns it directly. Otherwise selects the default based on
    /// auth mode and available models.
    pub async fn get_default_model(
        &self,
        model: &Option<String>,
        refresh_strategy: RefreshStrategy,
    ) -> String {
        if let Some(model) = model.as_ref() {
            if self.provider_catalog_is_authoritative() {
                if let Err(err) = self.refresh_available_models(refresh_strategy).await {
                    error!("failed to refresh available models: {err}");
                }
                let remote_models = self.get_remote_models().await;
                let available = self.build_available_models(remote_models);
                if available.iter().any(|preset| preset.model == *model) {
                    return model.to_string();
                }
                if let Some(fallback) = available
                    .iter()
                    .find(|preset| preset.is_default)
                    .or_else(|| available.first())
                    .map(|preset| preset.model.clone())
                {
                    warn!(
                        requested_model = model,
                        fallback_model = fallback,
                        provider_name = %self.provider.name,
                        "requested model is unavailable for provider; falling back to default"
                    );
                    return fallback;
                }
            }
            return model.to_string();
        }
        if let Err(err) = self.refresh_available_models(refresh_strategy).await {
            error!("failed to refresh available models: {err}");
        }
        let remote_models = self.get_remote_models().await;
        let available = self.build_available_models(remote_models);
        available
            .iter()
            .find(|model| model.is_default)
            .or_else(|| available.first())
            .map(|model| model.model.clone())
            .unwrap_or_default()
    }

    // todo(aibrahim): look if we can tighten it to pub(crate)
    /// Look up model metadata, applying remote overrides and config adjustments.
    pub async fn get_model_info(&self, model: &str, config: &Config) -> ModelInfo {
        let remote_models = self.get_remote_models().await;
        Self::construct_model_info_from_candidates(model, &remote_models, config)
    }

    fn find_model_by_longest_prefix(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
        let mut best: Option<ModelInfo> = None;
        for candidate in candidates {
            if !model.starts_with(&candidate.slug) {
                continue;
            }
            let is_better_match = if let Some(current) = best.as_ref() {
                candidate.slug.len() > current.slug.len()
            } else {
                true
            };
            if is_better_match {
                best = Some(candidate.clone());
            }
        }
        best
    }

    /// Retry metadata lookup for a single namespaced slug like `namespace/model-name`.
    ///
    /// This only strips one leading namespace segment and only when the namespace is ASCII
    /// alphanumeric/underscore (`\\w+`) to avoid broadly matching arbitrary aliases.
    fn find_model_by_namespaced_suffix(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
        let (namespace, suffix) = model.split_once('/')?;
        if suffix.contains('/') {
            return None;
        }
        if !namespace
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        Self::find_model_by_longest_prefix(suffix, candidates)
    }

    fn construct_model_info_from_candidates(
        model: &str,
        candidates: &[ModelInfo],
        config: &Config,
    ) -> ModelInfo {
        let remote = Self::find_metadata_candidate(model, candidates);
        let model_info = if let Some(remote) = remote {
            ModelInfo {
                slug: model.to_string(),
                used_fallback_model_metadata: false,
                ..remote
            }
        } else {
            model_info::model_info_from_slug(model)
        };
        model_info::with_config_overrides(model_info, config)
    }

    /// Reuse the same metadata lookup rules for both direct model selection and provider-backed
    /// model listings.
    fn find_metadata_candidate(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
        // First use the normal longest-prefix match. If that misses, allow a narrowly scoped
        // retry for namespaced slugs like `custom/gpt-5.3-codex`.
        Self::find_model_by_longest_prefix(model, candidates)
            .or_else(|| Self::find_model_by_namespaced_suffix(model, candidates))
    }

    /// Refresh models if the provided ETag differs from the cached ETag.
    ///
    /// Uses `Online` strategy to fetch latest models when ETags differ.
    pub(crate) async fn refresh_if_new_etag(&self, etag: String) {
        let current_etag = self.get_etag().await;
        if current_etag.clone().is_some() && current_etag.as_deref() == Some(etag.as_str()) {
            if let Err(err) = self.cache_manager.renew_cache_ttl().await {
                error!("failed to renew cache TTL: {err}");
            }
            return;
        }
        if let Err(err) = self.refresh_available_models(RefreshStrategy::Online).await {
            error!("failed to refresh available models: {err}");
        }
    }

    /// Refresh available models according to the specified strategy.
    async fn refresh_available_models(&self, refresh_strategy: RefreshStrategy) -> CoreResult<()> {
        // don't override the custom model catalog if one was provided by the user
        if matches!(self.catalog_mode, CatalogMode::Custom) {
            return Ok(());
        }

        // OpenAI in API-key mode still relies on bundled catalog metadata.
        // All other providers (ChatGPT, Copilot, local OSS, and non-OpenAI)
        // should refresh from a provider-authoritative source.
        let should_refresh_from_network = self.auth_manager.auth_mode() == Some(AuthMode::Chatgpt)
            || self.provider_catalog_is_authoritative();
        if !should_refresh_from_network {
            if matches!(
                refresh_strategy,
                RefreshStrategy::Offline | RefreshStrategy::OnlineIfUncached
            ) {
                self.try_load_cache().await;
            }
            return Ok(());
        }

        match refresh_strategy {
            RefreshStrategy::Offline => {
                // Only try to load from cache, never fetch
                self.try_load_cache().await;
                Ok(())
            }
            RefreshStrategy::OnlineIfUncached => {
                if self.provider.is_github_copilot_provider() {
                    info!("models cache: bypassing cache for github-copilot provider");
                    return self.fetch_and_update_models_with_guard().await;
                }
                // Try cache first, fall back to online if unavailable
                if self.try_load_cache().await {
                    info!("models cache: using cached models for OnlineIfUncached");
                    return Ok(());
                }
                info!("models cache: cache miss, fetching remote models");
                self.fetch_and_update_models_with_guard().await
            }
            RefreshStrategy::Online => {
                // Always fetch from network
                self.fetch_and_update_models_with_guard().await
            }
        }
    }

    async fn fetch_and_update_models_with_guard(&self) -> CoreResult<()> {
        match self.fetch_and_update_models().await {
            Ok(()) => Ok(()),
            Err(err) => {
                if self.provider_catalog_is_authoritative() {
                    warn!(
                        error = %err,
                        "provider-authoritative models refresh failed; clearing catalog to avoid bundled fallback models"
                    );
                    self.apply_remote_models(Vec::new()).await;
                }
                Err(err)
            }
        }
    }

    async fn fetch_and_update_models(&self) -> CoreResult<()> {
        let _timer =
            codex_otel::start_global_timer("codex.remote_models.fetch_update.duration_ms", &[]);
        let client_version = crate::models_manager::client_version_to_whole();
        let auth = self.auth_manager.auth().await;
        let auth_mode = self.auth_manager.auth_mode();
        let api_provider = self.provider.to_api_provider(auth_mode)?;
        let models_and_etag = if self.provider.is_github_copilot_provider() {
            let api_auth = match auth_provider_from_auth(auth.clone(), &self.provider) {
                Ok(api_auth) => api_auth,
                Err(CodexErr::EnvVar(_)) => {
                    info!("models refresh skipped: github-copilot token is unavailable");
                    // Avoid showing bundled fallback models when the provider
                    // cannot be authenticated.
                    self.apply_remote_models(Vec::new()).await;
                    *self.etag.write().await = None;
                    return Ok(());
                }
                Err(err) => return Err(err),
            };
            let Some(token) = api_auth.bearer_token() else {
                info!("models refresh skipped: github-copilot auth token is unavailable");
                // Avoid showing bundled fallback models when the provider
                // cannot be authenticated.
                self.apply_remote_models(Vec::new()).await;
                *self.etag.write().await = None;
                return Ok(());
            };

            let url = format!("{}/models", api_provider.base_url.trim_end_matches('/'));
            // Strip the Openai-Intent header for the /models listing request.
            // This header is intended for chat/completion requests and causes
            // the server to filter out model families (e.g. Gemini) that may
            // not match the declared intent.
            let mut listing_headers = api_provider.headers.clone();
            listing_headers.remove("openai-intent");
            let request = build_reqwest_client()
                .get(url)
                .query(&[("client_version", client_version.clone())])
                .headers(listing_headers)
                .bearer_auth(token);
            self.fetch_openai_compat_models(request, "github-copilot")
                .await?
        } else if self.provider.is_local_oss_provider() {
            // Local OSS providers (Ollama, LM Studio) expose an
            // OpenAI-compatible `/v1/models` endpoint that returns the same
            // `{ "data": [{ "id": "..." }] }` format as Copilot.  No auth
            // is required.
            let url = format!("{}/models", api_provider.base_url.trim_end_matches('/'));
            let request = build_reqwest_client().get(url);
            match self.fetch_openai_compat_models(request, "local-oss").await {
                Ok(models_and_etag) => models_and_etag,
                Err(primary_err) => {
                    info!(
                        "local-oss /v1/models lookup failed, attempting ollama /api/tags fallback: {primary_err}"
                    );
                    match self.fetch_ollama_tags_models(&api_provider.base_url).await {
                        Ok(models_and_etag) => models_and_etag,
                        Err(fallback_err) => {
                            return Err(CodexErr::Stream(
                                format!(
                                    "failed to fetch local-oss models from /v1/models ({primary_err}) and /api/tags ({fallback_err})"
                                ),
                                None,
                            ));
                        }
                    }
                }
            }
        } else if !self.provider.is_openai() {
            match self.fetch_models_from_models_dev().await {
                Ok(Some(models_and_etag)) => models_and_etag,
                Ok(None) => {
                    info!(
                        provider_name = %self.provider.name,
                        "models.dev match not found; falling back to provider /models endpoint"
                    );
                    self.fetch_openai_compat_models_for_provider(
                        auth.clone(),
                        api_provider.clone(),
                        "openai-compatible-provider",
                    )
                    .await?
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        provider_name = %self.provider.name,
                        "models.dev lookup failed; falling back to provider /models endpoint"
                    );
                    self.fetch_openai_compat_models_for_provider(
                        auth.clone(),
                        api_provider.clone(),
                        "openai-compatible-provider",
                    )
                    .await?
                }
            }
        } else {
            let api_auth = auth_provider_from_auth(auth, &self.provider)?;
            self.fetch_models_from_provider_api(api_auth, api_provider, client_version.clone())
                .await?
        };
        let (models, etag) = models_and_etag;

        self.apply_remote_models(models.clone()).await;
        *self.etag.write().await = etag.clone();
        let provider_scope = self.cache_scope_key();
        self.cache_manager
            .persist_cache(&models, etag, client_version, provider_scope)
            .await;
        Ok(())
    }

    /// Send a GET request that returns an OpenAI-compatible models response
    /// (`{ "data": [{ "id": "..." }, ...] }`) and convert the entries into
    /// [`ModelInfo`] values.  Used for both GitHub Copilot and local OSS
    /// providers (Ollama / LM Studio).
    async fn fetch_openai_compat_models(
        &self,
        request: reqwest::RequestBuilder,
        provider_label: &str,
    ) -> CoreResult<(Vec<ModelInfo>, Option<String>)> {
        let response = timeout(MODELS_REFRESH_TIMEOUT, request.send())
            .await
            .map_err(|_| CodexErr::Timeout)?
            .map_err(|err| CodexErr::Stream(err.to_string(), None))?;
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CodexErr::Stream(
                format!(
                    "failed to fetch models from {provider_label} provider: {status}; body: {body}"
                ),
                None,
            ));
        }
        let payload: OpenAiCompatModelsResponse = response
            .json()
            .await
            .map_err(|err| CodexErr::Stream(err.to_string(), None))?;
        let mut models = payload
            .data
            .into_iter()
            .filter(OpenAiCompatModel::is_picker_enabled)
            .collect::<Vec<_>>();
        models.sort_by_key(|model| !model.supports_responses_endpoint());
        let model_ids = models.into_iter().map(|model| model.id).collect::<Vec<_>>();
        let models = self.map_provider_model_ids(model_ids);
        Ok((models, etag))
    }

    async fn fetch_openai_compat_models_for_provider(
        &self,
        auth: Option<crate::CodexAuth>,
        api_provider: codex_api::Provider,
        provider_label: &str,
    ) -> CoreResult<(Vec<ModelInfo>, Option<String>)> {
        let url = format!("{}/models", api_provider.base_url.trim_end_matches('/'));
        let api_auth = auth_provider_from_auth(auth, &self.provider)?;
        let mut request = build_reqwest_client()
            .get(url)
            .headers(api_provider.headers);
        if let Some(query_params) = api_provider.query_params.as_ref() {
            request = request.query(query_params);
        }
        if let Some(token) = api_auth.bearer_token() {
            request = request.bearer_auth(token);
        }
        self.fetch_openai_compat_models(request, provider_label)
            .await
    }

    async fn fetch_ollama_tags_models(
        &self,
        base_url: &str,
    ) -> CoreResult<(Vec<ModelInfo>, Option<String>)> {
        let tags_url = Self::ollama_tags_url(base_url);
        let response = timeout(
            MODELS_REFRESH_TIMEOUT,
            build_reqwest_client().get(tags_url).send(),
        )
        .await
        .map_err(|_| CodexErr::Timeout)?
        .map_err(|err| CodexErr::Stream(err.to_string(), None))?;

        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CodexErr::Stream(
                format!("failed to fetch models from ollama /api/tags: {status}; body: {body}"),
                None,
            ));
        }

        let payload: OllamaTagsResponse = response
            .json()
            .await
            .map_err(|err| CodexErr::Stream(err.to_string(), None))?;
        let model_ids = payload
            .models
            .into_iter()
            .filter(OllamaTagsModel::is_picker_enabled)
            .map(|model| model.name)
            .collect::<Vec<_>>();
        let models = self.map_provider_model_ids(model_ids);
        Ok((models, etag))
    }

    async fn fetch_models_from_provider_api(
        &self,
        api_auth: CoreAuthProvider,
        api_provider: codex_api::Provider,
        client_version: String,
    ) -> CoreResult<(Vec<ModelInfo>, Option<String>)> {
        let transport = ReqwestTransport::new(build_reqwest_client());
        let client = ModelsClient::new(transport, api_provider, api_auth);

        timeout(
            MODELS_REFRESH_TIMEOUT,
            client.list_models(&client_version, HeaderMap::new()),
        )
        .await
        .map_err(|_| CodexErr::Timeout)?
        .map_err(map_api_error)
    }

    async fn fetch_models_from_models_dev(
        &self,
    ) -> CoreResult<Option<(Vec<ModelInfo>, Option<String>)>> {
        let catalog_url = self.models_dev_api_url();
        let response = timeout(
            MODELS_REFRESH_TIMEOUT,
            build_reqwest_client().get(catalog_url.clone()).send(),
        )
        .await
        .map_err(|_| CodexErr::Timeout)?
        .map_err(|err| CodexErr::Stream(err.to_string(), None))?;
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CodexErr::Stream(
                format!(
                    "failed to fetch models.dev catalog from {catalog_url}: {status}; body: {body}"
                ),
                None,
            ));
        }
        let payload: HashMap<String, ModelsDevProvider> = response
            .json()
            .await
            .map_err(|err| CodexErr::Stream(err.to_string(), None))?;
        let Some((provider_id, provider)) = self.match_models_dev_provider(&payload) else {
            return Ok(None);
        };
        let models = self.map_models_dev_provider(provider);
        info!(
            provider_name = %self.provider.name,
            models_dev_provider_id = provider_id,
            models_count = models.len(),
            "using models.dev provider catalog for model picker"
        );
        Ok(Some((models, etag)))
    }

    fn models_dev_api_url(&self) -> String {
        let raw = self
            .models_dev_api_url_override
            .clone()
            .or_else(|| std::env::var(MODELS_DEV_URL_ENV_KEY).ok())
            .unwrap_or_else(|| DEFAULT_MODELS_DEV_CATALOG_URL.to_string());
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return DEFAULT_MODELS_DEV_CATALOG_URL.to_string();
        }
        if trimmed.ends_with(".json") {
            trimmed.to_string()
        } else {
            format!("{}/api.json", trimmed.trim_end_matches('/'))
        }
    }

    fn match_models_dev_provider<'a>(
        &self,
        catalog: &'a HashMap<String, ModelsDevProvider>,
    ) -> Option<(&'a str, &'a ModelsDevProvider)> {
        for provider_alias in self.models_dev_provider_aliases() {
            if let Some((provider_id, provider)) = catalog.get_key_value(&provider_alias) {
                return Some((provider_id.as_str(), provider));
            }
        }
        if let Some((provider_id, provider)) = catalog.iter().find(|(_, provider)| {
            let normalized_provider_name = Self::normalize_provider_key(&provider.name);
            self.models_dev_provider_aliases()
                .iter()
                .any(|alias| alias == &normalized_provider_name)
        }) {
            return Some((provider_id.as_str(), provider));
        }

        let provider_host = self
            .provider
            .base_url
            .as_deref()
            .and_then(Self::extract_host_from_url)?;
        let mut host_matches = catalog.iter().filter(|(_, provider)| {
            provider
                .api
                .as_deref()
                .and_then(Self::extract_host_from_url)
                .is_some_and(|host| host == provider_host)
        });
        let first_match = host_matches.next()?;
        if host_matches.next().is_some() {
            warn!(
                provider_name = %self.provider.name,
                provider_host = %provider_host,
                "multiple models.dev providers matched provider host; skipping host fallback match"
            );
            return None;
        }
        Some((first_match.0.as_str(), first_match.1))
    }

    fn models_dev_provider_aliases(&self) -> Vec<String> {
        let normalized_provider_name = Self::normalize_provider_key(&self.provider.name);
        let mut aliases = vec![normalized_provider_name.clone()];
        let azure_named_provider = normalized_provider_name
            .split('-')
            .collect::<Vec<_>>()
            .windows(2)
            .any(|window| window == ["azure", "openai"]);
        if (azure_named_provider
            || is_azure_responses_wire_base_url(
                &self.provider.name,
                self.provider.base_url.as_deref(),
            ))
            && !aliases.iter().any(|alias| alias == "azure")
        {
            aliases.push("azure".to_string());
        }
        aliases
    }

    fn map_models_dev_provider(&self, provider: &ModelsDevProvider) -> Vec<ModelInfo> {
        let mut metadata_by_slug: HashMap<String, &ModelsDevModel> = HashMap::new();
        let mut model_ids = provider
            .models
            .iter()
            .filter_map(|(model_key, model)| {
                if model.is_deprecated() || !model.is_picker_enabled() {
                    return None;
                }
                let id = model.resolved_id(model_key)?;
                metadata_by_slug.insert(id.clone(), model);
                Some(id)
            })
            .collect::<Vec<_>>();
        model_ids.sort();

        let mut mapped = self.map_provider_model_ids(model_ids);
        for model in &mut mapped {
            if let Some(metadata) = metadata_by_slug.get(&model.slug)
                && let Some(name) = metadata.name.as_deref()
                && !name.trim().is_empty()
            {
                model.display_name = name.to_string();
            }
        }
        mapped
    }

    fn normalize_provider_key(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut last_was_dash = false;
        for ch in input.chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
                last_was_dash = false;
            } else if !last_was_dash {
                out.push('-');
                last_was_dash = true;
            }
        }
        out.trim_matches('-').to_string()
    }

    fn extract_host_from_url(input: &str) -> Option<String> {
        reqwest::Url::parse(input)
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
    }

    fn ollama_tags_url(base_url: &str) -> String {
        let trimmed = base_url.trim_end_matches('/');
        if let Some(without_v1) = trimmed.strip_suffix("/v1") {
            return format!("{without_v1}/api/tags");
        }
        format!("{trimmed}/api/tags")
    }

    fn map_provider_model_ids(&self, model_ids: Vec<String>) -> Vec<ModelInfo> {
        let mut seen = HashSet::new();
        let bundled_models = Self::load_remote_models_from_file().unwrap_or_default();
        model_ids
            .into_iter()
            .enumerate()
            .filter_map(|(index, model_id)| {
                if !seen.insert(model_id.clone()) {
                    return None;
                }

                let mut candidate = Self::find_metadata_candidate(&model_id, &bundled_models)
                    .unwrap_or_else(|| model_info::model_info_from_slug(&model_id));
                candidate.slug = model_id.clone();
                if candidate.display_name.is_empty() {
                    candidate.display_name = model_id;
                }
                candidate.visibility = ModelVisibility::List;
                candidate.supported_in_api = true;
                candidate.priority = i32::try_from(index).unwrap_or(i32::MAX);
                candidate.used_fallback_model_metadata = false;
                Some(candidate)
            })
            .collect()
    }

    async fn get_etag(&self) -> Option<String> {
        self.etag.read().await.clone()
    }

    fn provider_catalog_is_authoritative(&self) -> bool {
        self.provider.is_github_copilot_provider()
            || self.provider.is_local_oss_provider()
            || !self.provider.is_openai()
    }

    fn cache_scope_key(&self) -> String {
        let auth_mode = self.auth_manager.auth_mode();
        let base_url = self
            .provider
            .to_api_provider(auth_mode)
            .map(|provider| provider.base_url)
            .unwrap_or_else(|_| self.provider.base_url.clone().unwrap_or_default());
        format!(
            "provider_name={};base_url={base_url};auth_mode={auth_mode:?}",
            self.provider.name
        )
    }

    /// Replace the cached remote models and rebuild the derived presets list.
    async fn apply_remote_models(&self, models: Vec<ModelInfo>) {
        let next_models = if self.provider_catalog_is_authoritative() {
            // Copilot/local-OSS/non-OpenAI providers return an authoritative
            // catalog (provider endpoint or models.dev); use it as-is.
            models
        } else {
            let mut existing_models = Self::load_remote_models_from_file().unwrap_or_default();
            for model in models {
                if let Some(existing_index) = existing_models
                    .iter()
                    .position(|existing| existing.slug == model.slug)
                {
                    existing_models[existing_index] = model;
                } else {
                    existing_models.push(model);
                }
            }
            existing_models
        };
        *self.remote_models.write().await = next_models;
    }

    fn load_remote_models_from_file() -> Result<Vec<ModelInfo>, std::io::Error> {
        let file_contents = include_str!("../../models.json");
        let response: ModelsResponse = serde_json::from_str(file_contents)?;
        Ok(response.models)
    }

    /// Attempt to satisfy the refresh from the cache when it matches the provider and TTL.
    async fn try_load_cache(&self) -> bool {
        let _timer =
            codex_otel::start_global_timer("codex.remote_models.load_cache.duration_ms", &[]);
        let client_version = crate::models_manager::client_version_to_whole();
        let provider_scope = self.cache_scope_key();
        info!(client_version, "models cache: evaluating cache eligibility");
        let cache = match self
            .cache_manager
            .load_fresh(&client_version, &provider_scope)
            .await
        {
            Some(cache) => cache,
            None => {
                info!("models cache: no usable cache entry");
                return false;
            }
        };
        let models = cache.models.clone();
        *self.etag.write().await = cache.etag.clone();
        self.apply_remote_models(models.clone()).await;
        info!(
            models_count = models.len(),
            etag = ?cache.etag,
            "models cache: cache entry applied"
        );
        true
    }

    /// Build picker-ready presets from the active catalog snapshot.
    fn build_available_models(&self, mut remote_models: Vec<ModelInfo>) -> Vec<ModelPreset> {
        remote_models.sort_by(|a, b| a.priority.cmp(&b.priority));

        let mut presets: Vec<ModelPreset> = remote_models.into_iter().map(Into::into).collect();
        let chatgpt_mode = matches!(self.auth_manager.auth_mode(), Some(AuthMode::Chatgpt));
        presets = ModelPreset::filter_by_auth(presets, chatgpt_mode);

        ModelPreset::mark_default_by_picker_visibility(&mut presets);

        presets
    }

    async fn get_remote_models(&self) -> Vec<ModelInfo> {
        self.remote_models.read().await.clone()
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        Ok(self.remote_models.try_read()?.clone())
    }

    /// Construct a manager with a specific provider for testing.
    pub(crate) fn with_provider_for_tests(
        codex_home: PathBuf,
        auth_manager: Arc<AuthManager>,
        provider: ModelProviderInfo,
    ) -> Self {
        let cache_path = codex_home.join(MODEL_CACHE_FILE);
        let cache_manager = ModelsCacheManager::new(cache_path, DEFAULT_MODEL_CACHE_TTL);
        let mut remote_models = Self::load_remote_models_from_file()
            .unwrap_or_else(|err| panic!("failed to load bundled models.json: {err}"));
        Self::inject_test_models(&mut remote_models);
        Self {
            remote_models: RwLock::new(remote_models),
            catalog_mode: CatalogMode::Default,
            collaboration_modes_config: CollaborationModesConfig::default(),
            auth_manager,
            etag: RwLock::new(None),
            cache_manager,
            provider,
            models_dev_api_url_override: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_provider_and_models_dev_url_for_tests(
        codex_home: PathBuf,
        auth_manager: Arc<AuthManager>,
        provider: ModelProviderInfo,
        models_dev_api_url: String,
    ) -> Self {
        let mut manager = Self::with_provider_for_tests(codex_home, auth_manager, provider);
        manager.models_dev_api_url_override = Some(models_dev_api_url);
        manager
    }

    fn inject_test_models(models: &mut Vec<ModelInfo>) {
        Self::add_test_model(
            models,
            "gpt-5.1-codex",
            "test-gpt-5.1-codex",
            &["grep_files"],
        );
        Self::add_test_model(models, "gpt-5-codex", "test-gpt-5-codex", &[]);
        Self::add_test_model(models, "gpt-5-codex", "test-gpt-5-remote", &[]);
    }

    fn add_test_model(
        models: &mut Vec<ModelInfo>,
        base_slug: &str,
        test_slug: &str,
        tools: &[&str],
    ) {
        if models.iter().any(|model| model.slug == test_slug) {
            return;
        }

        let Some(base) = models.iter().find(|model| model.slug == base_slug).cloned() else {
            return;
        };

        let mut test_model = base;
        test_model.slug = test_slug.to_string();
        test_model.display_name = test_slug.to_string();
        test_model.description = Some(format!("Test model derived from {base_slug}."));
        test_model.visibility = ModelVisibility::Hide;
        test_model.experimental_supported_tools =
            tools.iter().map(std::string::ToString::to_string).collect();
        models.push(test_model);
    }

    /// Get model identifier without consulting remote state or cache.
    pub(crate) fn get_model_offline_for_tests(model: Option<&str>) -> String {
        if let Some(model) = model {
            return model.to_string();
        }
        let mut models = Self::load_remote_models_from_file().unwrap_or_default();
        models.sort_by(|a, b| a.priority.cmp(&b.priority));
        let presets: Vec<ModelPreset> = models.into_iter().map(Into::into).collect();
        presets
            .iter()
            .find(|preset| preset.show_in_picker)
            .or_else(|| presets.first())
            .map(|preset| preset.model.clone())
            .unwrap_or_default()
    }

    /// Build `ModelInfo` without consulting remote state or cache.
    pub(crate) fn construct_model_info_offline_for_tests(
        model: &str,
        config: &Config,
    ) -> ModelInfo {
        let candidates: &[ModelInfo] = if let Some(model_catalog) = config.model_catalog.as_ref() {
            &model_catalog.models
        } else {
            &[]
        };
        Self::construct_model_info_from_candidates(model, candidates, config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodexAuth;
    use crate::auth::AuthCredentialsStoreMode;
    use crate::config::ConfigBuilder;
    use crate::model_provider_info::WireApi;
    use chrono::Utc;
    use codex_protocol::openai_models::ModelsResponse;
    use core_test_support::responses::mount_models_once;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::tempdir;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    fn remote_model(slug: &str, display: &str, priority: i32) -> ModelInfo {
        remote_model_with_visibility(slug, display, priority, "list")
    }

    fn remote_model_with_visibility(
        slug: &str,
        display: &str,
        priority: i32,
        visibility: &str,
    ) -> ModelInfo {
        serde_json::from_value(json!({
            "slug": slug,
            "display_name": display,
            "description": format!("{display} desc"),
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [{"effort": "low", "description": "low"}, {"effort": "medium", "description": "medium"}],
            "shell_type": "shell_command",
            "visibility": visibility,
            "minimal_client_version": [0, 1, 0],
            "supported_in_api": true,
            "priority": priority,
            "upgrade": null,
            "base_instructions": "base instructions",
            "supports_reasoning_summaries": false,
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": null,
            "truncation_policy": {"mode": "bytes", "limit": 10_000},
            "supports_parallel_tool_calls": false,
            "supports_image_detail_original": false,
            "context_window": 272_000,
            "experimental_supported_tools": [],
        }))
        .expect("valid model")
    }

    fn assert_models_contain(actual: &[ModelInfo], expected: &[ModelInfo]) {
        for model in expected {
            assert!(
                actual.iter().any(|candidate| candidate.slug == model.slug),
                "expected model {} in cached list",
                model.slug
            );
        }
    }

    fn provider_for(base_url: String) -> ModelProviderInfo {
        ModelProviderInfo {
            name: "OpenAI".into(),
            base_url: Some(base_url),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(5_000),
            websocket_connect_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
        }
    }

    #[tokio::test]
    async fn get_model_info_tracks_fallback_usage() {
        let codex_home = tempdir().expect("temp dir");
        let config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let manager = ModelsManager::new(
            codex_home.path().to_path_buf(),
            auth_manager,
            None,
            CollaborationModesConfig::default(),
            ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        );
        let known_slug = manager
            .get_remote_models()
            .await
            .first()
            .expect("bundled models should include at least one model")
            .slug
            .clone();

        let known = manager.get_model_info(known_slug.as_str(), &config).await;
        assert!(!known.used_fallback_model_metadata);
        assert_eq!(known.slug, known_slug);

        let unknown = manager
            .get_model_info("model-that-does-not-exist", &config)
            .await;
        assert!(unknown.used_fallback_model_metadata);
        assert_eq!(unknown.slug, "model-that-does-not-exist");
    }

    #[tokio::test]
    async fn get_model_info_uses_custom_catalog() {
        let codex_home = tempdir().expect("temp dir");
        let config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        let mut overlay = remote_model("gpt-overlay", "Overlay", 0);
        overlay.supports_image_detail_original = true;

        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let manager = ModelsManager::new(
            codex_home.path().to_path_buf(),
            auth_manager,
            Some(ModelsResponse {
                models: vec![overlay],
            }),
            CollaborationModesConfig::default(),
            ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        );

        let model_info = manager
            .get_model_info("gpt-overlay-experiment", &config)
            .await;

        assert_eq!(model_info.slug, "gpt-overlay-experiment");
        assert_eq!(model_info.display_name, "Overlay");
        assert_eq!(model_info.context_window, Some(272_000));
        assert!(model_info.supports_image_detail_original);
        assert!(!model_info.supports_parallel_tool_calls);
        assert!(!model_info.used_fallback_model_metadata);
    }

    #[tokio::test]
    async fn get_model_info_matches_namespaced_suffix() {
        let codex_home = tempdir().expect("temp dir");
        let config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        let mut remote = remote_model("gpt-image", "Image", 0);
        remote.supports_image_detail_original = true;
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let manager = ModelsManager::new(
            codex_home.path().to_path_buf(),
            auth_manager,
            Some(ModelsResponse {
                models: vec![remote],
            }),
            CollaborationModesConfig::default(),
            ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        );
        let namespaced_model = "custom/gpt-image".to_string();

        let model_info = manager.get_model_info(&namespaced_model, &config).await;

        assert_eq!(model_info.slug, namespaced_model);
        assert!(model_info.supports_image_detail_original);
        assert!(!model_info.used_fallback_model_metadata);
    }

    #[tokio::test]
    async fn get_model_info_rejects_multi_segment_namespace_suffix_matching() {
        let codex_home = tempdir().expect("temp dir");
        let config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let manager = ModelsManager::new(
            codex_home.path().to_path_buf(),
            auth_manager,
            None,
            CollaborationModesConfig::default(),
            ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        );
        let known_slug = manager
            .get_remote_models()
            .await
            .first()
            .expect("bundled models should include at least one model")
            .slug
            .clone();
        let namespaced_model = format!("ns1/ns2/{known_slug}");

        let model_info = manager.get_model_info(&namespaced_model, &config).await;

        assert_eq!(model_info.slug, namespaced_model);
        assert!(model_info.used_fallback_model_metadata);
    }

    #[tokio::test]
    async fn refresh_available_models_sorts_by_priority() {
        let server = MockServer::start().await;
        let remote_models = vec![
            remote_model("priority-low", "Low", 1),
            remote_model("priority-high", "High", 0),
        ];
        let models_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: remote_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = provider_for(server.uri());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("refresh succeeds");
        let cached_remote = manager.get_remote_models().await;
        assert_models_contain(&cached_remote, &remote_models);

        let available = manager.list_models(RefreshStrategy::OnlineIfUncached).await;
        let high_idx = available
            .iter()
            .position(|model| model.model == "priority-high")
            .expect("priority-high should be listed");
        let low_idx = available
            .iter()
            .position(|model| model.model == "priority-low")
            .expect("priority-low should be listed");
        assert!(
            high_idx < low_idx,
            "higher priority should be listed before lower priority"
        );
        assert_eq!(
            models_mock.requests().len(),
            1,
            "expected a single /models request"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_uses_cache_when_fresh() {
        let server = MockServer::start().await;
        let remote_models = vec![remote_model("cached", "Cached", 5)];
        let models_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: remote_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = provider_for(server.uri());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("first refresh succeeds");
        assert_models_contain(&manager.get_remote_models().await, &remote_models);

        // Second call should read from cache and avoid the network.
        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("cached refresh succeeds");
        assert_models_contain(&manager.get_remote_models().await, &remote_models);
        assert_eq!(
            models_mock.requests().len(),
            1,
            "cache hit should avoid a second /models request"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_refetches_when_cache_stale() {
        let server = MockServer::start().await;
        let initial_models = vec![remote_model("stale", "Stale", 1)];
        let initial_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: initial_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = provider_for(server.uri());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("initial refresh succeeds");

        // Rewrite cache with an old timestamp so it is treated as stale.
        manager
            .cache_manager
            .manipulate_cache_for_test(|fetched_at| {
                *fetched_at = Utc::now() - chrono::Duration::hours(1);
            })
            .await
            .expect("cache manipulation succeeds");

        let updated_models = vec![remote_model("fresh", "Fresh", 9)];
        server.reset().await;
        let refreshed_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: updated_models.clone(),
            },
        )
        .await;

        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("second refresh succeeds");
        assert_models_contain(&manager.get_remote_models().await, &updated_models);
        assert_eq!(
            initial_mock.requests().len(),
            1,
            "initial refresh should only hit /models once"
        );
        assert_eq!(
            refreshed_mock.requests().len(),
            1,
            "stale cache refresh should fetch /models once"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_refetches_when_version_mismatch() {
        let server = MockServer::start().await;
        let initial_models = vec![remote_model("old", "Old", 1)];
        let initial_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: initial_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = provider_for(server.uri());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("initial refresh succeeds");

        manager
            .cache_manager
            .mutate_cache_for_test(|cache| {
                let client_version = crate::models_manager::client_version_to_whole();
                cache.client_version = Some(format!("{client_version}-mismatch"));
            })
            .await
            .expect("cache mutation succeeds");

        let updated_models = vec![remote_model("new", "New", 2)];
        server.reset().await;
        let refreshed_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: updated_models.clone(),
            },
        )
        .await;

        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("second refresh succeeds");
        assert_models_contain(&manager.get_remote_models().await, &updated_models);
        assert_eq!(
            initial_mock.requests().len(),
            1,
            "initial refresh should only hit /models once"
        );
        assert_eq!(
            refreshed_mock.requests().len(),
            1,
            "version mismatch should fetch /models once"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_drops_removed_remote_models() {
        let server = MockServer::start().await;
        let initial_models = vec![remote_model("remote-old", "Remote Old", 1)];
        let initial_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: initial_models,
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = provider_for(server.uri());
        let mut manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );
        manager.cache_manager.set_ttl(Duration::ZERO);

        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("initial refresh succeeds");

        server.reset().await;
        let refreshed_models = vec![remote_model("remote-new", "Remote New", 1)];
        let refreshed_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: refreshed_models,
            },
        )
        .await;

        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("second refresh succeeds");

        let available = manager
            .try_list_models()
            .expect("models should be available");
        assert!(
            available.iter().any(|preset| preset.model == "remote-new"),
            "new remote model should be listed"
        );
        assert!(
            !available.iter().any(|preset| preset.model == "remote-old"),
            "removed remote model should not be listed"
        );
        assert_eq!(
            initial_mock.requests().len(),
            1,
            "initial refresh should only hit /models once"
        );
        assert_eq!(
            refreshed_mock.requests().len(),
            1,
            "second refresh should only hit /models once"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_skips_network_without_chatgpt_auth() {
        let server = MockServer::start().await;
        let dynamic_slug = "dynamic-model-only-for-test-noauth";
        let models_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: vec![remote_model(dynamic_slug, "No Auth", 1)],
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));
        let provider = provider_for(server.uri());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::Online)
            .await
            .expect("refresh should no-op without chatgpt auth");
        let cached_remote = manager.get_remote_models().await;
        assert!(
            !cached_remote
                .iter()
                .any(|candidate| candidate.slug == dynamic_slug),
            "remote refresh should be skipped without chatgpt auth"
        );
        assert_eq!(
            models_mock.requests().len(),
            0,
            "no auth should avoid /models requests"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_fetches_for_github_copilot_with_stored_token() {
        let server = MockServer::start().await;
        let _models_mock = wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer copilot-test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id": "gpt-4.1"},
                    {"id": "claude-3.7-sonnet"},
                ]
            })))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("copilot-test-token"));
        let mut provider = ModelProviderInfo::create_github_copilot_provider();
        provider.base_url = Some(server.uri());
        // Force fallback to stored auth token instead of environment lookup.
        provider.env_key = Some("__CODEX_TEST_COPILOT_ENV_KEY_MISSING__".to_string());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::Online)
            .await
            .expect("github copilot refresh should succeed");
        let available = manager
            .try_list_models()
            .expect("models should be available");
        assert!(
            available.iter().any(|preset| preset.model == "gpt-4.1"),
            "expected gpt-4.1 to be listed"
        );
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "claude-3.7-sonnet"),
            "expected claude-3.7-sonnet to be listed"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_clears_copilot_catalog_when_token_is_unavailable() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));
        let mut provider = ModelProviderInfo::create_github_copilot_provider();
        provider.env_key = Some("__CODEX_TEST_COPILOT_ENV_KEY_MISSING__".to_string());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("refresh should complete even without token");

        let available = manager
            .try_list_models()
            .expect("models should be available");
        assert!(
            available.is_empty(),
            "expected no github-copilot models without token, got: {:?}",
            available
                .iter()
                .map(|preset| preset.model.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn online_if_uncached_bypasses_cache_for_github_copilot_provider() {
        let server = MockServer::start().await;
        let _models_mock = wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer copilot-test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id": "gpt-4.1"},
                    {"id": "claude-3.7-sonnet"},
                ]
            })))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("copilot-test-token"));
        let mut provider = ModelProviderInfo::create_github_copilot_provider();
        provider.base_url = Some(server.uri());
        // Force fallback to stored auth token instead of environment lookup.
        provider.env_key = Some("__CODEX_TEST_COPILOT_ENV_KEY_MISSING__".to_string());
        let manager = ModelsManager::new(
            codex_home.path().to_path_buf(),
            auth_manager,
            None,
            CollaborationModesConfig::default(),
            provider,
        );
        let bundled_slug_not_in_endpoint = ModelsManager::load_remote_models_from_file()
            .expect("bundled model catalog should load in tests")
            .into_iter()
            .map(|model| model.slug)
            .find(|slug| slug != "gpt-4.1" && slug != "claude-3.7-sonnet")
            .expect("bundled model catalog should contain non-endpoint entries");

        let stale_cache_slug = "cached-openai-only-model-for-copilot-test";
        manager
            .cache_manager
            .persist_cache(
                &[remote_model(stale_cache_slug, "Stale", 0)],
                None,
                crate::models_manager::client_version_to_whole(),
                "provider_name=other;base_url=http://other.invalid;auth_mode=None".to_string(),
            )
            .await;

        let available = manager.list_models(RefreshStrategy::OnlineIfUncached).await;
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "claude-3.7-sonnet"),
            "expected Copilot model to be fetched online"
        );
        assert!(
            !available
                .iter()
                .any(|preset| preset.model == stale_cache_slug),
            "stale cross-provider cache entry should not be used for github-copilot"
        );
        assert!(
            !available
                .iter()
                .any(|preset| preset.model == bundled_slug_not_in_endpoint),
            "github-copilot model list should come from endpoint ids, not bundled catalog"
        );
    }

    /// Verify that the `/models` listing request does NOT send the
    /// `Openai-Intent` header so the server returns all model families
    /// (GPT, Claude, Gemini) rather than filtering to a single intent.
    #[tokio::test]
    async fn copilot_models_request_omits_openai_intent_header_and_returns_all_families() {
        let server = MockServer::start().await;
        let _models_mock = wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer copilot-test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id": "gpt-5.3-codex"},
                    {"id": "gpt-5.1"},
                    {"id": "claude-sonnet-4.5"},
                    {"id": "gemini-2.5-pro"},
                    {"id": "gemini-2.5-flash"},
                ]
            })))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("copilot-test-token"));
        let mut provider = ModelProviderInfo::create_github_copilot_provider();
        provider.base_url = Some(server.uri());
        provider.env_key = Some("__CODEX_TEST_COPILOT_ENV_KEY_MISSING__".to_string());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::Online)
            .await
            .expect("copilot refresh should succeed");

        // Verify the request did NOT include the Openai-Intent header.
        let requests = server
            .received_requests()
            .await
            .expect("should have received requests");
        assert_eq!(requests.len(), 1, "expected exactly one /models request");
        assert!(
            requests[0].headers.get("openai-intent").is_none(),
            "the /models listing request must NOT send the Openai-Intent header; \
             sending it causes the server to filter out model families like Gemini"
        );

        // Verify ALL model families appear in the resulting model list.
        let available = manager
            .try_list_models()
            .expect("models should be available");
        let expected_models = [
            "gpt-5.3-codex",
            "gpt-5.1",
            "claude-sonnet-4.5",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
        ];
        for expected in &expected_models {
            assert!(
                available.iter().any(|preset| preset.model == *expected),
                "expected {expected} in model picker but got: {:?}",
                available.iter().map(|p| &p.model).collect::<Vec<_>>()
            );
        }
    }

    #[tokio::test]
    async fn copilot_models_excludes_entries_disabled_for_picker() {
        let server = MockServer::start().await;
        let _models_mock = wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer copilot-test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id": "gpt-5.1", "model_picker_enabled": true},
                    {"id": "claude-opus-4.6", "model_picker_enabled": false}
                ]
            })))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("copilot-test-token"));
        let mut provider = ModelProviderInfo::create_github_copilot_provider();
        provider.base_url = Some(server.uri());
        provider.env_key = Some("__CODEX_TEST_COPILOT_ENV_KEY_MISSING__".to_string());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::Online)
            .await
            .expect("copilot refresh should succeed");

        let available = manager
            .try_list_models()
            .expect("models should be available");
        assert!(
            available.iter().any(|preset| preset.model == "gpt-5.1"),
            "expected picker-enabled model to be listed"
        );
        assert!(
            !available
                .iter()
                .any(|preset| preset.model == "claude-opus-4.6"),
            "model_picker_enabled=false entries must not be listed"
        );
    }

    #[tokio::test]
    async fn copilot_models_includes_entries_without_responses_endpoint() {
        let server = MockServer::start().await;
        let _models_mock = wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer copilot-test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {
                        "id": "gpt-5.3-codex",
                        "model_picker_enabled": true,
                        "supported_endpoints": ["/responses"]
                    },
                    {
                        "id": "claude-opus-4.6",
                        "model_picker_enabled": true,
                        "supported_endpoints": ["/v1/messages", "/chat/completions"]
                    }
                ]
            })))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("copilot-test-token"));
        let mut provider = ModelProviderInfo::create_github_copilot_provider();
        provider.base_url = Some(server.uri());
        provider.env_key = Some("__CODEX_TEST_COPILOT_ENV_KEY_MISSING__".to_string());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::Online)
            .await
            .expect("copilot refresh should succeed");

        let available = manager
            .try_list_models()
            .expect("models should be available");
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "gpt-5.3-codex"),
            "expected /responses-capable model to be listed"
        );
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "claude-opus-4.6"),
            "chat-completions-only models should still be listed when picker-enabled"
        );
    }

    #[tokio::test]
    async fn get_default_model_falls_back_for_unavailable_copilot_model() {
        let server = MockServer::start().await;
        let _models_mock = wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer copilot-test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {
                        "id": "gpt-5.3-codex",
                        "model_picker_enabled": true,
                        "supported_endpoints": ["/responses"]
                    },
                    {
                        "id": "claude-opus-4.6",
                        "model_picker_enabled": true,
                        "supported_endpoints": ["/v1/messages", "/chat/completions"]
                    }
                ]
            })))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("copilot-test-token"));
        let mut provider = ModelProviderInfo::create_github_copilot_provider();
        provider.base_url = Some(server.uri());
        provider.env_key = Some("__CODEX_TEST_COPILOT_ENV_KEY_MISSING__".to_string());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        let selected = manager
            .get_default_model(
                &Some("claude-opus-4.6".to_string()),
                RefreshStrategy::OnlineIfUncached,
            )
            .await;
        assert_eq!(
            selected, "claude-opus-4.6",
            "requested picker-enabled model should be preserved"
        );
    }

    #[tokio::test]
    async fn get_default_model_falls_back_after_initial_models_refresh() {
        let server = MockServer::start().await;
        let _models_mock = wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer copilot-test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {
                        "id": "gpt-5.3-codex",
                        "model_picker_enabled": true,
                        "supported_endpoints": ["/responses"]
                    },
                    {
                        "id": "claude-opus-4.6",
                        "model_picker_enabled": true,
                        "supported_endpoints": ["/v1/messages", "/chat/completions"]
                    }
                ]
            })))
            .expect(2)
            .mount_as_scoped(&server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("copilot-test-token"));
        let mut provider = ModelProviderInfo::create_github_copilot_provider();
        provider.base_url = Some(server.uri());
        provider.env_key = Some("__CODEX_TEST_COPILOT_ENV_KEY_MISSING__".to_string());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        // Mimic the production startup flow, which refreshes once before selecting the model.
        let _ = manager.list_models(RefreshStrategy::OnlineIfUncached).await;

        let selected = manager
            .get_default_model(
                &Some("claude-opus-4.6".to_string()),
                RefreshStrategy::OnlineIfUncached,
            )
            .await;
        assert_eq!(
            selected, "claude-opus-4.6",
            "requested picker-enabled model should still be preserved after startup refresh"
        );
    }

    #[test]
    fn build_available_models_picks_default_after_hiding_hidden_models() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let provider = provider_for("http://example.test".to_string());
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        let hidden_model = remote_model_with_visibility("hidden", "Hidden", 0, "hide");
        let visible_model = remote_model_with_visibility("visible", "Visible", 1, "list");

        let expected_hidden = ModelPreset::from(hidden_model.clone());
        let mut expected_visible = ModelPreset::from(visible_model.clone());
        expected_visible.is_default = true;

        let available = manager.build_available_models(vec![hidden_model, visible_model]);

        assert_eq!(available, vec![expected_hidden, expected_visible]);
    }

    #[test]
    fn bundled_models_json_roundtrips() {
        let file_contents = include_str!("../../models.json");
        let response: ModelsResponse =
            serde_json::from_str(file_contents).expect("bundled models.json should deserialize");

        let serialized =
            serde_json::to_string(&response).expect("bundled models.json should serialize");
        let roundtripped: ModelsResponse =
            serde_json::from_str(&serialized).expect("serialized models.json should deserialize");

        assert_eq!(
            response, roundtripped,
            "bundled models.json should round trip through serde"
        );
        assert!(
            !response.models.is_empty(),
            "bundled models.json should contain at least one model"
        );
    }

    /// Verify that OSS providers (Ollama / LM Studio) fetch models from
    /// their local `/v1/models` endpoint using the OpenAI-compatible format
    /// and that only the server-returned models appear (no bundled GPT
    /// models mixed in).
    #[tokio::test]
    async fn oss_provider_fetches_models_from_local_server() {
        let server = MockServer::start().await;

        // Ollama's /v1/models returns the OpenAI-compat format.
        let _mock = wiremock::Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [
                    {"id": "smollm2:135m", "object": "model", "created": 1234567890, "owned_by": "library"},
                    {"id": "llama3.2:latest", "object": "model", "created": 1234567891, "owned_by": "library"},
                    {"id": "qwen2.5-coder:7b", "object": "model", "created": 1234567892, "owned_by": "library"},
                ]
            })))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("unused"));
        let provider = crate::model_provider_info::create_oss_provider_with_base_url(
            &format!("{}/v1", server.uri()),
            WireApi::Responses,
        );
        assert!(
            provider.is_local_oss_provider(),
            "test provider should be detected as local OSS"
        );
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("OSS refresh should succeed");

        let available = manager
            .try_list_models()
            .expect("models should be available");

        // All three Ollama models should be present.
        let expected = ["smollm2:135m", "llama3.2:latest", "qwen2.5-coder:7b"];
        for model in &expected {
            assert!(
                available.iter().any(|preset| preset.model == *model),
                "expected {model} in picker but got: {:?}",
                available.iter().map(|p| &p.model).collect::<Vec<_>>()
            );
        }

        // Bundled GPT models should NOT appear (OSS providers replace the
        // entire list with the server response).
        assert!(
            !available
                .iter()
                .any(|preset| preset.model.starts_with("gpt-")),
            "bundled GPT models should not appear for OSS providers; got: {:?}",
            available.iter().map(|p| &p.model).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn oss_provider_falls_back_to_ollama_api_tags() {
        let server = MockServer::start().await;

        let _v1_models = wiremock::Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        let _api_tags = wiremock::Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [
                    {"name": "llama3.2:latest"},
                    {"name": "qwen2.5-coder:7b"},
                ]
            })))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("unused"));
        let provider = crate::model_provider_info::create_oss_provider_with_base_url(
            &format!("{}/v1", server.uri()),
            WireApi::Responses,
        );
        let manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
        );

        manager
            .refresh_available_models(RefreshStrategy::OnlineIfUncached)
            .await
            .expect("OSS refresh should succeed via /api/tags fallback");

        let available = manager
            .try_list_models()
            .expect("models should be available");
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "llama3.2:latest"),
            "expected llama3.2:latest from /api/tags response"
        );
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "qwen2.5-coder:7b"),
            "expected qwen2.5-coder:7b from /api/tags response"
        );
        assert!(
            !available
                .iter()
                .any(|preset| preset.model.starts_with("gpt-")),
            "bundled GPT models should not appear for OSS providers; got: {:?}",
            available.iter().map(|p| &p.model).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn non_openai_provider_ignores_cross_provider_cache_and_uses_models_dev_catalog() {
        let codex_home = tempdir().expect("temp dir");
        let copilot_auth =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("copilot-test-token"));
        let mut copilot_provider = ModelProviderInfo::create_github_copilot_provider();
        copilot_provider.base_url = Some("https://api.githubcopilot.com".to_string());
        let copilot_manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            copilot_auth,
            copilot_provider,
        );

        let leaked_slug = "copilot-cache-only-model";
        copilot_manager
            .cache_manager
            .persist_cache(
                &[remote_model(leaked_slug, "Leaked", 0)],
                None,
                crate::models_manager::client_version_to_whole(),
                copilot_manager.cache_scope_key(),
            )
            .await;

        let models_dev_server = MockServer::start().await;
        let _models_dev = wiremock::Mock::given(method("GET"))
            .and(path("/api.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "azure": {
                    "id": "azure",
                    "name": "Azure",
                    "api": "https://azure.example.com/openai",
                    "env": ["AZURE_OPENAI_API_KEY"],
                    "models": {
                        "azure-model-a": {
                            "id": "azure-model-a",
                            "name": "Azure Model A",
                            "release_date": "2026-01-01",
                            "attachment": false,
                            "reasoning": true,
                            "temperature": true,
                            "tool_call": true,
                            "limit": {"context": 128000, "output": 4096},
                            "options": {}
                        }
                    }
                }
            })))
            .expect(1)
            .mount_as_scoped(&models_dev_server)
            .await;

        let azure_auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("azure-key"));
        let azure_provider = ModelProviderInfo {
            name: "Azure".to_string(),
            base_url: Some("https://azure.example.com/openai".to_string()),
            env_key: Some("AZURE_OPENAI_API_KEY".to_string()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: Some(
                [("api-version".to_string(), "2025-04-01-preview".to_string())]
                    .into_iter()
                    .collect(),
            ),
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(5_000),
            websocket_connect_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
        };
        let azure_manager = ModelsManager::with_provider_and_models_dev_url_for_tests(
            codex_home.path().to_path_buf(),
            azure_auth,
            azure_provider,
            format!("{}/api.json", models_dev_server.uri()),
        );

        let available = azure_manager
            .list_models(RefreshStrategy::OnlineIfUncached)
            .await;
        assert!(
            !available.iter().any(|preset| preset.model == leaked_slug),
            "non-openai providers must ignore cache entries from github-copilot scope"
        );
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "azure-model-a"),
            "expected models.dev-discovered model to be listed"
        );
        assert!(
            !available
                .iter()
                .any(|preset| preset.model.starts_with("gpt-")),
            "non-openai provider should use authoritative models.dev catalog, not bundled OpenAI catalog"
        );
    }

    #[tokio::test]
    async fn models_dev_provider_match_uses_base_url_host_when_name_is_custom() {
        let models_dev_server = MockServer::start().await;
        let _models_dev = wiremock::Mock::given(method("GET"))
            .and(path("/api.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "anthropic": {
                    "id": "anthropic",
                    "name": "Anthropic",
                    "api": "https://api.anthropic.com/v1",
                    "env": ["ANTHROPIC_API_KEY"],
                    "models": {
                        "claude-host-match": {
                            "id": "claude-host-match",
                            "name": "Claude Host Match",
                            "release_date": "2026-01-01",
                            "attachment": false,
                            "reasoning": true,
                            "temperature": true,
                            "tool_call": true,
                            "limit": {"context": 200000, "output": 4096},
                            "options": {}
                        }
                    }
                }
            })))
            .expect(1)
            .mount_as_scoped(&models_dev_server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("unused"));
        let provider = ModelProviderInfo {
            name: "Internal Anthropic Proxy".to_string(),
            base_url: Some("https://api.anthropic.com/v1".to_string()),
            env_key: Some("ANTHROPIC_API_KEY".to_string()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(5_000),
            websocket_connect_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
        };
        let manager = ModelsManager::with_provider_and_models_dev_url_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
            format!("{}/api.json", models_dev_server.uri()),
        );

        let available = manager.list_models(RefreshStrategy::OnlineIfUncached).await;
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "claude-host-match"),
            "expected provider host fallback to match models.dev provider"
        );
    }

    #[tokio::test]
    async fn models_dev_provider_match_accepts_azure_openai_alias() {
        let models_dev_server = MockServer::start().await;
        let _models_dev = wiremock::Mock::given(method("GET"))
            .and(path("/api.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "azure": {
                    "id": "azure",
                    "name": "Azure",
                    "models": {
                        "azure-model-a": {
                            "id": "azure-model-a",
                            "name": "Azure Model A",
                            "release_date": "2026-01-01",
                            "attachment": false,
                            "reasoning": true,
                            "temperature": true,
                            "tool_call": true,
                            "limit": {"context": 128000, "output": 4096},
                            "options": {}
                        }
                    }
                }
            })))
            .expect(1)
            .mount_as_scoped(&models_dev_server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("unused"));
        let provider = ModelProviderInfo {
            name: "Azure OpenAI".to_string(),
            base_url: Some("http://127.0.0.1:9/openai".to_string()),
            env_key: Some("AZURE_OPENAI_API_KEY".to_string()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: Some(
                [("api-version".to_string(), "2025-04-01-preview".to_string())]
                    .into_iter()
                    .collect(),
            ),
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(5_000),
            websocket_connect_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
        };
        let manager = ModelsManager::with_provider_and_models_dev_url_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
            format!("{}/api.json", models_dev_server.uri()),
        );

        let available = manager.list_models(RefreshStrategy::OnlineIfUncached).await;
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "azure-model-a"),
            "expected Azure OpenAI alias to match models.dev Azure provider"
        );
    }

    #[tokio::test]
    async fn non_openai_provider_falls_back_to_provider_models_when_models_dev_has_no_match() {
        let models_dev_server = MockServer::start().await;
        let _models_dev = wiremock::Mock::given(method("GET"))
            .and(path("/api.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "unrelated": {
                    "id": "unrelated",
                    "name": "Unrelated Provider",
                    "api": "https://unrelated.example.com/v1",
                    "env": ["UNRELATED_KEY"],
                    "models": {}
                }
            })))
            .expect(1)
            .mount_as_scoped(&models_dev_server)
            .await;

        let provider_server = MockServer::start().await;
        let _provider_models = wiremock::Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id": "provider-fallback-a"},
                    {"id": "provider-fallback-b"}
                ]
            })))
            .expect(1)
            .mount_as_scoped(&provider_server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("provider-token"));
        let provider = ModelProviderInfo {
            name: "Custom Provider".to_string(),
            base_url: Some(format!("{}/v1", provider_server.uri())),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(5_000),
            websocket_connect_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
        };
        let manager = ModelsManager::with_provider_and_models_dev_url_for_tests(
            codex_home.path().to_path_buf(),
            auth_manager,
            provider,
            format!("{}/api.json", models_dev_server.uri()),
        );

        let available = manager.list_models(RefreshStrategy::OnlineIfUncached).await;
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "provider-fallback-a"),
            "expected fallback to provider /models endpoint when models.dev has no match"
        );
        assert!(
            !available
                .iter()
                .any(|preset| preset.model.starts_with("gpt-")),
            "provider-authoritative fallback should still avoid bundled OpenAI model bleed"
        );
    }

    #[tokio::test]
    async fn local_oss_provider_ignores_cache_from_other_provider() {
        let codex_home = tempdir().expect("temp dir");
        let openai_auth =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let openai_manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            openai_auth,
            provider_for("https://api.openai.com/v1".to_string()),
        );

        let leaked_slug = "openai-cache-only-model";
        openai_manager
            .cache_manager
            .persist_cache(
                &[remote_model(leaked_slug, "Leaked", 0)],
                None,
                crate::models_manager::client_version_to_whole(),
                openai_manager.cache_scope_key(),
            )
            .await;

        let server = MockServer::start().await;
        let _mock = wiremock::Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id": "qwen2.5-coder:7b"}
                ]
            })))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let oss_auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("unused"));
        let oss_provider = crate::model_provider_info::create_oss_provider_with_base_url(
            &format!("{}/v1", server.uri()),
            WireApi::Responses,
        );
        let oss_manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            oss_auth,
            oss_provider,
        );

        let available = oss_manager
            .list_models(RefreshStrategy::OnlineIfUncached)
            .await;
        assert!(
            !available.iter().any(|preset| preset.model == leaked_slug),
            "local OSS provider must ignore cache entries from non-OSS provider scopes"
        );
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "qwen2.5-coder:7b"),
            "local OSS provider should refresh from /v1/models when cache scope mismatches"
        );
    }

    #[tokio::test]
    async fn local_oss_provider_does_not_fall_back_to_bundled_models_when_fetch_fails() {
        let server = MockServer::start().await;
        let _v1_models = wiremock::Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        let _api_tags = wiremock::Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let codex_home = tempdir().expect("temp dir");
        let oss_auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("unused"));
        let oss_provider = crate::model_provider_info::create_oss_provider_with_base_url(
            &format!("{}/v1", server.uri()),
            WireApi::Responses,
        );
        let oss_manager = ModelsManager::with_provider_for_tests(
            codex_home.path().to_path_buf(),
            oss_auth,
            oss_provider,
        );

        let available = oss_manager
            .list_models(RefreshStrategy::OnlineIfUncached)
            .await;
        assert!(
            available.is_empty(),
            "local OSS providers must not expose bundled fallback models when discovery fails; got: {:?}",
            available
                .iter()
                .map(|preset| &preset.model)
                .collect::<Vec<_>>()
        );
    }
}
