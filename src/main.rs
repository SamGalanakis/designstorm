use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use askama::Template;
use axum::{
    Router,
    body::Body,
    extract::{Json, Path, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CONTENT_TYPE, COOKIE, SET_COOKIE},
    },
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cookie::{Cookie, SameSite};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use lash_core::{
    AgentCapabilities, AgentStateEnvelope, HostProfile, InputItem, PromptOverrideMode,
    PromptSectionName, PromptSectionOverride, Provider, RuntimeConfig, RuntimeEngine, ToolDefinition,
    ToolParam, ToolProvider, ToolResult, TurnInput, oauth,
    provider::OPENAI_GENERIC_DEFAULT_BASE_URL,
    tools::{CompositeTools, FetchUrl, WebSearch},
};
use mime_guess::from_path;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, postgres::PgPoolOptions};
use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    path::{Component, Path as StdPath, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::info;
use uuid::Uuid;

const SESSION_COOKIE_NAME: &str = "designstorm_session";
const DATASTAR_CDN: &str =
    "https://cdn.jsdelivr.net/gh/starfederation/datastar@main/bundles/datastar.js";

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    db: PgPool,
    http: Client,
    session_encoding_key: EncodingKey,
    session_decoding_key: DecodingKey,
    clerk_decoding_key: DecodingKey,
    storm_runs: Arc<RwLock<HashMap<Uuid, StormRunRecord>>>,
}

#[derive(Clone)]
struct Config {
    port: u16,
    database_url: String,
    clerk_publishable_key: String,
    clerk_secret_key: Option<String>,
    clerk_issuer: String,
    clerk_jwks_url: String,
    clerk_jwt_public_key: String,
    session_secret: String,
    app_url: String,
    aws_endpoint_url_s3: Option<String>,
    aws_region: Option<String>,
    bucket_name: Option<String>,
    tavily_api_key: Option<String>,
    storm_model: Option<String>,
    openai_generic_api_key: Option<String>,
    openai_generic_base_url: String,
    workspace_root: PathBuf,
}

