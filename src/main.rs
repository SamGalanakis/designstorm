use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use askama::Template;
use async_stream::stream;
use axum::{
    Router,
    body::Body,
    extract::{Json, Path, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{
            CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, SET_COOKIE,
            X_CONTENT_TYPE_OPTIONS,
        },
    },
    response::{
        Html, IntoResponse, Redirect, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cookie::{Cookie, SameSite};
use datastar::{
    axum::ReadSignals,
    prelude::{ElementPatchMode, PatchElements, PatchSignals},
};
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
use sqlx::{
    FromRow, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    collections::HashMap,
    convert::Infallible,
    env,
    net::SocketAddr,
    pin::Pin,
    path::{Component, Path as StdPath, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tower_http::{
    services::ServeDir,
    trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer},
};
use tracing::{Level, error, info, warn};
use uuid::Uuid;

const SESSION_COOKIE_NAME: &str = "designstorm_session";
const DATASTAR_CDN: &str =
    "https://cdn.jsdelivr.net/gh/starfederation/datastar@1.0.0-RC.8/bundles/datastar.js";

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    db: PgPool,
    http: Client,
    session_encoding_key: EncodingKey,
    session_decoding_key: DecodingKey,
    clerk_decoding_key: DecodingKey,
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
        if status.is_server_error() {
            error!(%status, error = %message, "request failed");
        } else {
            warn!(%status, error = %message, "request rejected");
        }
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
    parent_ids: Vec<Uuid>,
}

impl StormRunSummary {
    fn created_label(&self) -> String {
        self.created_at.format("%b %d").to_string()
    }

    fn created_iso(&self) -> String {
        self.created_at.to_rfc3339()
    }

    fn status_label(&self) -> &'static str {
        if self.submitted { "submitted" } else { "draft" }
    }

    fn status_class(&self) -> &'static str {
        if self.submitted {
            "pill pill-accent"
        } else {
            "pill pill-muted"
        }
    }

    fn parent_ids_csv(&self) -> String {
        self.parent_ids
            .iter()
            .map(Uuid::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[derive(Debug, Clone)]
struct RootNavigatorEntry {
    run_id: String,
    label: String,
    summary: String,
    created_label: String,
    branch_size: usize,
}

impl RootNavigatorEntry {
    fn branch_label(&self) -> String {
        if self.branch_size == 1 {
            "1 artifact".to_string()
        } else {
            format!("{} artifacts", self.branch_size)
        }
    }
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
    parent_ids: Vec<Uuid>,
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
            parent_ids: self.parent_ids.clone(),
        }
    }
}

#[derive(Debug, Clone, FromRow)]
struct StormRunRow {
    id: Uuid,
    owner_user_id: Uuid,
    prompt: String,
    title: String,
    summary: String,
    assistant_summary: String,
    preview_url: String,
    submitted: bool,
    created_at: DateTime<Utc>,
    workspace_dir: String,
    parent_ids: Vec<Uuid>,
}

impl From<StormRunRow> for StormRunRecord {
    fn from(row: StormRunRow) -> Self {
        Self {
            id: row.id,
            owner_user_id: row.owner_user_id,
            prompt: row.prompt,
            title: row.title,
            summary: row.summary,
            assistant_summary: row.assistant_summary,
            preview_url: row.preview_url,
            submitted: row.submitted,
            created_at: row.created_at,
            workspace_dir: PathBuf::from(row.workspace_dir),
            parent_ids: row.parent_ids,
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
}

impl StormAgentRole {
    fn label(self) -> &'static str {
        "root"
    }

    fn prompt_identity(self) -> &'static str {
        "You are Design Storm, an AI art-direction runtime that creates bold design language documents as static HTML artifacts."
    }
}

struct StormToolProvider {
    workspace: Arc<Mutex<WorkspaceRuntimeState>>,
}

impl StormToolProvider {
    fn new(
        _role: StormAgentRole,
        workspace: Arc<Mutex<WorkspaceRuntimeState>>,
    ) -> Self {
        Self { workspace }
    }

    fn logical_path(&self, args: &serde_json::Value, key: &str) -> Result<String, ToolResult> {
        args.get(key)
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| ToolResult::err_fmt(format_args!("Missing required parameter: {key}")))
    }

    async fn run_id(&self) -> Uuid {
        self.workspace.lock().await.run_id
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
}

#[async_trait::async_trait]
impl ToolProvider for StormToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let defs = vec![
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

        defs
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolResult {
        let run_id = self.run_id().await;
        info!(
            run_id = %run_id,
            tool = name,
            args = %summarize_tool_args(args),
            "storm tool call started"
        );
        let started = Instant::now();
        let result = match name {
            "workspace_list" => self.workspace_list().await,
            "workspace_read" => self.workspace_read(args).await,
            "workspace_write" => self.workspace_write(args).await,
            "render_result" => self.render_result().await,
            "view_result" => self.view_result().await,
            "submit_result" => self.submit_result(args).await,
            _ => ToolResult::err_fmt(format_args!("Unknown tool: {name}")),
        };
        info!(
            run_id = %run_id,
            tool = name,
            elapsed_ms = started.elapsed().as_millis(),
            success = result.success,
            result = %truncate_for_log(&result.result.to_string(), 480),
            "storm tool call finished"
        );
        result
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
    board_html: &'a str,
    roots_html: &'a str,
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

#[derive(Template)]
#[template(path = "storm_board.html")]
struct StormBoardTemplate<'a> {
    runs: &'a [StormRunSummary],
}

#[derive(Template)]
#[template(path = "roots_list.html")]
struct RootsListTemplate<'a> {
    roots: &'a [RootNavigatorEntry],
}

#[derive(Debug, Deserialize)]
struct StormRequest {
    prompt: String,
    draft_mode: Option<String>,
    source_ids: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateStormSignals {
    prompt: String,
    draft_mode: Option<String>,
    source_ids: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientTelemetryEvent {
    event_type: String,
    message: Option<String>,
    details: Option<serde_json::Value>,
    href: String,
    user_agent: String,
    timestamp: String,
}

#[derive(Debug, Clone)]
struct StormGenerationInput {
    prompt: String,
    draft_mode: Option<String>,
    source_ids: Vec<Uuid>,
}

impl StormGenerationInput {
    fn from_prompt_and_sources(
        prompt: String,
        draft_mode: Option<String>,
        source_ids: Option<String>,
    ) -> Result<Self, String> {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return Err("Seed prompt is required.".to_string());
        }

        let source_ids = source_ids
            .unwrap_or_default()
            .split(',')
            .filter_map(|fragment| {
                let trimmed = fragment.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(
                        Uuid::parse_str(trimmed)
                            .map_err(|_| format!("Invalid source id: {trimmed}")),
                    )
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            prompt,
            draft_mode: draft_mode
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            source_ids,
        })
    }
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
    info!("ensuring workspace root exists");
    tokio::fs::create_dir_all(&config.workspace_root).await?;

    info!("connecting to postgres");
    let connect_options = PgConnectOptions::from_str(&config.database_url)
        .map_err(|error| AppError::Internal(format!("invalid DATABASE_URL: {error}")))?;
    let db = tokio::time::timeout(
        Duration::from_secs(10),
        PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(15))
        .connect_with(connect_options),
    )
    .await
    .map_err(|_| AppError::Internal("Timed out connecting to postgres".to_string()))??;
    info!("postgres connection established");

    info!("running sql migrations");
    sqlx::migrate!("./migrations").run(&db).await?;
    info!("sql migrations completed");

    let state = AppState {
        db,
        http: Client::new(),
        session_encoding_key: EncodingKey::from_secret(config.session_secret.as_bytes()),
        session_decoding_key: DecodingKey::from_secret(config.session_secret.as_bytes()),
        clerk_decoding_key: DecodingKey::from_rsa_pem(config.clerk_jwt_public_key.as_bytes())?,
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
        .route("/storms/generate", post(generate_storm_datastar))
        .route("/telemetry/client", post(client_telemetry))
        .route("/api/storms", get(list_storms).post(create_storm))
        .route("/preview/{run_id}", get(preview_index_redirect))
        .route("/preview/{run_id}/", get(preview_index))
        .route("/preview/{run_id}/{*path}", get(preview_asset))
        .nest_service("/static", ServeDir::new("static"))
        .nest_service("/docs", ServeDir::new("docs"))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO))
                .on_failure(DefaultOnFailure::new().level(Level::ERROR)),
        )
        .with_state(state.clone());

    let address = SocketAddr::from(([0, 0, 0, 0], state.config.port));
    info!("binding axum listener on {}", address);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| AppError::Internal(error.to_string()))?;
    info!("axum listener ready");
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
) -> Result<Response, AppError> {
    let viewer = current_viewer(&state, &headers).await?;
    if viewer.is_some() {
        return Ok(Redirect::temporary("/app").into_response());
    }
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

    Ok(Html(page.render()?).into_response())
}