impl Config {
    fn from_env() -> Result<Self, AppError> {
        let port = env::var("PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(8080);

        Ok(Self {
            port,
            database_url: required_env("DATABASE_URL")?,
            clerk_publishable_key: required_env("CLERK_PUBLISHABLE_KEY")?,
            clerk_secret_key: env::var("CLERK_SECRET_KEY").ok(),
            clerk_issuer: required_env("CLERK_ISSUER")?,
            clerk_jwks_url: required_env("CLERK_JWKS_URL")?,
            clerk_jwt_public_key: required_env("CLERK_JWT_PUBLIC_KEY")?,
            session_secret: required_env("SESSION_SECRET")?,
            app_url: env::var("APP_URL").unwrap_or_else(|_| "http://localhost:8080".to_string()),
            aws_endpoint_url_s3: env::var("AWS_ENDPOINT_URL_S3").ok(),
            aws_region: env::var("AWS_REGION").ok(),
            bucket_name: env::var("BUCKET_NAME").ok(),
            tavily_api_key: env::var("TAVILY_API_KEY").ok(),
            storm_model: env::var("DESIGNSTORM_MODEL").ok(),
            openai_generic_api_key: env::var("OPENAI_GENERIC_API_KEY")
                .or_else(|_| env::var("OPENROUTER_API_KEY"))
                .ok(),
            openai_generic_base_url: env::var("OPENAI_GENERIC_BASE_URL")
                .unwrap_or_else(|_| OPENAI_GENERIC_DEFAULT_BASE_URL.to_string()),
            workspace_root: env::var("DESIGNSTORM_WORKSPACE_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|_| env::temp_dir().join("designstorm")),
        })
    }
}

#[derive(Debug, Error)]
enum AppError {
    #[error("Missing environment variable: {0}")]
    MissingEnv(String),
    #[error("Template render failed: {0}")]
    Template(#[from] askama::Error),
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("HTTP client error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JWT error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            AppError::MissingEnv(_)
            | AppError::Template(_)
            | AppError::Database(_)
            | AppError::Migration(_)
            | AppError::Io(_)
            | AppError::Serde(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Http(_) | AppError::Jwt(_) => StatusCode::UNAUTHORIZED,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let message = self.to_string();
        (
            status,
            [(
                CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            message,
        )
            .into_response()
    }
}

#[derive(Debug, Clone, Serialize)]
struct Viewer {
    id: Uuid,
    clerk_user_id: String,
    email: Option<String>,
    name: String,
    avatar_url: Option<String>,
    created_at: DateTime<Utc>,
}

impl Viewer {
    fn secondary_line(&self) -> &str {
        self.email.as_deref().unwrap_or("Ready to design storm.")
    }
}

#[derive(Debug, Deserialize)]
struct SessionPayload {
    token: String,
}

#[derive(Debug, Serialize)]
struct AuthState {
    authenticated: bool,
    user: Option<Viewer>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionClaims {
    sub: String,
    clerk_user_id: String,
    exp: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct ClerkClaims {
    sub: String,
    iss: Option<String>,
    exp: usize,
    email: Option<String>,
    name: Option<String>,
    image_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClerkUser {
    primary_email_address_id: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    image_url: Option<String>,
    email_addresses: Vec<ClerkEmailAddress>,
}

#[derive(Debug, Deserialize)]
struct ClerkEmailAddress {
    id: String,
    email_address: String,
}

impl ClerkUser {
    fn primary_email(&self) -> Option<&str> {
        let primary_id = self.primary_email_address_id.as_ref()?;
        self.email_addresses
            .iter()
            .find(|address| &address.id == primary_id)
            .map(|address| address.email_address.as_str())
    }

    fn full_name(&self) -> Option<String> {
        match (&self.first_name, &self.last_name) {
            (Some(first), Some(last)) => Some(format!("{first} {last}")),
            (Some(first), None) => Some(first.clone()),
            (None, Some(last)) => Some(last.clone()),
            (None, None) => None,
        }
    }
}

#[derive(Debug, FromRow)]
struct UserRow {
    id: Uuid,
    clerk_user_id: String,
    email: Option<String>,
    name: Option<String>,
    avatar_url: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, FromRow)]
struct ProviderCredentialRow {
    encrypted_config: String,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, FromRow)]
struct CodexDeviceAuthRow {
    device_auth_id: String,
    user_code: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredProviderConfig {
    provider: Provider,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderStatusView {
    connected: bool,
    using_fallback: bool,
    label: String,
    detail: String,
    updated_label: String,
}

#[derive(Debug, Clone)]
struct LoadedProvider {
    provider: Provider,
    source: ProviderSource,
    updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy)]
enum ProviderSource {
    Stored,
    ServerFallback,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StormRunSummary {
    id: Uuid,
    prompt: String,
    title: String,
    summary: String,
    assistant_summary: String,
    preview_url: String,
    submitted: bool,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct StormRunRecord {
    id: Uuid,
    owner_user_id: Uuid,
    prompt: String,
    title: String,
    summary: String,
    assistant_summary: String,
    preview_url: String,
    submitted: bool,
    created_at: DateTime<Utc>,
    workspace_dir: PathBuf,
}

impl StormRunRecord {
    fn summary_view(&self) -> StormRunSummary {
        StormRunSummary {
            id: self.id,
            prompt: self.prompt.clone(),
            title: self.title.clone(),
            summary: self.summary.clone(),
            assistant_summary: self.assistant_summary.clone(),
            preview_url: self.preview_url.clone(),
            submitted: self.submitted,
            created_at: self.created_at,
        }
    }
}

#[derive(Debug)]
struct WorkspaceSnapshot {
    run_id: Uuid,
    workspace_dir: PathBuf,
    prompt: String,
    title: String,
    summary: String,
    submitted: bool,
}

#[derive(Debug)]
struct WorkspaceRuntimeState {
    run_id: Uuid,
    preview_url: String,
    workspace_dir: PathBuf,
    prompt: String,
    title: String,
    summary: String,
    submitted: bool,
}

impl WorkspaceRuntimeState {
    fn snapshot(&self) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            run_id: self.run_id,
            workspace_dir: self.workspace_dir.clone(),
            prompt: self.prompt.clone(),
            title: self.title.clone(),
            summary: self.summary.clone(),
            submitted: self.submitted,
        }
    }
}

#[derive(Clone)]
struct StormRuntimeCtx {
    provider: Provider,
    model: String,
    tavily_api_key: Option<String>,
}

#[derive(Clone, Copy)]
enum StormAgentRole {
    Root,
    Researcher,
    Renderer,
    Critic,
}

impl StormAgentRole {
    fn from_str(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "researcher" => Some(Self::Researcher),
            "renderer" => Some(Self::Renderer),
            "critic" => Some(Self::Critic),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Researcher => "researcher",
            Self::Renderer => "renderer",
            Self::Critic => "critic",
        }
    }

    fn prompt_identity(self) -> &'static str {
        match self {
            Self::Root => {
                "You are Design Storm, an AI art-direction runtime that creates bold design language documents as static HTML artifacts."
            }
            Self::Researcher => {
                "You are the research subagent for Design Storm. Pull references, extract signals, and hand back concise visual direction."
            }
            Self::Renderer => {
                "You are the renderer subagent for Design Storm. Turn the design thesis into a strong static HTML artifact inside the workspace."
            }
            Self::Critic => {
                "You are the critic subagent for Design Storm. Inspect the artifact, identify sameness or weak choices, and suggest sharper revisions."
            }
        }
    }
}

struct StormToolProvider {
    allow_subagents: bool,
    workspace: Arc<Mutex<WorkspaceRuntimeState>>,
    runtime: StormRuntimeCtx,
}

impl StormToolProvider {
    fn new(
        _role: StormAgentRole,
        allow_subagents: bool,
        workspace: Arc<Mutex<WorkspaceRuntimeState>>,
        runtime: StormRuntimeCtx,
    ) -> Self {
        Self {
            allow_subagents,
            workspace,
            runtime,
        }
    }

    fn logical_path(&self, args: &serde_json::Value, key: &str) -> Result<String, ToolResult> {
        args.get(key)
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| ToolResult::err_fmt(format_args!("Missing required parameter: {key}")))
    }

    async fn workspace_list(&self) -> ToolResult {
        let workspace_dir = { self.workspace.lock().await.workspace_dir.clone() };
        let mut stack = vec![workspace_dir];
        let mut items = Vec::new();

        while let Some(dir) = stack.pop() {
            let mut entries = match tokio::fs::read_dir(&dir).await {
                Ok(entries) => entries,
                Err(error) => return ToolResult::err_fmt(format_args!("Failed to list workspace: {error}")),
            };
            loop {
                match entries.next_entry().await {
                    Ok(Some(entry)) => {
                        let path = entry.path();
                        let metadata = match entry.metadata().await {
                            Ok(metadata) => metadata,
                            Err(error) => {
                                return ToolResult::err_fmt(format_args!(
                                    "Failed to stat {}: {error}",
                                    path.display()
                                ));
                            }
                        };
                        let relative = match path.strip_prefix(
                            &self.workspace.lock().await.workspace_dir,
                        ) {
                            Ok(relative) => relative.to_string_lossy().to_string(),
                            Err(_) => continue,
                        };
                        if metadata.is_dir() {
                            stack.push(path);
                            items.push(json!({"path": relative, "kind": "dir"}));
                        } else {
                            items.push(json!({
                                "path": relative,
                                "kind": "file",
                                "sizeBytes": metadata.len()
                            }));
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        return ToolResult::err_fmt(format_args!("Failed to iterate workspace: {error}"));
                    }
                }
            }
        }

        items.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
        ToolResult::ok(json!({ "items": items }))
    }

    async fn workspace_read(&self, args: &serde_json::Value) -> ToolResult {
        let path = match self.logical_path(args, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let resolved = {
            let workspace = self.workspace.lock().await;
            match resolve_workspace_path(&workspace.workspace_dir, &path) {
                Ok(path) => path,
                Err(message) => return ToolResult::err_fmt(message),
            }
        };

        match tokio::fs::read_to_string(&resolved).await {
            Ok(content) => ToolResult::ok(json!({ "path": path, "content": content })),
            Err(error) => ToolResult::err_fmt(format_args!("Failed to read {}: {error}", resolved.display())),
        }
    }

    async fn workspace_write(&self, args: &serde_json::Value) -> ToolResult {
        let path = match self.logical_path(args, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let content = match args.get("content").and_then(|value| value.as_str()) {
            Some(content) => content,
            None => return ToolResult::err_fmt("Missing required parameter: content"),
        };
        let resolved = {
            let workspace = self.workspace.lock().await;
            match resolve_workspace_path(&workspace.workspace_dir, &path) {
                Ok(path) => path,
                Err(message) => return ToolResult::err_fmt(message),
            }
        };

        if let Some(parent) = resolved.parent()
            && let Err(error) = tokio::fs::create_dir_all(parent).await
        {
            return ToolResult::err_fmt(format_args!(
                "Failed to create directory {}: {error}",
                parent.display()
            ));
        }

        match tokio::fs::write(&resolved, content).await {
            Ok(()) => ToolResult::ok(json!({
                "path": path,
                "bytesWritten": content.len()
            })),
            Err(error) => ToolResult::err_fmt(format_args!("Failed to write {}: {error}", resolved.display())),
        }
    }

    async fn render_result(&self) -> ToolResult {
        let workspace = self.workspace.lock().await;
        let index_path = workspace.workspace_dir.join("index.html");
        let styles_path = workspace.workspace_dir.join("styles.css");
        let has_index = tokio::fs::try_exists(&index_path).await.unwrap_or(false);
        let has_styles = tokio::fs::try_exists(&styles_path).await.unwrap_or(false);
        if !has_index {
            return ToolResult::err(json!("index.html does not exist yet"));
        }

        ToolResult::ok(json!({
            "previewUrl": workspace.preview_url,
            "hasIndex": has_index,
            "hasStyles": has_styles
        }))
    }

    async fn view_result(&self) -> ToolResult {
        let workspace = self.workspace.lock().await;
        let index_path = workspace.workspace_dir.join("index.html");
        let styles_path = workspace.workspace_dir.join("styles.css");
        let html = tokio::fs::read_to_string(&index_path)
            .await
            .unwrap_or_default();
        let css = tokio::fs::read_to_string(&styles_path)
            .await
            .unwrap_or_default();

        ToolResult::ok(json!({
            "previewUrl": workspace.preview_url,
            "htmlExcerpt": truncate_for_tool(&html, 2400),
            "cssExcerpt": truncate_for_tool(&css, 1800),
            "title": workspace.title,
            "summary": workspace.summary
        }))
    }

    async fn submit_result(&self, args: &serde_json::Value) -> ToolResult {
        let title = args
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or("Storm Artifact")
            .trim()
            .to_string();
        let summary = args
            .get("summary")
            .and_then(|value| value.as_str())
            .unwrap_or("Design language document ready.")
            .trim()
            .to_string();
        let mut workspace = self.workspace.lock().await;
        workspace.title = if title.is_empty() {
            "Storm Artifact".to_string()
        } else {
            title
        };
        workspace.summary = if summary.is_empty() {
            "Design language document ready.".to_string()
        } else {
            summary
        };
        workspace.submitted = true;
        ToolResult::ok(json!({
            "previewUrl": workspace.preview_url,
            "title": workspace.title,
            "summary": workspace.summary
        }))
    }

    async fn spawn_subagent(&self, args: &serde_json::Value) -> ToolResult {
        if !self.allow_subagents {
            return ToolResult::err(json!("spawn_subagent is disabled for this agent"));
        }

        let role = match args.get("role").and_then(|value| value.as_str()) {
            Some(value) => match StormAgentRole::from_str(value) {
                Some(role) => role,
                None => {
                    return ToolResult::err(json!(
                        "Invalid role. Expected researcher, renderer, or critic."
                    ));
                }
            },
            None => return ToolResult::err(json!("Missing required parameter: role")),
        };
        let prompt = match args.get("prompt").and_then(|value| value.as_str()) {
            Some(prompt) if !prompt.trim().is_empty() => prompt.trim().to_string(),
            _ => return ToolResult::err(json!("Missing required parameter: prompt")),
        };

        let result = match run_design_agent(
            role,
            false,
            self.workspace.clone(),
            self.runtime.clone(),
            prompt,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => return ToolResult::err_fmt(error),
        };

        ToolResult::ok(json!({
            "role": role.label(),
            "output": result.assistant_output.safe_text
        }))
    }
}

#[async_trait::async_trait]
impl ToolProvider for StormToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = vec![
            ToolDefinition {
                name: "workspace_list".into(),
                description: vec![lash_core::ToolText::new(
                    "List the current artifact workspace. Use this to inspect available files before reading or editing them.",
                    [lash_core::ExecutionMode::NativeTools],
                )],
                params: vec![],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
            ToolDefinition {
                name: "workspace_read".into(),
                description: vec![lash_core::ToolText::new(
                    "Read a UTF-8 text file from the current artifact workspace. Paths must stay inside the workspace.",
                    [lash_core::ExecutionMode::NativeTools],
                )],
                params: vec![ToolParam::typed("path", "str")],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
            ToolDefinition {
                name: "workspace_write".into(),
                description: vec![lash_core::ToolText::new(
                    "Create or overwrite a text file inside the current artifact workspace. Use this to author index.html, styles.css, and supporting docs.",
                    [lash_core::ExecutionMode::NativeTools],
                )],
                params: vec![ToolParam::typed("path", "str"), ToolParam::typed("content", "str")],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
            ToolDefinition {
                name: "render_result".into(),
                description: vec![lash_core::ToolText::new(
                    "Validate that the current workspace can be previewed and return the preview URL.",
                    [lash_core::ExecutionMode::NativeTools],
                )],
                params: vec![],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
            ToolDefinition {
                name: "view_result".into(),
                description: vec![lash_core::ToolText::new(
                    "Inspect the current artifact result as structured excerpts plus the preview URL. Use this after rendering to critique or refine the output.",
                    [lash_core::ExecutionMode::NativeTools],
                )],
                params: vec![],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
            ToolDefinition {
                name: "submit_result".into(),
                description: vec![lash_core::ToolText::new(
                    "Mark the current artifact as the candidate output for this storm run. Call this after you are satisfied with the HTML result.",
                    [lash_core::ExecutionMode::NativeTools],
                )],
                params: vec![
                    ToolParam::optional("title", "str"),
                    ToolParam::optional("summary", "str"),
                ],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
        ];

        if self.allow_subagents {
            defs.push(ToolDefinition {
                name: "spawn_subagent".into(),
                description: vec![lash_core::ToolText::new(
                    "Run a focused subagent and get its answer back synchronously. Use role=\"researcher\" for reference digging, role=\"renderer\" for HTML-focused refinement, and role=\"critic\" for critique.",
                    [lash_core::ExecutionMode::NativeTools],
                )],
                params: vec![ToolParam::typed("role", "str"), ToolParam::typed("prompt", "str")],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            });
        }

        defs
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolResult {
        match name {
            "workspace_list" => self.workspace_list().await,
            "workspace_read" => self.workspace_read(args).await,
            "workspace_write" => self.workspace_write(args).await,
            "render_result" => self.render_result().await,
            "view_result" => self.view_result().await,
            "submit_result" => self.submit_result(args).await,
            "spawn_subagent" => self.spawn_subagent(args).await,
            _ => ToolResult::err_fmt(format_args!("Unknown tool: {name}")),
        }
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate<'a> {
    title: &'a str,
    body_class: &'a str,
    datastar_cdn: &'a str,
    auth_panel: &'a str,
    app_config_json: &'a str,
}

#[derive(Template)]
#[template(path = "app.html")]
struct AppTemplate<'a> {
    title: &'a str,
    body_class: &'a str,
    datastar_cdn: &'a str,
    viewer: &'a Viewer,
    app_config_json: &'a str,
    provider_panel: &'a str,
}

#[derive(Template)]
#[template(path = "auth_panel.html")]
struct AuthPanelTemplate<'a> {
    viewer: Option<&'a Viewer>,
}

#[derive(Template)]
#[template(path = "provider_panel.html")]
struct ProviderPanelTemplate<'a> {
    status: &'a ProviderStatusView,
}

#[derive(Debug, Deserialize)]
struct StormRequest {
    prompt: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StormResponse {
    run: StormRunSummary,
    assistant_summary: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CodexStartResponse {
    verify_url: &'static str,
    user_code: String,
    interval_seconds: i32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CodexPollResponse {
    status: &'static str,
    message: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    dotenvy::dotenv().ok();
    init_tracing();

    let config = Arc::new(Config::from_env()?);
    tokio::fs::create_dir_all(&config.workspace_root).await?;

    let db = PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await?;

    sqlx::migrate!("./migrations").run(&db).await?;

    let state = AppState {
        db,
        http: Client::new(),
        session_encoding_key: EncodingKey::from_secret(config.session_secret.as_bytes()),
        session_decoding_key: DecodingKey::from_secret(config.session_secret.as_bytes()),
        clerk_decoding_key: DecodingKey::from_rsa_pem(config.clerk_jwt_public_key.as_bytes())?,
        storm_runs: Arc::new(RwLock::new(HashMap::new())),
        config,
    };

    info!(
        clerk_issuer = %state.config.clerk_issuer,
        clerk_jwks_url = %state.config.clerk_jwks_url,
        bucket_name = ?state.config.bucket_name,
        aws_region = ?state.config.aws_region,
        aws_endpoint_url_s3 = ?state.config.aws_endpoint_url_s3,
        has_tavily = state.config.tavily_api_key.is_some(),
        workspace_root = %state.config.workspace_root.display(),
        "loaded configuration"
    );

    let router = Router::new()
        .route("/", get(index))
        .route("/app", get(app_page))
        .route("/healthz", get(healthz))
        .route("/auth/me", get(auth_me))
        .route("/auth/session", post(create_session))
        .route("/auth/logout", post(logout))
        .route("/partials/auth-panel", get(auth_panel))
        .route("/settings/provider", get(provider_panel))
        .route("/settings/provider/codex/start", post(start_codex_auth))
        .route("/settings/provider/codex/poll", post(poll_codex_auth))
        .route("/settings/provider/logout", post(disconnect_provider))
        .route("/api/storms", get(list_storms).post(create_storm))
        .route("/preview/{run_id}", get(preview_index))
        .route("/preview/{run_id}/*path", get(preview_asset))
        .nest_service("/static", ServeDir::new("static"))
        .nest_service("/docs", ServeDir::new("docs"))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let address = SocketAddr::from(([0, 0, 0, 0], state.config.port));
    info!("listening on {}", address);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| AppError::Internal(error.to_string()))?;
    axum::serve(listener, router)
        .await
        .map_err(|error| AppError::Internal(error.to_string()))?;

    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let viewer = current_viewer(&state, &headers).await?;
    let auth_panel = AuthPanelTemplate {
        viewer: viewer.as_ref(),
    }
    .render()?;
    let config_json = json!({
        "clerkPublishableKey": state.config.clerk_publishable_key,
        "appUrl": state.config.app_url,
        "hasServerSession": viewer.is_some(),
        "currentPath": "/",
    })
    .to_string();

    let page = IndexTemplate {
        title: "Design Storm",
        body_class: "landing-page",
        datastar_cdn: DATASTAR_CDN,
        auth_panel: &auth_panel,
        app_config_json: &config_json,
    };

    Ok(Html(page.render()?))
}

async fn app_page(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    let Some(viewer) = current_viewer(&state, &headers).await? else {
        return Ok(Redirect::temporary("/").into_response());
    };

    let provider_panel = render_provider_panel_html(&state, &viewer).await?;
    let config_json = json!({
        "clerkPublishableKey": state.config.clerk_publishable_key,
        "appUrl": state.config.app_url,
        "hasServerSession": true,
        "currentPath": "/app",
    })
    .to_string();

    let page = AppTemplate {
        title: "Design Storm / Stormboard",
        body_class: "app-page",
        datastar_cdn: DATASTAR_CDN,
        viewer: &viewer,
        app_config_json: &config_json,
        provider_panel: &provider_panel,
    };

    Ok(Html(page.render()?).into_response())
}

async fn auth_panel(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let viewer = current_viewer(&state, &headers).await?;
    let panel = AuthPanelTemplate {
        viewer: viewer.as_ref(),
    };
    Ok(Html(panel.render()?))
}

async fn provider_panel(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    Ok(Html(render_provider_panel_html(&state, &viewer).await?))
}

async fn auth_me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthState>, AppError> {
    let viewer = current_viewer(&state, &headers).await?;
    Ok(Json(AuthState {
        authenticated: viewer.is_some(),
        user: viewer,
    }))
}

async fn create_session(
    State(state): State<AppState>,
    Json(payload): Json<SessionPayload>,
) -> Result<Response, AppError> {
    if payload.token.trim().is_empty() {
        return Err(AppError::BadRequest("Missing Clerk token".to_string()));
    }

    let claims = verify_clerk_token(&state, &payload.token)?;
    let clerk_user = fetch_clerk_user(&state, &claims.sub).await.ok();

    let email = clerk_user
        .as_ref()
        .and_then(ClerkUser::primary_email)
        .map(ToString::to_string)
        .or(claims.email.clone());
    let name = clerk_user
        .as_ref()
        .and_then(ClerkUser::full_name)
        .or(claims.name.clone())
        .unwrap_or_else(|| "Design Storm User".to_string());
    let avatar_url = clerk_user
        .and_then(|user| user.image_url)
        .or(claims.image_url.clone());

    let user = upsert_user(&state.db, &claims.sub, email, name, avatar_url).await?;
    let cookie = session_cookie(&state, &user)?;

    let mut response = Json(json!({
        "ok": true,
        "redirect": "/app"
    }))
    .into_response();
    response.headers_mut().append(
        SET_COOKIE,
        HeaderValue::from_str(&cookie.to_string())
            .map_err(|error| AppError::Internal(error.to_string()))?,
    );

    Ok(response)
}

async fn logout() -> Result<Response, AppError> {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().append(
        SET_COOKIE,
        HeaderValue::from_str(&cleared_session_cookie().to_string())
            .map_err(|error| AppError::Internal(error.to_string()))?,
    );
    Ok(response)
}

async fn start_codex_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CodexStartResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let device = oauth::codex_request_device_code()
        .await
        .map_err(|error| AppError::Internal(error.to_string()))?;

    sqlx::query(
        r#"
        INSERT INTO codex_device_auth_sessions (user_id, device_auth_id, user_code, interval_seconds)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (user_id) DO UPDATE
        SET device_auth_id = EXCLUDED.device_auth_id,
            user_code = EXCLUDED.user_code,
            interval_seconds = EXCLUDED.interval_seconds,
            updated_at = now()
        "#,
    )
    .bind(viewer.id)
    .bind(&device.device_auth_id)
    .bind(&device.user_code)
    .bind(device.interval as i32)
    .execute(&state.db)
    .await?;

    Ok(Json(CodexStartResponse {
        verify_url: oauth::CODEX_DEVICE_VERIFY_URL,
        user_code: device.user_code,
        interval_seconds: device.interval as i32,
    }))
}

async fn poll_codex_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CodexPollResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let pending = sqlx::query_as::<_, CodexDeviceAuthRow>(
        r#"
        SELECT device_auth_id, user_code
        FROM codex_device_auth_sessions
        WHERE user_id = $1
        "#,
    )
    .bind(viewer.id)
    .fetch_optional(&state.db)
    .await?;

    let Some(pending) = pending else {
        return Ok(Json(CodexPollResponse {
            status: "idle",
            message: None,
        }));
    };

    match oauth::codex_poll_device_auth(&pending.device_auth_id, &pending.user_code)
        .await
        .map_err(|error| AppError::Internal(error.to_string()))?
    {
        Some((authorization_code, code_verifier)) => {
            let tokens = oauth::codex_exchange_code(&authorization_code, &code_verifier)
                .await
                .map_err(|error| AppError::Internal(error.to_string()))?;

            let provider = Provider::Codex {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                expires_at: tokens.expires_at,
                account_id: tokens.account_id,
            };

            save_provider_credentials(&state, viewer.id, &provider).await?;
            clear_codex_pending(&state.db, viewer.id).await?;

            Ok(Json(CodexPollResponse {
                status: "connected",
                message: Some("Codex is connected.".to_string()),
            }))
        }
        None => Ok(Json(CodexPollResponse {
            status: "pending",
            message: Some(format!(
                "Waiting for approval. Enter code {} in the OpenAI window.",
                pending.user_code
            )),
        })),
    }
}

async fn disconnect_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    sqlx::query("DELETE FROM user_provider_credentials WHERE user_id = $1")
        .bind(viewer.id)
        .execute(&state.db)
        .await?;
    clear_codex_pending(&state.db, viewer.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_storms(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<StormRunSummary>>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let runs = state.storm_runs.read().await;
    let mut items = runs
        .values()
        .filter(|run| run.owner_user_id == viewer.id)
        .map(StormRunRecord::summary_view)
        .collect::<Vec<_>>();
    items.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(Json(items))
}

async fn create_storm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<StormRequest>,
) -> Result<Json<StormResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let prompt = payload.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(AppError::BadRequest("Seed prompt is required.".to_string()));
    }

    let loaded_provider = load_provider_for_user(&state, viewer.id).await?;
    let Some(loaded_provider) = loaded_provider else {
        return Err(AppError::BadRequest(
            "Connect Codex in settings before generating a storm.".to_string(),
        ));
    };

    let run_id = Uuid::new_v4();
    let workspace_dir = state
        .config
        .workspace_root
        .join(viewer.id.to_string())
        .join(run_id.to_string());
    tokio::fs::create_dir_all(&workspace_dir).await?;
    write_workspace_scaffold(&workspace_dir, &prompt).await?;

    let preview_url = format!("/preview/{run_id}");
    let workspace = Arc::new(Mutex::new(WorkspaceRuntimeState {
        run_id,
        preview_url: preview_url.clone(),
        workspace_dir: workspace_dir.clone(),
        prompt: prompt.clone(),
        title: "Storm Artifact".to_string(),
        summary: "Design language artifact in progress.".to_string(),
        submitted: false,
    }));

    let model = state
        .config
        .storm_model
        .clone()
        .unwrap_or_else(|| loaded_provider.provider.default_model().to_string());
    let runtime_ctx = StormRuntimeCtx {
        provider: loaded_provider.provider,
        model,
        tavily_api_key: state.config.tavily_api_key.clone(),
    };

    let result = run_design_agent(StormAgentRole::Root, true, workspace.clone(), runtime_ctx, prompt)
        .await
        .map_err(AppError::Internal)?;

    let assistant_summary = result.assistant_output.safe_text.trim().to_string();
    let snapshot = workspace.lock().await.snapshot();
    let record = StormRunRecord {
        id: snapshot.run_id,
        owner_user_id: viewer.id,
        prompt: snapshot.prompt,
        title: snapshot.title,
        summary: snapshot.summary,
        assistant_summary,
        preview_url,
        submitted: snapshot.submitted,
        created_at: Utc::now(),
        workspace_dir: snapshot.workspace_dir,
    };
    let summary = record.summary_view();
    state.storm_runs.write().await.insert(run_id, record);

    Ok(Json(StormResponse {
        run: summary.clone(),
        assistant_summary: summary.assistant_summary,
    }))
}

async fn preview_index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let run = get_owned_run(&state, viewer.id, run_id).await?;
    let html = tokio::fs::read_to_string(run.workspace_dir.join("index.html")).await?;
    Ok((
        [(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        Html(html),
    )
        .into_response())
}

async fn preview_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((run_id, path)): Path<(Uuid, String)>,
) -> Result<Response, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let run = get_owned_run(&state, viewer.id, run_id).await?;
    let asset_path = resolve_workspace_path(&run.workspace_dir, &path)
        .map_err(AppError::BadRequest)?;
    let bytes = tokio::fs::read(&asset_path).await?;
    let mime = from_path(&asset_path).first_or_octet_stream();

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, mime.as_ref())
        .body(Body::from(bytes))
        .map_err(|error| AppError::Internal(error.to_string()))?)
}

fn verify_clerk_token(state: &AppState, token: &str) -> Result<ClerkClaims, AppError> {
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_required_spec_claims(&["exp", "sub", "iss"]);
    validation.set_issuer(&[state.config.clerk_issuer.as_str()]);
    let decoded = decode::<ClerkClaims>(token, &state.clerk_decoding_key, &validation)?;
    Ok(decoded.claims)
}

async fn fetch_clerk_user(state: &AppState, clerk_user_id: &str) -> Result<ClerkUser, AppError> {
    let Some(secret_key) = state.config.clerk_secret_key.as_ref() else {
        return Err(AppError::Internal(
            "CLERK_SECRET_KEY is not configured".to_string(),
        ));
    };

    let user = state
        .http
        .get(format!("https://api.clerk.com/v1/users/{clerk_user_id}"))
        .bearer_auth(secret_key)
        .send()
        .await?
        .error_for_status()?
        .json::<ClerkUser>()
        .await?;
    Ok(user)
}

async fn current_viewer(state: &AppState, headers: &HeaderMap) -> Result<Option<Viewer>, AppError> {
    let Some(token) = read_cookie(headers, SESSION_COOKIE_NAME) else {
        return Ok(None);
    };

    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_required_spec_claims(&["exp", "sub"]);
    let claims = match decode::<SessionClaims>(&token, &state.session_decoding_key, &validation) {
        Ok(token) => token.claims,
        Err(_) => return Ok(None),
    };

    let user = sqlx::query_as::<_, UserRow>(
        r#"
        SELECT id, clerk_user_id, email, name, avatar_url, created_at
        FROM users
        WHERE id = $1
        "#,
    )
    .bind(match Uuid::parse_str(&claims.sub) {
        Ok(user_id) => user_id,
        Err(_) => return Ok(None),
    })
    .fetch_optional(&state.db)
    .await?;

    Ok(user.map(Viewer::from))
}

async fn require_viewer(state: &AppState, headers: &HeaderMap) -> Result<Viewer, AppError> {
    current_viewer(state, headers)
        .await?
        .ok_or_else(|| AppError::BadRequest("Authentication required.".to_string()))
}

async fn upsert_user(
    db: &PgPool,
    clerk_user_id: &str,
    email: Option<String>,
    name: String,
    avatar_url: Option<String>,
) -> Result<Viewer, AppError> {
    let row = sqlx::query_as::<_, UserRow>(
        r#"
        INSERT INTO users (clerk_user_id, email, name, avatar_url)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (clerk_user_id) DO UPDATE
        SET email = EXCLUDED.email,
            name = EXCLUDED.name,
            avatar_url = EXCLUDED.avatar_url,
            updated_at = now()
        RETURNING id, clerk_user_id, email, name, avatar_url, created_at
        "#,
    )
    .bind(clerk_user_id)
    .bind(email)
    .bind(name)
    .bind(avatar_url)
    .fetch_one(db)
    .await?;

    Ok(row.into())
}

fn session_cookie(state: &AppState, viewer: &Viewer) -> Result<Cookie<'static>, AppError> {
    let expiration = Utc::now() + ChronoDuration::days(7);
    let claims = SessionClaims {
        sub: viewer.id.to_string(),
        clerk_user_id: viewer.clerk_user_id.clone(),
        exp: expiration.timestamp() as usize,
    };
    let token = encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &state.session_encoding_key,
    )?;

    Ok(Cookie::build((SESSION_COOKIE_NAME, token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.app_url.starts_with("https://"))
        .expires(OffsetDateTime::now_utc() + time::Duration::days(7))
        .build())
}

fn cleared_session_cookie() -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .expires(OffsetDateTime::now_utc() - time::Duration::days(1))
        .build()
}

fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(COOKIE)?.to_str().ok()?;
    header.split(';').find_map(|fragment| {
        let mut parts = fragment.trim().splitn(2, '=');
        let key = parts.next()?;
        let value = parts.next()?;
        if key == name {
            Some(value.to_string())
        } else {
            None
        }
    })
}

fn required_env(name: &str) -> Result<String, AppError> {
    env::var(name).map_err(|_| AppError::MissingEnv(name.to_string()))
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "designstorm=info,tower_http=info".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

impl From<UserRow> for Viewer {
    fn from(value: UserRow) -> Self {
        Self {
            id: value.id,
            clerk_user_id: value.clerk_user_id,
            email: value.email,
            name: value
                .name
                .unwrap_or_else(|| "Design Storm User".to_string()),
            avatar_url: value.avatar_url,
            created_at: value.created_at,
        }
    }
}

async fn render_provider_panel_html(state: &AppState, viewer: &Viewer) -> Result<String, AppError> {
    let status = provider_status_view(state, viewer.id).await?;
    Ok(ProviderPanelTemplate { status: &status }.render()?)
}

async fn provider_status_view(
    state: &AppState,
    user_id: Uuid,
) -> Result<ProviderStatusView, AppError> {
    match load_provider_for_user(state, user_id).await? {
        Some(provider) => {
            let (label, detail) = match &provider.provider {
                Provider::Codex { account_id, .. } => (
                    "Codex OAuth".to_string(),
                    account_id
                        .as_ref()
                        .map(|account_id| format!("Connected via OpenAI account {account_id}."))
                        .unwrap_or_else(|| "Connected via OpenAI device OAuth.".to_string()),
                ),
                Provider::OpenAiGeneric { base_url, .. } => (
                    if matches!(provider.source, ProviderSource::ServerFallback) {
                        "Server Provider".to_string()
                    } else {
                        "OpenAI-Compatible".to_string()
                    },
                    format!("Requests route through {base_url}."),
                ),
                Provider::Claude { .. } => (
                    "Claude OAuth".to_string(),
                    "Anthropic OAuth tokens are stored for this account.".to_string(),
                ),
                Provider::GoogleOAuth { .. } => (
                    "Google OAuth".to_string(),
                    "Google OAuth tokens are stored for this account.".to_string(),
                ),
            };
            Ok(ProviderStatusView {
                connected: true,
                using_fallback: matches!(provider.source, ProviderSource::ServerFallback),
                label,
                detail,
                updated_label: provider
                    .updated_at
                    .map(|timestamp| format!("Updated {}", timestamp.format("%Y-%m-%d %H:%M UTC")))
                    .unwrap_or_else(|| "Server-scoped fallback".to_string()),
            })
        }
        None => Ok(ProviderStatusView {
            connected: false,
            using_fallback: false,
            label: "No provider connected".to_string(),
            detail:
                "Connect Codex here. The site stores the encrypted OAuth tokens server-side and uses them for storm generation."
                    .to_string(),
            updated_label: "No stored runtime credentials".to_string(),
        }),
    }
}

async fn load_provider_for_user(
    state: &AppState,
    user_id: Uuid,
) -> Result<Option<LoadedProvider>, AppError> {
    let row = sqlx::query_as::<_, ProviderCredentialRow>(
        r#"
        SELECT encrypted_config, updated_at
        FROM user_provider_credentials
        WHERE user_id = $1
        "#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    if let Some(row) = row {
        let mut stored = decrypt_provider_config(&state.config.session_secret, &row.encrypted_config)?;
        let refreshed = stored
            .provider
            .ensure_fresh()
            .await
            .map_err(|error| AppError::Internal(error.to_string()))?;
        if refreshed {
            save_provider_credentials(state, user_id, &stored.provider).await?;
        }
        return Ok(Some(LoadedProvider {
            provider: stored.provider,
            source: ProviderSource::Stored,
            updated_at: Some(row.updated_at),
        }));
    }

    Ok(server_fallback_provider(&state.config).map(|provider| LoadedProvider {
        provider,
        source: ProviderSource::ServerFallback,
        updated_at: None,
    }))
}

fn server_fallback_provider(config: &Config) -> Option<Provider> {
    config
        .openai_generic_api_key
        .as_ref()
        .map(|api_key| Provider::OpenAiGeneric {
            api_key: api_key.clone(),
            base_url: config.openai_generic_base_url.clone(),
        })
}

async fn save_provider_credentials(
    state: &AppState,
    user_id: Uuid,
    provider: &Provider,
) -> Result<(), AppError> {
    let config = StoredProviderConfig {
        provider: provider.clone(),
    };
    let encrypted = encrypt_provider_config(&state.config.session_secret, &config)?;

    sqlx::query(
        r#"
        INSERT INTO user_provider_credentials (user_id, provider_kind, encrypted_config)
        VALUES ($1, $2, $3)
        ON CONFLICT (user_id) DO UPDATE
        SET provider_kind = EXCLUDED.provider_kind,
            encrypted_config = EXCLUDED.encrypted_config,
            updated_at = now()
        "#,
    )
    .bind(user_id)
    .bind(provider.kind().id())
    .bind(encrypted)
    .execute(&state.db)
    .await?;

    Ok(())
}

async fn clear_codex_pending(db: &PgPool, user_id: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM codex_device_auth_sessions WHERE user_id = $1")
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(())
}

fn encrypt_provider_config(secret: &str, config: &StoredProviderConfig) -> Result<String, AppError> {
    let plaintext = serde_json::to_vec(config)?;
    let key = derive_cipher_key(secret);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|error| AppError::Internal(error.to_string()))?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_ref())
        .map_err(|error| AppError::Internal(error.to_string()))?;
    let mut combined = nonce.to_vec();
    combined.extend(ciphertext);
    Ok(STANDARD.encode(combined))
}

fn decrypt_provider_config(secret: &str, encrypted: &str) -> Result<StoredProviderConfig, AppError> {
    let bytes = STANDARD
        .decode(encrypted)
        .map_err(|error| AppError::Internal(error.to_string()))?;
    if bytes.len() < 12 {
        return Err(AppError::Internal(
            "Encrypted provider config payload is invalid.".to_string(),
        ));
    }
    let (nonce_bytes, ciphertext) = bytes.split_at(12);
    let key = derive_cipher_key(secret);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|error| AppError::Internal(error.to_string()))?;
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .map_err(|error| AppError::Internal(error.to_string()))?;
    Ok(serde_json::from_slice(&plaintext)?)
}

fn derive_cipher_key(secret: &str) -> [u8; 32] {
    let digest = Sha256::digest(secret.as_bytes());
    let mut key = [0_u8; 32];
    key.copy_from_slice(&digest);
    key
}

async fn write_workspace_scaffold(workspace_dir: &StdPath, prompt: &str) -> Result<(), AppError> {
    let index_html = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"UTF-8\">\n  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n  <title>Storm Artifact</title>\n  <link rel=\"stylesheet\" href=\"styles.css\">\n</head>\n<body>\n  <main class=\"storm-doc\">\n    <section class=\"seed-block\">\n      <span class=\"eyebrow\">Seed</span>\n      <p>{}</p>\n    </section>\n  </main>\n</body>\n</html>\n",
        html_escape(prompt)
    );
    let styles = ":root {\n  color-scheme: dark;\n  --bg: #071018;\n  --text: #d8e4ee;\n}\nbody {\n  margin: 0;\n  min-height: 100vh;\n  font-family: 'IBM Plex Sans', system-ui, sans-serif;\n  background: var(--bg);\n  color: var(--text);\n}\n.storm-doc {\n  padding: 48px;\n}\n.seed-block {\n  max-width: 40rem;\n}\n.eyebrow {\n  display: inline-block;\n  margin-bottom: 12px;\n  font-size: 12px;\n  letter-spacing: 0.18em;\n  text-transform: uppercase;\n  opacity: 0.65;\n}\n";
    let manifest = json!({
        "seed": prompt,
        "goal": "Create a design language document as a full static HTML artifact."
    })
    .to_string();