async fn app_page(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    let Some(viewer) = current_viewer(&state, &headers).await? else {
        return Ok(Redirect::temporary("/").into_response());
    };

    let provider_panel = render_provider_panel_html(&state, &viewer).await?;
    let board_html = render_storm_board_html(&state, viewer.id).await?;
    let roots_html = render_roots_html(&state, viewer.id).await?;
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
        board_html: &board_html,
        roots_html: &roots_html,
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
    info!(user_id = %viewer.id, "starting codex device auth");
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
    info!(user_id = %viewer.id, "polling codex device auth");
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
            info!(user_id = %viewer.id, "codex device auth approved; exchanging tokens");
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
) -> Result<Html<String>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    info!(user_id = %viewer.id, "disconnecting stored provider credentials");
    sqlx::query("DELETE FROM user_provider_credentials WHERE user_id = $1")
        .bind(viewer.id)
        .execute(&state.db)
        .await?;
    clear_codex_pending(&state.db, viewer.id).await?;
    Ok(Html(render_provider_panel_html(&state, &viewer).await?))
}

async fn list_storms(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<StormRunSummary>>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let mut items = viewer_run_summaries(&state, viewer.id).await;
    items.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    info!(user_id = %viewer.id, run_count = items.len(), "listing storm runs");
    Ok(Json(items))
}

async fn generate_storm_datastar(
    State(state): State<AppState>,
    headers: HeaderMap,
    ReadSignals(signals): ReadSignals<GenerateStormSignals>,
) -> Result<impl IntoResponse, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let input = match StormGenerationInput::from_prompt_and_sources(
        signals.prompt,
        signals.draft_mode,
        signals.source_ids,
    ) {
        Ok(input) => input,
        Err(message) => {
            return Ok(datastar_event_stream(Box::pin(stream! {
                yield Ok::<_, Infallible>(patch_signals(json!({
                    "_generating": false,
                    "_status": message,
                }).to_string()));
            })));
        }
    };

    Ok(datastar_event_stream(Box::pin(stream! {
        yield Ok::<_, Infallible>(patch_signals(json!({
            "_generating": true,
            "_status": "Generating storm...",
            "_latestRunId": "",
        }).to_string()));

        match generate_storm_internal(&state, &viewer, input).await {
            Ok(response) => {
                match (
                    render_storm_board_html(&state, viewer.id).await,
                    render_roots_html(&state, viewer.id).await,
                ) {
                    (Ok(board_html), Ok(roots_html)) => {
                        yield Ok::<_, Infallible>(
                            PatchElements::new(roots_html)
                                .selector("#roots-list")
                                .mode(ElementPatchMode::Inner)
                                .write_as_axum_sse_event(),
                        );
                        yield Ok::<_, Infallible>(
                            PatchElements::new(board_html)
                                .selector("#storm-runs")
                                .mode(ElementPatchMode::Outer)
                                .write_as_axum_sse_event(),
                        );
                        yield Ok::<_, Infallible>(patch_signals(json!({
                            "_generating": false,
                            "_status": "Storm generated.",
                            "prompt": "",
                            "draftMode": "",
                            "sourceIds": "",
                            "_latestRunId": response.run.id.to_string(),
                        }).to_string()));
                    }
                    (Err(error), _) | (_, Err(error)) => {
                        yield Ok::<_, Infallible>(patch_signals(json!({
                            "_generating": false,
                            "_latestRunId": "",
                            "_status": error.to_string(),
                        }).to_string()));
                    }
                }
            }
            Err(error) => {
                yield Ok::<_, Infallible>(patch_signals(json!({
                    "_generating": false,
                    "_latestRunId": "",
                    "_status": error.to_string(),
                }).to_string()));
            }
        }
    })))
}

async fn create_storm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<StormRequest>,
) -> Result<Json<StormResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let input = StormGenerationInput::from_prompt_and_sources(
        payload.prompt,
        payload.draft_mode,
        payload.source_ids,
    )
    .map_err(AppError::BadRequest)?;
    Ok(Json(generate_storm_internal(&state, &viewer, input).await?))
}