    tokio::fs::write(workspace_dir.join("index.html"), index_html).await?;
    tokio::fs::write(workspace_dir.join("styles.css"), styles).await?;
    tokio::fs::write(workspace_dir.join("manifest.json"), manifest).await?;
    Ok(())
}

async fn get_owned_run(
    state: &AppState,
    user_id: Uuid,
    run_id: Uuid,
) -> Result<StormRunRecord, AppError> {
    let runs = state.storm_runs.read().await;
    match runs.get(&run_id) {
        Some(run) if run.owner_user_id == user_id => Ok(run.clone()),
        _ => Err(AppError::BadRequest("Storm run not found.".to_string())),
    }
}

fn build_runtime_capabilities(with_web: bool) -> AgentCapabilities {
    let mut capabilities = AgentCapabilities::default()
        .disable(lash_core::CapabilityId::CoreRead)
        .disable(lash_core::CapabilityId::CoreWrite)
        .disable(lash_core::CapabilityId::Shell)
        .disable(lash_core::CapabilityId::Tasks)
        .disable(lash_core::CapabilityId::Planning)
        .disable(lash_core::CapabilityId::Delegation)
        .disable(lash_core::CapabilityId::Memory)
        .disable(lash_core::CapabilityId::History)
        .disable(lash_core::CapabilityId::Skills);
    if !with_web {
        capabilities = capabilities.disable(lash_core::CapabilityId::Web);
    }
    capabilities
}