async fn generate_storm_internal(
    state: &AppState,
    viewer: &Viewer,
    input: StormGenerationInput,
) -> Result<StormResponse, AppError> {
    let prompt = input.prompt.clone();
    info!(
        user_id = %viewer.id,
        prompt_len = prompt.len(),
        draft_mode = ?input.draft_mode,
        source_count = input.source_ids.len(),
        "storm generation requested"
    );

    let loaded_provider = load_provider_for_user(state, viewer.id).await?;
    let Some(loaded_provider) = loaded_provider else {
        return Err(AppError::BadRequest(
            "Connect Codex in settings before generating a storm.".to_string(),
        ));
    };
    info!(
        user_id = %viewer.id,
        provider_kind = loaded_provider.provider.kind().id(),
        provider_source = %provider_source_label(loaded_provider.source),
        "storm provider resolved"
    );

    let run_id = Uuid::new_v4();
    let workspace_dir = state
        .config
        .workspace_root
        .join(viewer.id.to_string())
        .join(run_id.to_string());
    info!(
        user_id = %viewer.id,
        run_id = %run_id,
        workspace_dir = %workspace_dir.display(),
        "creating storm workspace"
    );
    tokio::fs::create_dir_all(&workspace_dir).await?;
    write_workspace_scaffold(&workspace_dir, &prompt).await?;
    info!(user_id = %viewer.id, run_id = %run_id, "storm workspace scaffolded");

    let preview_url = format!("/preview/{run_id}/");
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
    let storm_started = Instant::now();
    info!(
        user_id = %viewer.id,
        run_id = %run_id,
        model = %runtime_ctx.model,
        has_tavily = runtime_ctx.tavily_api_key.is_some(),
        "starting lash storm runtime"
    );
    let result = match run_design_agent(
        StormAgentRole::Root,
        workspace.clone(),
        runtime_ctx,
        prompt,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            error!(
                user_id = %viewer.id,
                run_id = %run_id,
                elapsed_ms = storm_started.elapsed().as_millis(),
                error = %error,
                "storm generation failed"
            );
            return Err(AppError::Internal(error));
        }
    };
    info!(
        user_id = %viewer.id,
        run_id = %run_id,
        elapsed_ms = storm_started.elapsed().as_millis(),
        status = ?result.status,
        done_reason = ?result.done_reason,
        tool_calls = result.tool_calls.len(),
        errors = result.errors.len(),
        "lash storm runtime completed"
    );

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
        parent_ids: input.source_ids.clone(),
    };
    store_storm_run(&state.db, &record).await?;
    let summary = record.summary_view();
    info!(
        user_id = %viewer.id,
        run_id = %run_id,
        submitted = summary.submitted,
        preview_url = %summary.preview_url,
        "storm run stored"
    );

    Ok(StormResponse {
        run: summary.clone(),
        assistant_summary: summary.assistant_summary,
    })
}

async fn preview_index_redirect(Path(run_id): Path<Uuid>) -> Redirect {
    Redirect::temporary(&format!("/preview/{run_id}/"))
}