fn prompt_overrides(role: StormAgentRole) -> Vec<PromptSectionOverride> {
    vec![
        PromptSectionOverride {
            section: PromptSectionName::Identity,
            mode: PromptOverrideMode::Replace,
            content: role.prompt_identity().to_string(),
        },
        PromptSectionOverride {
            section: PromptSectionName::Personality,
            mode: PromptOverrideMode::Replace,
            content: "Be specific, visually opinionated, and economical. Avoid generic startup aesthetics. Prefer clear design rules, sharp contrast, and deliberate composition.".to_string(),
        },
        PromptSectionOverride {
            section: PromptSectionName::ExecutionContract,
            mode: PromptOverrideMode::Replace,
            content: "Work only inside the provided artifact workspace. Use the workspace tools to inspect and edit files, use render/view tools to verify the output, and call submit_result before you finish whenever you have a viable artifact.".to_string(),
        },
        PromptSectionOverride {
            section: PromptSectionName::ToolAccess,
            mode: PromptOverrideMode::Replace,
            content: "Your tools are intentionally narrow. There is no shell and no host filesystem access. Every change must happen through workspace tools or the provided subagent tool.".to_string(),
        },
        PromptSectionOverride {
            section: PromptSectionName::Guidelines,
            mode: PromptOverrideMode::Replace,
            content: "Generate static HTML that feels authored. Build a design-language page, not a CRUD app. Keep assets self-contained, make type and spacing choices legible, and use submit_result only after the preview is coherent.".to_string(),
        },
        PromptSectionOverride {
            section: PromptSectionName::ErrorRecovery,
            mode: PromptOverrideMode::Replace,
            content: "If a tool call fails, inspect the workspace, fix the cause, and continue. If the preview is weak, iterate instead of explaining why it would be weak.".to_string(),
        },
        PromptSectionOverride {
            section: PromptSectionName::Memory,
            mode: PromptOverrideMode::Disable,
            content: String::new(),
        },
        PromptSectionOverride {
            section: PromptSectionName::MemoryApi,
            mode: PromptOverrideMode::Disable,
            content: String::new(),
        },
        PromptSectionOverride {
            section: PromptSectionName::Builtins,
            mode: PromptOverrideMode::Disable,
            content: String::new(),
        },
    ]
}