async fn preview_index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let run = get_owned_run(&state, viewer.id, run_id).await?;
    info!(user_id = %viewer.id, run_id = %run_id, "serving preview index");
    let html = tokio::fs::read_to_string(run.workspace_dir.join("index.html")).await?;
    let (html, removed_refresh_tags) = strip_meta_refresh_tags(&html);
    if removed_refresh_tags > 0 {
        warn!(
            user_id = %viewer.id,
            run_id = %run_id,
            removed_refresh_tags,
            "sanitized preview html"
        );
    }
    Ok((
        [(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        ), (
            CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'self' data: blob: https:; img-src 'self' data: blob: https:; style-src 'self' 'unsafe-inline' https:; font-src 'self' data: https:; script-src 'self' 'unsafe-inline' 'unsafe-eval' blob: data: https:; connect-src 'self' blob: data: https: wss:; worker-src 'self' blob: data: https:; child-src 'self' blob: data: https:; object-src 'none'; base-uri 'none'; form-action 'self' https:; frame-ancestors 'self'"),
        ), (
            CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=300"),
        ), (
            X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
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
    let normalized_path = normalize_workspace_asset_path(&path);
    let asset_path = resolve_workspace_path(&run.workspace_dir, &normalized_path)
        .map_err(AppError::BadRequest)?;
    info!(
        user_id = %viewer.id,
        run_id = %run_id,
        path = %path,
        normalized_path = %normalized_path,
        "serving preview asset"
    );
    let bytes = tokio::fs::read(&asset_path).await?;
    let mime = from_path(&asset_path).first_or_octet_stream();

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, mime.as_ref())
        .header(CACHE_CONTROL, "private, max-age=300")
        .header(X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from(bytes))
        .map_err(|error| AppError::Internal(error.to_string()))?)
}

async fn client_telemetry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(event): Json<ClientTelemetryEvent>,
) -> Result<StatusCode, AppError> {
    let viewer = current_viewer(&state, &headers).await?;
    info!(
        user_id = viewer
            .as_ref()
            .map(|user| user.id.to_string())
            .unwrap_or_else(|| "anonymous".to_string()),
        event_type = %event.event_type,
        message = ?event.message,
        href = %event.href,
        user_agent = %event.user_agent,
        timestamp = %event.timestamp,
        details = ?event.details,
        "client telemetry"
    );
    Ok(StatusCode::NO_CONTENT)
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

async fn render_storm_board_html(state: &AppState, user_id: Uuid) -> Result<String, AppError> {
    let runs = viewer_run_summaries(state, user_id).await;
    Ok(StormBoardTemplate { runs: &runs }.render()?)
}

fn truncate_for_root_label(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    let char_count = trimmed.chars().count();
    if char_count <= max_chars {
        return trimmed.to_string();
    }
    trimmed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>()
        .trim_end()
        .to_string()
        + "…"
}

fn root_display_label(run: &StormRunSummary, index: usize) -> String {
    let title = run.title.trim();
    if !title.is_empty() && !title.eq_ignore_ascii_case("storm artifact") {
        return title.to_string();
    }
    if !run.prompt.trim().is_empty() {
        return truncate_for_root_label(&run.prompt.replace('\n', " "), 42);
    }
    format!("Root {}", index + 1)
}

fn build_root_entries(runs: &[StormRunSummary]) -> Vec<RootNavigatorEntry> {
    let run_lookup = runs
        .iter()
        .map(|run| (run.id, run))
        .collect::<HashMap<Uuid, &StormRunSummary>>();
    let mut children_by_parent: HashMap<Uuid, Vec<Uuid>> = HashMap::new();

    for run in runs {
        for parent_id in &run.parent_ids {
            if run_lookup.contains_key(parent_id) {
                children_by_parent
                    .entry(*parent_id)
                    .or_default()
                    .push(run.id);
            }
        }
    }

    runs.iter()
        .filter(|run| {
            run.parent_ids
                .iter()
                .filter(|parent_id| run_lookup.contains_key(parent_id))
                .count()
                == 0
        })
        .enumerate()
        .map(|(index, run)| {
            let mut seen = HashMap::<Uuid, ()>::new();
            let mut stack = children_by_parent.get(&run.id).cloned().unwrap_or_default();

            while let Some(child_id) = stack.pop() {
                if seen.insert(child_id, ()).is_some() {
                    continue;
                }
                if let Some(next) = children_by_parent.get(&child_id) {
                    stack.extend(next.iter().copied());
                }
            }

            RootNavigatorEntry {
                run_id: run.id.to_string(),
                label: root_display_label(run, index),
                summary: truncate_for_root_label(
                    if run.summary.trim().is_empty() {
                        run.prompt.trim()
                    } else {
                        run.summary.trim()
                    },
                    112,
                ),
                created_label: run.created_label(),
                branch_size: seen.len() + 1,
            }
        })
        .collect()
}

async fn render_roots_html(state: &AppState, user_id: Uuid) -> Result<String, AppError> {
    let runs = viewer_run_summaries(state, user_id).await;
    let roots = build_root_entries(&runs);
    Ok(RootsListTemplate { roots: &roots }.render()?)
}

async fn viewer_run_summaries(state: &AppState, user_id: Uuid) -> Vec<StormRunSummary> {
    match sqlx::query_as::<_, StormRunRow>(
        r#"
        SELECT id, owner_user_id, prompt, title, summary, assistant_summary, preview_url,
               submitted, created_at, workspace_dir, parent_ids
        FROM storm_runs
        WHERE owner_user_id = $1
        ORDER BY created_at ASC
        "#,
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(StormRunRecord::from)
            .map(|run| run.summary_view())
            .collect(),
        Err(error) => {
            error!(user_id = %user_id, error = %error, "failed to load storm runs");
            Vec::new()
        }
    }
}

fn patch_signals(signals: String) -> Event {
    PatchSignals::new(signals).write_as_axum_sse_event()
}

fn summarize_tool_args(args: &serde_json::Value) -> String {
    let Some(object) = args.as_object() else {
        return truncate_for_log(&args.to_string(), 240);
    };
    let mut parts = Vec::new();
    for (key, value) in object {
        let rendered = match value {
            serde_json::Value::String(text) => {
                if key == "content" {
                    format!("\"{}\"", truncate_for_log(text, 120))
                } else {
                    format!("\"{}\"", truncate_for_log(text, 80))
                }
            }
            _ => truncate_for_log(&value.to_string(), 120),
        };
        parts.push(format!("{key}={rendered}"));
    }
    parts.join(", ")
}

fn truncate_for_log(value: &str, max_chars: usize) -> String {
    let mut iter = value.chars();
    let truncated: String = iter.by_ref().take(max_chars).collect();
    if iter.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn strip_meta_refresh_tags(html: &str) -> (String, usize) {
    let mut output = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0usize;
    let mut removed = 0usize;

    while let Some(relative_start) = lower[cursor..].find("<meta") {
        let start = cursor + relative_start;
        output.push_str(&html[cursor..start]);
        let Some(relative_end) = lower[start..].find('>') else {
            output.push_str(&html[start..]);
            return (output, removed);
        };
        let end = start + relative_end + 1;
        let tag = &lower[start..end];
        if tag.contains("http-equiv") && tag.contains("refresh") {
            removed += 1;
        } else {
            output.push_str(&html[start..end]);
        }
        cursor = end;
    }

    output.push_str(&html[cursor..]);
    (output, removed)
}

fn datastar_event_stream(
    stream: Pin<Box<dyn futures_core::Stream<Item = Result<Event, Infallible>> + Send>>,
) -> impl IntoResponse {
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
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
        info!(
            user_id = %user_id,
            provider_kind = stored.provider.kind().id(),
            "loaded stored provider credentials"
        );
        let refreshed = stored
            .provider
            .ensure_fresh()
            .await
            .map_err(|error| AppError::Internal(error.to_string()))?;
        if refreshed {
            info!(
                user_id = %user_id,
                provider_kind = stored.provider.kind().id(),
                "provider tokens refreshed"
            );
            save_provider_credentials(state, user_id, &stored.provider).await?;
        }
        return Ok(Some(LoadedProvider {
            provider: stored.provider,
            source: ProviderSource::Stored,
            updated_at: Some(row.updated_at),
        }));
    }

    if let Some(provider) = server_fallback_provider(&state.config) {
        info!(
            user_id = %user_id,
            provider_kind = provider.kind().id(),
            "using server fallback provider"
        );
        Ok(Some(LoadedProvider {
            provider,
            source: ProviderSource::ServerFallback,
            updated_at: None,
        }))
    } else {
        warn!(user_id = %user_id, "no provider credentials available");
        Ok(None)
    }
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
    let run = sqlx::query_as::<_, StormRunRow>(
        r#"
        SELECT id, owner_user_id, prompt, title, summary, assistant_summary, preview_url,
               submitted, created_at, workspace_dir, parent_ids
        FROM storm_runs
        WHERE id = $1 AND owner_user_id = $2
        "#,
    )
    .bind(run_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    run.map(StormRunRecord::from)
        .ok_or_else(|| AppError::BadRequest("Storm run not found.".to_string()))
}

async fn store_storm_run(db: &PgPool, record: &StormRunRecord) -> Result<(), AppError> {
    sqlx::query(
        r#"
        INSERT INTO storm_runs (
            id,
            owner_user_id,
            prompt,
            title,
            summary,
            assistant_summary,
            preview_url,
            submitted,
            created_at,
            workspace_dir,
            parent_ids
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (id) DO UPDATE
        SET prompt = EXCLUDED.prompt,
            title = EXCLUDED.title,
            summary = EXCLUDED.summary,
            assistant_summary = EXCLUDED.assistant_summary,
            preview_url = EXCLUDED.preview_url,
            submitted = EXCLUDED.submitted,
            created_at = EXCLUDED.created_at,
            workspace_dir = EXCLUDED.workspace_dir,
            parent_ids = EXCLUDED.parent_ids
        "#,
    )
    .bind(record.id)
    .bind(record.owner_user_id)
    .bind(&record.prompt)
    .bind(&record.title)
    .bind(&record.summary)
    .bind(&record.assistant_summary)
    .bind(&record.preview_url)
    .bind(record.submitted)
    .bind(record.created_at)
    .bind(record.workspace_dir.to_string_lossy().to_string())
    .bind(&record.parent_ids)
    .execute(db)
    .await?;

    Ok(())
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
            content: "Your tools are intentionally narrow. There is no shell, no host filesystem access, and no subagent system. Every change must happen through the workspace, web, render, and submit tools.".to_string(),
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
            "Design a distinctive design-language document for this seed:\n\n{prompt}\n\nRequirements:\n- produce a full static HTML artifact in the workspace\n- use index.html and styles.css as the primary files\n- if web research helps, use search_web/fetch_url selectively\n- iterate yourself instead of delegating to other agents\n- render and inspect the artifact before finishing\n- call submit_result(title=..., summary=...) once the result is coherent"
        ),
    }
}

async fn build_tool_provider(
    role: StormAgentRole,
    workspace: Arc<Mutex<WorkspaceRuntimeState>>,
    runtime: StormRuntimeCtx,
) -> Arc<dyn ToolProvider> {
    let custom = StormToolProvider::new(role, workspace);
    let mut tools = CompositeTools::new().add(custom);
    if let Some(key) = runtime.tavily_api_key.as_ref() {
        tools = tools.add(WebSearch::new(key.clone())).add(FetchUrl::new(key.clone()));
    }
    Arc::new(tools)
}

async fn run_design_agent(
    role: StormAgentRole,
    workspace: Arc<Mutex<WorkspaceRuntimeState>>,
    runtime: StormRuntimeCtx,
    prompt: String,
) -> Result<lash_core::AssembledTurn, String> {
    let (workspace_dir, run_id) = {
        let workspace = workspace.lock().await;
        (workspace.workspace_dir.clone(), workspace.run_id)
    };
    let started = Instant::now();
    info!(
        run_id = %run_id,
        role = role.label(),
        model = %runtime.model,
        workspace_dir = %workspace_dir.display(),
        prompt_len = prompt.len(),
        "initializing lash runtime"
    );
    let tools = build_tool_provider(role, workspace, runtime.clone()).await;
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
        .map_err(|error| {
            error!(
                run_id = %run_id,
                role = role.label(),
                elapsed_ms = started.elapsed().as_millis(),
                error = %error,
                "failed to initialize lash runtime"
            );
            error.to_string()
        })?;
    let input = TurnInput {
        items: vec![InputItem::Text {
            text: compose_agent_prompt(role, prompt),
        }],
        image_blobs: HashMap::new(),
        mode: None,
        plan_file: None,
    };
    info!(
        run_id = %run_id,
        role = role.label(),
        "starting lash turn"
    );
    let turn = engine.run_turn_assembled(input, CancellationToken::new());
    tokio::pin!(turn);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
        tokio::select! {
            result = &mut turn => {
                let result = result.map_err(|error| {
                    error!(
                        run_id = %run_id,
                        role = role.label(),
                        elapsed_ms = started.elapsed().as_millis(),
                        error = %error,
                        "lash turn failed"
                    );
                    error.to_string()
                })?;
                break result;
            }
            _ = heartbeat.tick() => {
                info!(
                    run_id = %run_id,
                    role = role.label(),
                    elapsed_ms = started.elapsed().as_millis(),
                    "lash turn still running"
                );
            }
        }
    };
    info!(
        run_id = %run_id,
        role = role.label(),
        elapsed_ms = started.elapsed().as_millis(),
        status = ?result.status,
        done_reason = ?result.done_reason,
        tool_calls = result.tool_calls.len(),
        errors = result.errors.len(),
        "lash turn completed"
    );
    Ok(result)
}

fn provider_source_label(source: ProviderSource) -> &'static str {
    match source {
        ProviderSource::Stored => "stored",
        ProviderSource::ServerFallback => "server_fallback",
    }
}

fn model_for_role(provider: &Provider, fallback_model: &str, role: StormAgentRole) -> String {
    let tier = match role {
        StormAgentRole::Root => "high",
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

fn normalize_workspace_asset_path(logical_path: &str) -> String {
    logical_path.trim_start_matches('/').to_string()
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