fn compose_agent_prompt(role: StormAgentRole, prompt: String) -> String {
    match role {
        StormAgentRole::Root => format!(
            "Design a distinctive design-language document for this seed:\n\n{prompt}\n\nRequirements:\n- produce a full static HTML artifact in the workspace\n- use index.html and styles.css as the primary files\n- if web research helps, use search_web/fetch_url selectively\n- use spawn_subagent for focused research, rendering, or critique when useful\n- render and inspect the artifact before finishing\n- call submit_result(title=..., summary=...) once the result is coherent"
        ),
        StormAgentRole::Researcher => format!(
            "Research visual references and return a tight brief for this task:\n\n{prompt}\n\nFocus on eras, materials, layout cues, and concrete visual rules."
        ),
        StormAgentRole::Renderer => format!(
            "Improve or complete the workspace artifact for this task:\n\n{prompt}\n\nWrite concrete HTML/CSS changes, render the result, and submit if it becomes stronger."
        ),
        StormAgentRole::Critic => format!(
            "Critique the current workspace artifact for this task:\n\n{prompt}\n\nUse view_result and workspace_read to identify weak, generic, or incoherent choices, then return the clearest revisions."
        ),
    }
}

async fn build_tool_provider(
    role: StormAgentRole,
    allow_subagents: bool,
    workspace: Arc<Mutex<WorkspaceRuntimeState>>,
    runtime: StormRuntimeCtx,
) -> Arc<dyn ToolProvider> {
    let custom = StormToolProvider::new(role, allow_subagents, workspace, runtime.clone());
    let mut tools = CompositeTools::new().add(custom);
    if let Some(key) = runtime.tavily_api_key.as_ref() {
        tools = tools.add(WebSearch::new(key.clone())).add(FetchUrl::new(key.clone()));
    }
    Arc::new(tools)
}

async fn run_design_agent(
    role: StormAgentRole,
    allow_subagents: bool,
    workspace: Arc<Mutex<WorkspaceRuntimeState>>,
    runtime: StormRuntimeCtx,
    prompt: String,
) -> Result<lash_core::AssembledTurn, String> {
    let workspace_dir = { workspace.lock().await.workspace_dir.clone() };
    let tools = build_tool_provider(role, allow_subagents, workspace, runtime.clone()).await;
    let has_web = runtime.tavily_api_key.is_some();
    let config = RuntimeConfig {
        capabilities: build_runtime_capabilities(has_web),
        model: model_for_role(&runtime.provider, &runtime.model, role),
        provider: runtime.provider,
        execution_mode: lash_core::ExecutionMode::NativeTools,
        host_profile: HostProfile::Embedded,
        headless: true,
        session_id: Some(format!("storm-{}", Uuid::new_v4())),
        prompt_overrides: prompt_overrides(role),
        base_dir: Some(workspace_dir),
        ..RuntimeConfig::default()
    };
    let mut engine = RuntimeEngine::from_state(config, tools, AgentStateEnvelope::default())
        .await
        .map_err(|error| error.to_string())?;
    let input = TurnInput {
        items: vec![InputItem::Text {
            text: compose_agent_prompt(role, prompt),
        }],
        image_blobs: HashMap::new(),
        mode: None,
        plan_file: None,
    };
    engine
        .run_turn_assembled(input, CancellationToken::new())
        .await
        .map_err(|error| error.to_string())
}

fn model_for_role(provider: &Provider, fallback_model: &str, role: StormAgentRole) -> String {
    let tier = match role {
        StormAgentRole::Root | StormAgentRole::Renderer => "high",
        StormAgentRole::Researcher => "low",
        StormAgentRole::Critic => "medium",
    };
    provider
        .default_agent_model(tier)
        .map(|(model, _)| model.to_string())
        .unwrap_or_else(|| fallback_model.to_string())
}

fn resolve_workspace_path(root: &StdPath, logical_path: &str) -> Result<PathBuf, String> {
    let candidate = StdPath::new(logical_path);
    if candidate.is_absolute() {
        return Err("Path must be relative to the workspace root.".to_string());
    }

    let mut sanitized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => sanitized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("Path escapes the workspace root.".to_string());
            }
        }
    }

    if sanitized.as_os_str().is_empty() {
        return Err("Path cannot be empty.".to_string());
    }

    Ok(root.join(sanitized))
}

fn truncate_for_tool(content: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for character in content.chars().take(max_chars) {
        out.push(character);
    }
    if content.chars().count() > max_chars {
        out.push_str("\n... [truncated]");
    }
    out
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
