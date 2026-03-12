use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use askama::Template;
use aws_config::BehaviorVersion;
use aws_sdk_s3::{Client as S3Client, config::Region, primitives::ByteStream};
use axum::{
    Router,
    body::Body,
    extract::{Json, Multipart, Path, Query, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{
            CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE,
            SET_COOKIE, X_CONTENT_TYPE_OPTIONS,
        },
    },
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cookie::{Cookie, SameSite};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use lash_core::{
    AgentCapabilities, AgentStateEnvelope, HostProfile, InputItem, LashRuntime, PromptOverrideMode,
    PromptSectionName, PromptSectionOverride, Provider, RuntimeConfig, ToolDefinition, ToolParam,
    ToolProvider, ToolResult, TurnInput, oauth,
    provider::OPENAI_GENERIC_DEFAULT_BASE_URL,
    tools::{FetchUrl, ToolSet, WebSearch},
};
use futures_util::StreamExt as FuturesStreamExt;
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
    collections::{HashMap, HashSet},
    env,
    io::{Cursor, Read, Write},
    net::SocketAddr,
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
use walkdir::WalkDir;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::FileOptions};

const SESSION_COOKIE_NAME: &str = "designstorm_session";
const DATASTAR_CDN: &str =
    "https://cdn.jsdelivr.net/gh/starfederation/datastar@1.0.0-RC.8/bundles/datastar.js";

#[derive(Clone)]
struct ScreenshotService {
    browser: Arc<chromiumoxide::Browser>,
    /// Keep the handler task alive so the browser doesn't shut down.
    _handle: Arc<tokio::task::JoinHandle<()>>,
}

impl ScreenshotService {
    async fn new() -> Result<Self, String> {
        let (browser, mut handler) =
            chromiumoxide::Browser::launch(
                chromiumoxide::BrowserConfig::builder()
                    .chrome_executable(std::env::var("CHROMIUM_PATH").unwrap_or_else(|_| "/usr/bin/chromium".into()))
                    .no_sandbox()
                    .new_headless_mode()
                    .arg("disable-gpu")
                    .arg("disable-dev-shm-usage")
                    .window_size(1280, 900)
                    .build()
                    .map_err(|e| format!("browser config error: {e}"))?,
            )
            .await
            .map_err(|e| format!("browser launch error: {e}"))?;

        let handle = tokio::spawn(async move {
            while let Some(_event) = handler.next().await {
                // keep the browser event loop running
            }
        });

        Ok(Self {
            browser: Arc::new(browser),
            _handle: Arc::new(handle),
        })
    }

    async fn screenshot(&self, url: &str) -> Result<Vec<u8>, String> {
        let page = self
            .browser
            .new_page(url)
            .await
            .map_err(|e| format!("new_page error: {e}"))?;

        // Wait for the page to settle (fonts, layout, animations)
        tokio::time::sleep(Duration::from_millis(800)).await;

        let bytes = page
            .screenshot(
                ScreenshotParams::builder()
                    .full_page(true)
                    .format(CaptureScreenshotFormat::Png)
                    .build(),
            )
            .await
            .map_err(|e| format!("screenshot error: {e}"))?;

        page.close().await.ok();
        Ok(bytes)
    }
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    db: PgPool,
    http: Client,
    artifact_storage: Option<ArtifactStorage>,
    screenshotter: ScreenshotService,
    session_encoding_key: EncodingKey,
    session_decoding_key: DecodingKey,
    clerk_decoding_key: DecodingKey,
}

#[derive(Clone)]
struct ArtifactStorage {
    client: S3Client,
    bucket: String,
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

async fn build_artifact_storage(config: &Config) -> Result<Option<ArtifactStorage>, AppError> {
    let Some(bucket) = config.bucket_name.clone() else {
        return Ok(None);
    };

    let region = config
        .aws_region
        .clone()
        .unwrap_or_else(|| "auto".to_string());
    let shared_config = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region))
        .load()
        .await;

    let mut s3_config = aws_sdk_s3::config::Builder::from(&shared_config);
    if let Some(endpoint_url) = &config.aws_endpoint_url_s3 {
        s3_config = s3_config.endpoint_url(endpoint_url);
    }

    Ok(Some(ArtifactStorage {
        client: S3Client::from_conf(s3_config.build()),
        bucket,
    }))
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

#[derive(Debug, FromRow)]
struct ClaudeOAuthSessionRow {
    verifier: String,
    auth_url: String,
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
    selected_kind: String,
    connected_kind: Option<String>,
    codex_pending_user_code: Option<String>,
    claude_pending_auth_url: Option<String>,
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
    session_id: Option<Uuid>,
    iterates_on_id: Option<Uuid>,
    position_x: Option<f64>,
    position_y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
}

impl StormRunSummary {
    fn created_label(&self) -> String {
        self.created_at.format("%b %d").to_string()
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
    session_id: Option<Uuid>,
    iterates_on_id: Option<Uuid>,
    position_x: Option<f64>,
    position_y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
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
            session_id: self.session_id,
            iterates_on_id: self.iterates_on_id,
            position_x: self.position_x,
            position_y: self.position_y,
            width: self.width,
            height: self.height,
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
    session_id: Option<Uuid>,
    iterates_on_id: Option<Uuid>,
    position_x: Option<f64>,
    position_y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
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
            session_id: row.session_id,
            iterates_on_id: row.iterates_on_id,
            position_x: row.position_x,
            position_y: row.position_y,
            width: row.width,
            height: row.height,
        }
    }
}

#[derive(Debug, Clone, FromRow)]
struct DesignSessionRow {
    id: Uuid,
    title: String,
    updated_at: DateTime<Utc>,
}

impl DesignSessionRow {
    fn updated_label(&self) -> String {
        self.updated_at.format("%b %d").to_string()
    }
}

#[derive(Debug, Clone, FromRow)]
struct SessionMessageRow {
    role: String,
    body: String,
    design_job_id: Option<Uuid>,
    created_at: DateTime<Utc>,
}

impl SessionMessageRow {
    fn created_label(&self) -> String {
        self.created_at.format("%H:%M").to_string()
    }
}

#[derive(Debug, Clone, FromRow)]
struct ReferenceItemRow {
    id: Uuid,
    kind: String,
    title: String,
    content_json: serde_json::Value,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
struct DesignJobRow {
    id: Uuid,
    session_id: Uuid,
    status: String,
    prompt: String,
    title: String,
    iterates_on_id: Option<Uuid>,
    result_run_id: Option<Uuid>,
    reference_snapshot_json: serde_json::Value,
    error: Option<String>,
    created_at: DateTime<Utc>,
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
    screenshotter: ScreenshotService,
    port: u16,
}

impl StormToolProvider {
    fn new(_role: StormAgentRole, workspace: Arc<Mutex<WorkspaceRuntimeState>>, screenshotter: ScreenshotService, port: u16) -> Self {
        Self { workspace, screenshotter, port }
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
                Err(error) => {
                    return ToolResult::err_fmt(format_args!("Failed to list workspace: {error}"));
                }
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
                        let relative =
                            match path.strip_prefix(&self.workspace.lock().await.workspace_dir) {
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
                        return ToolResult::err_fmt(format_args!(
                            "Failed to iterate workspace: {error}"
                        ));
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
            Err(error) => ToolResult::err_fmt(format_args!(
                "Failed to read {}: {error}",
                resolved.display()
            )),
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
            Err(error) => ToolResult::err_fmt(format_args!(
                "Failed to write {}: {error}",
                resolved.display()
            )),
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

        let result = json!({
            "previewUrl": workspace.preview_url,
            "hasIndex": has_index,
            "hasStyles": has_styles
        });

        let url = format!("http://127.0.0.1:{}{}", self.port, workspace.preview_url);
        let png = self.screenshotter.screenshot(&url).await
            .map_err(|e| format!("screenshot failed: {e}")).unwrap();
        let image = lash_core::ToolImage {
            mime: "image/png".to_string(),
            data: png,
            label: format!("{} — render preview", workspace.title),
        };
        ToolResult::with_images(true, result, vec![image])
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

        let url = format!("http://127.0.0.1:{}{}", self.port, workspace.preview_url);
        let png = self.screenshotter.screenshot(&url).await
            .map_err(|e| format!("screenshot failed: {e}")).unwrap();
        let image = lash_core::ToolImage {
            mime: "image/png".to_string(),
            data: png,
            label: format!("{} — view preview", workspace.title),
        };

        let result = json!({
            "previewUrl": workspace.preview_url,
            "htmlExcerpt": truncate_for_tool(&html, 2400),
            "cssExcerpt": truncate_for_tool(&css, 1800),
            "title": workspace.title,
            "summary": workspace.summary
        });

        ToolResult::with_images(true, result, vec![image])
    }

    async fn copy_input(&self, args: &serde_json::Value) -> ToolResult {
        let source = match self.logical_path(args, "source") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let destination = match self.logical_path(args, "destination") {
            Ok(p) => p,
            Err(e) => return e,
        };

        if !source.starts_with("inputs/") {
            return ToolResult::err_fmt("source must start with \"inputs/\"");
        }
        if destination.starts_with("inputs/") {
            return ToolResult::err_fmt("destination must not be inside \"inputs/\"");
        }

        let workspace_dir = self.workspace.lock().await.workspace_dir.clone();
        let source_resolved = match resolve_workspace_path(&workspace_dir, &source) {
            Ok(p) => p,
            Err(msg) => return ToolResult::err_fmt(msg),
        };
        let dest_resolved = match resolve_workspace_path(&workspace_dir, &destination) {
            Ok(p) => p,
            Err(msg) => return ToolResult::err_fmt(msg),
        };

        if !tokio::fs::try_exists(&source_resolved)
            .await
            .unwrap_or(false)
        {
            return ToolResult::err_fmt(format_args!("Source file not found: {source}"));
        }

        if let Some(parent) = dest_resolved.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolResult::err_fmt(format_args!(
                    "Failed to create directory {}: {e}",
                    parent.display()
                ));
            }
        }

        match tokio::fs::copy(&source_resolved, &dest_resolved).await {
            Ok(bytes_copied) => ToolResult::ok(json!({
                "source": source,
                "destination": destination,
                "bytesCopied": bytes_copied
            })),
            Err(e) => ToolResult::err_fmt(format_args!("Failed to copy: {e}")),
        }
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
                params: vec![
                    ToolParam::typed("path", "str"),
                    ToolParam::typed("content", "str"),
                ],
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
            ToolDefinition {
                name: "copy_input".into(),
                description: vec![lash_core::ToolText::new(
                    "Copy a file from the inputs/ directory into the workspace so it becomes part of the output artifact. Use this when you want to include a provided asset (image, font, etc.) in your HTML result.",
                    [lash_core::ExecutionMode::NativeTools],
                )],
                params: vec![
                    ToolParam::typed("source", "str"),
                    ToolParam::typed("destination", "str"),
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
            "copy_input" => self.copy_input(args).await,
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
    active_session_id: Uuid,
    active_session_title: &'a str,
    active_session_updated_label: &'a str,
    session_list_html: &'a str,
    gallery_html: &'a str,
    messages_html: &'a str,
    reference_list_html: &'a str,
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
    draft_mode: Option<String>,
    source_ids: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AppQuery {
    session: Option<String>,
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
    asset_ids: Vec<(Uuid, String)>,
    session_id: Option<Uuid>,
    iterates_on_id: Option<Uuid>,
    fallback_title: Option<String>,
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
            asset_ids: Vec::new(),
            session_id: None,
            iterates_on_id: None,
            fallback_title: None,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionMessageRequest {
    body: String,
    reference_ids: Vec<String>,
    iterates_on_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateSessionForm {
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RenameSessionForm {
    title: String,
}

#[derive(Debug, Deserialize)]
struct CreateReferenceForm {
    kind: String,
    title: Option<String>,
    body: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionMessageResponse {
    session_list_html: String,
    messages_html: String,
    gallery_html: String,
    reference_list_html: String,
    active_session_title: String,
    active_session_updated_label: String,
    status: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateDesignSessionResponse {
    session_id: Uuid,
    location: String,
}

#[derive(Debug)]
struct SessionPageSnapshot {
    session_list_html: String,
    messages_html: String,
    gallery_html: String,
    reference_list_html: String,
    active_session_title: String,
    active_session_updated_label: String,
}

#[derive(Debug, Clone)]
struct UploadedAsset {
    id: Uuid,
    url: String,
    file_name: String,
    content_type: String,
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeStartResponse {
    auth_url: String,
}

#[derive(Debug, Deserialize)]
struct ClaudeExchangePayload {
    code: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeExchangeResponse {
    status: &'static str,
    message: Option<String>,
    auth_url: Option<String>,
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

    let artifact_storage = build_artifact_storage(&config).await?;

    let screenshotter = ScreenshotService::new()
        .await
        .expect("failed to launch headless browser for screenshots");
    info!("headless browser ready for screenshots");

    let state = AppState {
        db,
        http: Client::new(),
        artifact_storage,
        screenshotter,
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
        .route("/settings/provider/claude/start", post(start_claude_auth))
        .route(
            "/settings/provider/claude/exchange",
            post(exchange_claude_auth),
        )
        .route("/settings/provider/codex/start", post(start_codex_auth))
        .route("/settings/provider/codex/poll", post(poll_codex_auth))
        .route("/settings/provider/logout", post(disconnect_provider))
        .route("/sessions", post(create_design_session_route))
        .route("/sessions/{id}/rename", post(rename_design_session))
        .route("/sessions/{id}/snapshot", get(session_snapshot))
        .route("/sessions/{id}/messages", post(post_session_message))
        .route("/sessions/{id}/references", post(create_reference_item))
        .route(
            "/sessions/{id}/references/image",
            post(upload_reference_image),
        )
        .route("/storms/{id}", axum::routing::delete(delete_storm_run))
        .route("/storms/{id}/download", get(download_storm_run_archive))
        .route("/telemetry/client", post(client_telemetry))
        .route("/api/storms", get(list_storms).post(create_storm))
        .route("/preview/{run_id}", get(preview_index_redirect))
        .route("/preview/{run_id}/", get(preview_index))
        .route("/preview/{run_id}/{*path}", get(preview_asset))
        .route("/assets", post(upload_asset))
        .route("/assets/{id}/{file_name}", get(serve_asset))
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

async fn index(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
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

async fn app_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AppQuery>,
) -> Result<Response, AppError> {
    let Some(viewer) = current_viewer(&state, &headers).await? else {
        return Ok(Redirect::temporary("/").into_response());
    };

    let provider_panel = render_provider_panel_html(&state, &viewer).await?;
    let active_session =
        resolve_active_session(&state, viewer.id, query.session.as_deref()).await?;
    let snapshot = build_session_snapshot(&state, viewer.id, active_session.id).await?;
    let config_json = json!({
        "clerkPublishableKey": state.config.clerk_publishable_key,
        "appUrl": state.config.app_url,
        "hasServerSession": true,
        "currentPath": "/app",
    })
    .to_string();

    let page = AppTemplate {
        title: "Design Storm / Studio",
        body_class: "app-page",
        datastar_cdn: DATASTAR_CDN,
        viewer: &viewer,
        app_config_json: &config_json,
        provider_panel: &provider_panel,
        active_session_id: active_session.id,
        active_session_title: &snapshot.active_session_title,
        active_session_updated_label: &snapshot.active_session_updated_label,
        session_list_html: &snapshot.session_list_html,
        gallery_html: &snapshot.gallery_html,
        messages_html: &snapshot.messages_html,
        reference_list_html: &snapshot.reference_list_html,
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

async fn create_design_session_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateSessionForm>,
) -> Result<Json<CreateDesignSessionResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let session = create_design_session(&state.db, viewer.id, payload.title.as_deref()).await?;
    Ok(Json(CreateDesignSessionResponse {
        session_id: session.id,
        location: format!("/app?session={}", session.id),
    }))
}

async fn rename_design_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(payload): Json<RenameSessionForm>,
) -> Result<Json<SessionMessageResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    update_design_session_title(&state.db, viewer.id, id, &payload.title).await?;
    Ok(Json(
        build_session_response(&state, viewer.id, id, "Session renamed.").await?,
    ))
}

async fn session_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<SessionMessageResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    Ok(Json(
        build_session_response(&state, viewer.id, id, "").await?,
    ))
}

async fn create_reference_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(payload): Json<CreateReferenceForm>,
) -> Result<Json<SessionMessageResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    create_reference_record(&state.db, viewer.id, id, &payload).await?;
    Ok(Json(
        build_session_response(&state, viewer.id, id, "Reference added.").await?,
    ))
}

async fn upload_reference_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    mut multipart: Multipart,
) -> Result<Json<SessionMessageResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let asset = upload_asset_to_storage(&state, viewer.id, &mut multipart).await?;
    create_image_reference_record(&state.db, viewer.id, id, &asset).await?;
    Ok(Json(
        build_session_response(&state, viewer.id, id, "Image reference added.").await?,
    ))
}

async fn post_session_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    mut multipart: Multipart,
) -> Result<Json<SessionMessageResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let mut payload_json: Option<String> = None;
    let mut images: Vec<Vec<u8>> = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("Multipart error: {e}")))?
    {
        match field.name() {
            Some("payload") => {
                payload_json = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| AppError::BadRequest(format!("Bad payload field: {e}")))?,
                );
            }
            Some("image") => {
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("Bad image field: {e}")))?;
                if !data.is_empty() {
                    images.push(data.to_vec());
                }
            }
            _ => {}
        }
    }
    let payload: SessionMessageRequest = serde_json::from_str(
        payload_json
            .as_deref()
            .ok_or_else(|| AppError::BadRequest("Missing payload field.".to_string()))?,
    )
    .map_err(|e| AppError::BadRequest(format!("Invalid payload JSON: {e}")))?;
    let status = handle_session_message(&state, &viewer, id, payload, images).await?;
    Ok(Json(
        build_session_response(&state, viewer.id, id, &status).await?,
    ))
}

async fn start_claude_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ClaudeStartResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    info!(user_id = %viewer.id, "starting claude oauth");
    let (verifier, challenge) = oauth::generate_pkce();
    let auth_url = oauth::authorize_url(&challenge, &verifier);

    clear_codex_pending(&state.db, viewer.id).await?;
    sqlx::query(
        r#"
        INSERT INTO claude_oauth_sessions (user_id, verifier, auth_url)
        VALUES ($1, $2, $3)
        ON CONFLICT (user_id) DO UPDATE
        SET verifier = EXCLUDED.verifier,
            auth_url = EXCLUDED.auth_url,
            updated_at = now()
        "#,
    )
    .bind(viewer.id)
    .bind(&verifier)
    .bind(&auth_url)
    .execute(&state.db)
    .await?;

    Ok(Json(ClaudeStartResponse { auth_url }))
}

async fn exchange_claude_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ClaudeExchangePayload>,
) -> Result<Json<ClaudeExchangeResponse>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let code = payload.code.trim();
    if code.is_empty() {
        return Ok(Json(ClaudeExchangeResponse {
            status: "error",
            message: Some("Paste the Claude authorization code first.".to_string()),
            auth_url: None,
        }));
    }

    let pending = sqlx::query_as::<_, ClaudeOAuthSessionRow>(
        r#"
        SELECT verifier, auth_url
        FROM claude_oauth_sessions
        WHERE user_id = $1
        "#,
    )
    .bind(viewer.id)
    .fetch_optional(&state.db)
    .await?;

    let Some(pending) = pending else {
        return Ok(Json(ClaudeExchangeResponse {
            status: "error",
            message: Some("Start Claude OAuth again to get a fresh login URL.".to_string()),
            auth_url: None,
        }));
    };

    match oauth::exchange_code(code, &pending.verifier).await {
        Ok(tokens) => {
            let provider = Provider::Claude {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                expires_at: tokens.expires_at,
            };
            save_provider_credentials(&state, viewer.id, &provider).await?;
            clear_provider_pending(&state.db, viewer.id).await?;
            Ok(Json(ClaudeExchangeResponse {
                status: "connected",
                message: Some("Claude is connected.".to_string()),
                auth_url: None,
            }))
        }
        Err(error) => {
            let (verifier, challenge) = oauth::generate_pkce();
            let auth_url = oauth::authorize_url(&challenge, &verifier);
            sqlx::query(
                r#"
                INSERT INTO claude_oauth_sessions (user_id, verifier, auth_url)
                VALUES ($1, $2, $3)
                ON CONFLICT (user_id) DO UPDATE
                SET verifier = EXCLUDED.verifier,
                    auth_url = EXCLUDED.auth_url,
                    updated_at = now()
                "#,
            )
            .bind(viewer.id)
            .bind(&verifier)
            .bind(&auth_url)
            .execute(&state.db)
            .await?;

            Ok(Json(ClaudeExchangeResponse {
                status: "error",
                message: Some(error.to_string()),
                auth_url: Some(auth_url),
            }))
        }
    }
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

    clear_claude_pending(&state.db, viewer.id).await?;

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
            clear_provider_pending(&state.db, viewer.id).await?;

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
    clear_provider_pending(&state.db, viewer.id).await?;
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

async fn delete_storm_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let viewer = require_viewer(&state, &headers).await?;

    if let Some(storage) = &state.artifact_storage {
        if let Err(error) = storage
            .client
            .delete_object()
            .bucket(&storage.bucket)
            .key(artifact_archive_key(viewer.id, id))
            .send()
            .await
        {
            warn!(
                user_id = %viewer.id,
                run_id = %id,
                error = %error,
                "failed to delete persisted artifact archive; continuing with db deletion"
            );
        }
    }

    sqlx::query(
        r#"
        UPDATE storm_runs
        SET parent_ids = array_remove(parent_ids, $2)
        WHERE owner_user_id = $1
        "#,
    )
    .bind(viewer.id)
    .bind(id)
    .execute(&state.db)
    .await?;

    sqlx::query(
        r#"
        UPDATE storm_runs
        SET iterates_on_id = NULL
        WHERE owner_user_id = $1 AND iterates_on_id = $2
        "#,
    )
    .bind(viewer.id)
    .bind(id)
    .execute(&state.db)
    .await?;

    sqlx::query(
        r#"
        UPDATE design_jobs
        SET iterates_on_id = NULL,
            result_run_id = NULL
        WHERE owner_user_id = $1 AND (iterates_on_id = $2 OR result_run_id = $2)
        "#,
    )
    .bind(viewer.id)
    .bind(id)
    .execute(&state.db)
    .await?;

    sqlx::query(
        r#"
        DELETE FROM storm_runs
        WHERE id = $1 AND owner_user_id = $2
        "#,
    )
    .bind(id)
    .bind(viewer.id)
    .execute(&state.db)
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

async fn download_storm_run_archive(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let run = get_owned_run(&state, viewer.id, id).await?;

    let archive_bytes = if tokio::fs::try_exists(&run.workspace_dir)
        .await
        .unwrap_or(false)
    {
        let workspace_dir = run.workspace_dir.clone();
        tokio::task::spawn_blocking(move || zip_workspace_dir(&workspace_dir))
            .await
            .map_err(|error| AppError::Internal(error.to_string()))??
    } else if let Some(bytes) = load_persisted_workspace_archive(&state, viewer.id, id).await? {
        bytes
    } else {
        return Err(AppError::BadRequest(
            "Artifact archive unavailable.".to_string(),
        ));
    };

    let filename = sanitize_download_name(&run.title, id);
    let mut response = Response::new(Body::from(archive_bytes));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|error| AppError::Internal(error.to_string()))?,
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=60"),
    );
    Ok(response)
}

// ─── Asset upload & serving ───

const ALLOWED_ASSET_CONTENT_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/svg+xml",
    "font/woff2",
    "font/ttf",
    "font/otf",
    "font/woff",
    "application/font-woff",
    "application/font-woff2",
    "application/x-font-ttf",
    "application/vnd.ms-opentype",
];

const MAX_ASSET_SIZE: usize = 10 * 1024 * 1024; // 10 MB

async fn upload_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let asset = upload_asset_to_storage(&state, viewer.id, &mut multipart).await?;
    Ok(Json(
        json!({ "id": asset.id.to_string(), "url": asset.url }),
    ))
}

async fn upload_asset_to_storage(
    state: &AppState,
    user_id: Uuid,
    multipart: &mut Multipart,
) -> Result<UploadedAsset, AppError> {
    let storage = state
        .artifact_storage
        .as_ref()
        .ok_or_else(|| AppError::Internal("Asset storage not configured.".to_string()))?;

    let field = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("Invalid multipart request: {e}")))?
        .ok_or_else(|| AppError::BadRequest("No file field in upload.".to_string()))?;

    let file_name = field.file_name().unwrap_or("upload").to_string();
    let content_type = field
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();

    if !ALLOWED_ASSET_CONTENT_TYPES.contains(&content_type.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Unsupported file type: {content_type}"
        )));
    }

    let data = field
        .bytes()
        .await
        .map_err(|e| AppError::BadRequest(format!("Failed to read upload: {e}")))?;

    if data.len() > MAX_ASSET_SIZE {
        return Err(AppError::BadRequest(format!(
            "File too large ({} bytes). Maximum is {} bytes.",
            data.len(),
            MAX_ASSET_SIZE
        )));
    }

    let asset_id = Uuid::new_v4();
    let s3_key = format!("assets/{}/{}/{}", user_id, asset_id, file_name);

    storage
        .client
        .put_object()
        .bucket(&storage.bucket)
        .key(&s3_key)
        .content_type(&content_type)
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to upload asset to S3: {e}")))?;

    sqlx::query(
        "INSERT INTO assets (id, owner_user_id, file_name, content_type, byte_size, s3_key) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(asset_id)
    .bind(user_id)
    .bind(&file_name)
    .bind(&content_type)
    .bind(data.len() as i64)
    .bind(&s3_key)
    .execute(&state.db)
    .await?;

    Ok(UploadedAsset {
        id: asset_id,
        url: format!("/assets/{}/{}", asset_id, file_name),
        file_name,
        content_type,
    })
}

async fn serve_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, _file_name)): Path<(Uuid, String)>,
) -> Result<Response, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let storage = state
        .artifact_storage
        .as_ref()
        .ok_or_else(|| AppError::Internal("Asset storage not configured.".to_string()))?;

    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT s3_key, content_type FROM assets WHERE id = $1 AND owner_user_id = $2",
    )
    .bind(id)
    .bind(viewer.id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::BadRequest("Asset not found.".to_string()))?;

    let (s3_key, content_type) = row;

    let response = storage
        .client
        .get_object()
        .bucket(&storage.bucket)
        .key(&s3_key)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch asset from S3: {e}")))?;

    let bytes = response
        .body
        .collect()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to read asset body: {e}")))?
        .into_bytes()
        .to_vec();

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .header(CACHE_CONTROL, "private, max-age=3600")
        .header(X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from(bytes))
        .unwrap())
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

    if !input.asset_ids.is_empty() {
        let copied =
            copy_assets_to_workspace_inputs(state, viewer.id, &input.asset_ids, &workspace_dir)
                .await?;
        info!(
            user_id = %viewer.id,
            run_id = %run_id,
            asset_count = copied.len(),
            "copied assets to workspace inputs/"
        );
    }

    let preview_url = format!("/preview/{run_id}/");
    let initial_title = input
        .fallback_title
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Storm Artifact".to_string());
    let workspace = Arc::new(Mutex::new(WorkspaceRuntimeState {
        run_id,
        preview_url: preview_url.clone(),
        workspace_dir: workspace_dir.clone(),
        prompt: prompt.clone(),
        title: initial_title,
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
        state.screenshotter.clone(),
        state.config.port,
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
    persist_workspace_archive(state, viewer.id, run_id, &snapshot.workspace_dir).await?;
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
        session_id: input.session_id,
        iterates_on_id: input.iterates_on_id,
        position_x: None,
        position_y: None,
        width: None,
        height: None,
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
    Path(run_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let run = get_run(&state, run_id).await?;
    let html = match tokio::fs::read_to_string(run.workspace_dir.join("index.html")).await {
        Ok(html) => html,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match load_persisted_workspace_entry(&state, run.owner_user_id, run_id, "index.html")
                .await?
            {
                Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                None => {
                    warn!(
                        run_id = %run_id,
                        workspace_dir = %run.workspace_dir.display(),
                        "preview workspace file missing"
                    );
                    render_missing_preview_html(&run)
                }
            }
        }
        Err(error) => return Err(AppError::Io(error)),
    };
    let (html, removed_refresh_tags) = strip_meta_refresh_tags(&html);
    if removed_refresh_tags > 0 {
        warn!(
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
    Path((run_id, path)): Path<(Uuid, String)>,
) -> Result<Response, AppError> {
    let run = get_run(&state, run_id).await?;
    let normalized_path = normalize_workspace_asset_path(&path);
    let asset_path = resolve_workspace_path(&run.workspace_dir, &normalized_path)
        .map_err(AppError::BadRequest)?;
    let bytes = match tokio::fs::read(&asset_path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match load_persisted_workspace_entry(
                &state,
                run.owner_user_id,
                run_id,
                &normalized_path,
            )
            .await?
            {
                Some(bytes) => bytes,
                None => return Ok(StatusCode::NOT_FOUND.into_response()),
            }
        }
        Err(error) => return Err(AppError::Io(error)),
    };
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

async fn list_design_sessions(
    db: &PgPool,
    user_id: Uuid,
) -> Result<Vec<DesignSessionRow>, AppError> {
    Ok(sqlx::query_as::<_, DesignSessionRow>(
        r#"
        SELECT id, owner_user_id, title, created_at, updated_at
        FROM design_sessions
        WHERE owner_user_id = $1
        ORDER BY updated_at DESC, created_at DESC
        "#,
    )
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

async fn get_owned_design_session(
    db: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<DesignSessionRow, AppError> {
    sqlx::query_as::<_, DesignSessionRow>(
        r#"
        SELECT id, owner_user_id, title, created_at, updated_at
        FROM design_sessions
        WHERE id = $1 AND owner_user_id = $2
        "#,
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::BadRequest("Session not found.".to_string()))
}

fn normalize_session_title(input: Option<&str>) -> String {
    let title = input.unwrap_or("").trim();
    if title.is_empty() {
        "Untitled Session".to_string()
    } else {
        truncate_for_log(title, 80)
    }
}

async fn create_design_session(
    db: &PgPool,
    user_id: Uuid,
    title: Option<&str>,
) -> Result<DesignSessionRow, AppError> {
    let title = normalize_session_title(title);
    Ok(sqlx::query_as::<_, DesignSessionRow>(
        r#"
        INSERT INTO design_sessions (owner_user_id, title)
        VALUES ($1, $2)
        RETURNING id, owner_user_id, title, created_at, updated_at
        "#,
    )
    .bind(user_id)
    .bind(title)
    .fetch_one(db)
    .await?)
}

async fn update_design_session_title(
    db: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    title: &str,
) -> Result<(), AppError> {
    let next_title = normalize_session_title(Some(title));
    let updated = sqlx::query(
        r#"
        UPDATE design_sessions
        SET title = $3,
            updated_at = now()
        WHERE id = $1 AND owner_user_id = $2
        "#,
    )
    .bind(session_id)
    .bind(user_id)
    .bind(next_title)
    .execute(db)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::BadRequest("Session not found.".to_string()));
    }
    Ok(())
}

async fn touch_design_session(
    db: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        UPDATE design_sessions
        SET updated_at = now()
        WHERE id = $1 AND owner_user_id = $2
        "#,
    )
    .bind(session_id)
    .bind(user_id)
    .execute(db)
    .await?;
    Ok(())
}

async fn resolve_active_session(
    state: &AppState,
    user_id: Uuid,
    requested: Option<&str>,
) -> Result<DesignSessionRow, AppError> {
    if let Some(session_id) = requested.and_then(|value| Uuid::parse_str(value).ok()) {
        return get_owned_design_session(&state.db, user_id, session_id).await;
    }

    if let Some(session) = sqlx::query_as::<_, DesignSessionRow>(
        r#"
        SELECT id, owner_user_id, title, created_at, updated_at
        FROM design_sessions
        WHERE owner_user_id = $1
        ORDER BY updated_at DESC, created_at DESC
        LIMIT 1
        "#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(session);
    }

    create_design_session(&state.db, user_id, None).await
}

async fn load_session_messages(
    db: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<Vec<SessionMessageRow>, AppError> {
    get_owned_design_session(db, user_id, session_id).await?;
    Ok(sqlx::query_as::<_, SessionMessageRow>(
        r#"
        SELECT id, session_id, role, body, design_job_id, created_at
        FROM session_messages
        WHERE session_id = $1
        ORDER BY created_at ASC
        "#,
    )
    .bind(session_id)
    .fetch_all(db)
    .await?)
}

async fn load_session_references(
    db: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<Vec<ReferenceItemRow>, AppError> {
    get_owned_design_session(db, user_id, session_id).await?;
    Ok(sqlx::query_as::<_, ReferenceItemRow>(
        r#"
        SELECT id, owner_user_id, session_id, kind, title, content_json, created_at
        FROM reference_items
        WHERE owner_user_id = $1 AND session_id = $2
        ORDER BY created_at ASC
        "#,
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_all(db)
    .await?)
}

async fn load_session_jobs(
    db: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<Vec<DesignJobRow>, AppError> {
    get_owned_design_session(db, user_id, session_id).await?;
    Ok(sqlx::query_as::<_, DesignJobRow>(
        r#"
        SELECT id, session_id, owner_user_id, status, prompt, title, iterates_on_id,
               result_run_id, reference_snapshot_json, error, created_at, started_at, completed_at
        FROM design_jobs
        WHERE owner_user_id = $1 AND session_id = $2
        ORDER BY created_at DESC
        "#,
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_all(db)
    .await?)
}

fn session_run_sort_key(run: &StormRunSummary) -> DateTime<Utc> {
    run.created_at
}

fn render_session_list_html(sessions: &[DesignSessionRow], active_session_id: Uuid) -> String {
    if sessions.is_empty() {
        return "<div class=\"session-list-empty\">No sessions yet.</div>".to_string();
    }

    sessions
        .iter()
        .map(|session| {
            let active_class = if session.id == active_session_id { " is-active" } else { "" };
            format!(
                r#"<a class="session-link{active_class}" href="/app?session={id}" data-session-id="{id}">
  <span class="session-link-title">{title}</span>
  <span class="session-link-meta">{label}</span>
</a>"#,
                id = session.id,
                title = html_escape(&session.title),
                label = session.updated_label(),
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

fn render_messages_html(
    messages: &[SessionMessageRow],
    jobs_by_id: &HashMap<Uuid, DesignJobRow>,
) -> String {
    if messages.is_empty() {
        return "<div class=\"chat-empty\">\u{2014}</div>".to_string();
    }

    messages
        .iter()
        .map(|message| {
            let role_class = if message.role == "user" {
                " is-user"
            } else {
                " is-assistant"
            };
            let mut footer = message.created_label();
            if let Some(job_id) = message.design_job_id
                && let Some(job) = jobs_by_id.get(&job_id)
            {
                footer = format!("{footer} · {}", html_escape(&job.status));
            }
            format!(
                r#"<article class="chat-message{role_class}">
  <header class="chat-message-head">
    <span class="chat-message-role">{role}</span>
    <span class="chat-message-meta">{footer}</span>
  </header>
  <div class="chat-message-body">{body}</div>
</article>"#,
                role = if message.role == "user" {
                    "You"
                } else {
                    "Agent"
                },
                footer = footer,
                body = html_escape(&message.body).replace('\n', "<br>"),
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

fn render_reference_list_html(references: &[ReferenceItemRow]) -> String {
    if references.is_empty() {
        return "<div class=\"reference-empty\"></div>".to_string();
    }

    references
        .iter()
        .map(|reference| {
            let handle = format!("ref:{}", reference.id);
            let detail = match reference.kind.as_str() {
                "text" => reference
                    .content_json
                    .get("body")
                    .and_then(|value| value.as_str())
                    .map(|value| truncate_for_log(value, 140))
                    .unwrap_or_else(|| "Text note".to_string()),
                "link" => reference
                    .content_json
                    .get("url")
                    .and_then(|value| value.as_str())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "Link reference".to_string()),
                "image" => reference
                    .content_json
                    .get("url")
                    .and_then(|value| value.as_str())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "Image reference".to_string()),
                _ => "Reference".to_string(),
            };
            let preview = if reference.kind == "image" {
                reference
                    .content_json
                    .get("url")
                    .and_then(|value| value.as_str())
                    .map(|url| {
                        format!(
                            r#"<img class="reference-item-thumb" src="{src}" alt="{alt}" loading="lazy">"#,
                            src = html_escape(url),
                            alt = html_escape(&reference.title),
                        )
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            format!(
                r#"<button class="reference-item" type="button" data-reference-handle="{handle}" data-reference-label="{label}" data-reference-kind="{kind}">
  <span class="reference-item-top">
    <span class="reference-item-kind">{kind}</span>
    <span class="reference-item-date">{date}</span>
  </span>
  {preview}
  <strong class="reference-item-title">{label}</strong>
  <span class="reference-item-detail">{detail}</span>
</button>"#,
                handle = handle,
                label = html_escape(&reference.title),
                kind = html_escape(&reference.kind),
                date = reference.created_at.format("%b %d").to_string(),
                detail = html_escape(&detail),
                preview = preview,
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

fn render_gallery_html(
    runs: &[StormRunSummary],
    jobs: &[DesignJobRow],
    title_lookup: &HashMap<Uuid, String>,
) -> String {
    let pending_cards = jobs.iter().filter(|job| job.result_run_id.is_none()).map(|job| {
        let lineage = job
            .iterates_on_id
            .and_then(|id| title_lookup.get(&id))
            .map(|title| format!(r#"<span class="gallery-lineage">Iterates on {}</span>"#, html_escape(title)))
            .unwrap_or_default();
        let status_class = if job.status == "failed" { " is-failed" } else { "" };
        let error = job
            .error
            .as_ref()
            .map(|message| format!(r#"<p class="gallery-job-error">{}</p>"#, html_escape(message)))
            .unwrap_or_default();
        format!(
            r#"<article class="gallery-card gallery-card-job{status_class}" data-job-id="{id}" data-job-status="{status}">
  <div class="gallery-card-meta">
    <span class="gallery-card-status" data-status="{status}">{status}</span>
    <span class="gallery-card-date">{date}</span>
  </div>
  <div class="gallery-card-body">
    <strong class="gallery-card-title">{title}</strong>
    {lineage}
    <p class="gallery-card-summary">{prompt}</p>
    {error}
  </div>
</article>"#,
            id = job.id,
            status = html_escape(&job.status),
            date = job.created_at.format("%H:%M").to_string(),
            title = html_escape(&job.title),
            prompt = html_escape(&truncate_for_log(&job.prompt, 180)),
            lineage = lineage,
            error = error,
        )
    });

    let design_cards = runs.iter().rev().map(|run| {
        let lineage = run
            .iterates_on_id
            .and_then(|id| title_lookup.get(&id))
            .map(|title| format!(r#"<span class="gallery-lineage">Iterates on {}</span>"#, html_escape(title)))
            .unwrap_or_default();
        format!(
            r#"<article class="gallery-card design-card" data-design-id="{id}" data-design-handle="design:{id}" data-design-label="{title}" tabindex="0">
  <div class="gallery-card-meta">
    <span class="gallery-card-status" data-status="{status}">{status}</span>
    <span class="gallery-card-date">{date}</span>
  </div>
  <div class="gallery-card-preview-shell">
    <iframe class="gallery-card-preview" src="{preview}" title="{title}" loading="lazy" sandbox="allow-scripts allow-forms allow-modals" referrerpolicy="no-referrer"></iframe>
  </div>
  <div class="gallery-card-body">
    <strong class="gallery-card-title">{title}</strong>
    {lineage}
    <p class="gallery-card-summary">{summary}</p>
  </div>
  <div class="gallery-card-actions">
    <button class="gallery-action" type="button" data-action="expand-design" data-design-id="{id}" data-design-label="{title}" data-preview-url="{preview}">Expand</button>
    <button class="gallery-action" type="button" data-action="iterate-design" data-design-id="{id}" data-design-label="{title}">Iterate</button>
    <button class="gallery-action" type="button" data-action="use-design-reference" data-reference-handle="design:{id}" data-reference-label="{title}">Use as reference</button>
    <a class="gallery-action" href="/storms/{id}/download">Download ZIP</a>
  </div>
</article>"#,
            id = run.id,
            title = html_escape(&run.title),
            status = if run.submitted { "Ready" } else { "Draft" },
            date = run.created_label(),
            preview = html_escape(&run.preview_url),
            summary = html_escape(&run.summary),
            lineage = lineage,
        )
    });

    let html = pending_cards
        .chain(design_cards)
        .collect::<Vec<_>>()
        .join("");
    if html.is_empty() {
        "<div class=\"gallery-empty\"></div>"
            .to_string()
    } else {
        html
    }
}

async fn build_session_snapshot(
    state: &AppState,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<SessionPageSnapshot, AppError> {
    let sessions = list_design_sessions(&state.db, user_id).await?;
    let active_session = get_owned_design_session(&state.db, user_id, session_id).await?;
    let messages = load_session_messages(&state.db, user_id, session_id).await?;
    let references = load_session_references(&state.db, user_id, session_id).await?;
    let jobs = load_session_jobs(&state.db, user_id, session_id).await?;
    let all_runs = viewer_run_summaries(state, user_id).await;
    let mut runs = all_runs
        .iter()
        .filter(|run| run.session_id == Some(session_id))
        .cloned()
        .collect::<Vec<_>>();
    runs.sort_by_key(session_run_sort_key);

    let title_lookup = all_runs
        .iter()
        .map(|run| (run.id, run.title.clone()))
        .collect::<HashMap<_, _>>();
    let jobs_by_id = jobs
        .iter()
        .cloned()
        .map(|job| (job.id, job))
        .collect::<HashMap<_, _>>();
    let active_session_updated_label = active_session.updated_label();

    Ok(SessionPageSnapshot {
        session_list_html: render_session_list_html(&sessions, session_id),
        messages_html: render_messages_html(&messages, &jobs_by_id),
        gallery_html: render_gallery_html(&runs, &jobs, &title_lookup),
        reference_list_html: render_reference_list_html(&references),
        active_session_title: active_session.title,
        active_session_updated_label,
    })
}

async fn build_session_response(
    state: &AppState,
    user_id: Uuid,
    session_id: Uuid,
    status: &str,
) -> Result<SessionMessageResponse, AppError> {
    let snapshot = build_session_snapshot(state, user_id, session_id).await?;
    Ok(SessionMessageResponse {
        session_list_html: snapshot.session_list_html,
        messages_html: snapshot.messages_html,
        gallery_html: snapshot.gallery_html,
        reference_list_html: snapshot.reference_list_html,
        active_session_title: snapshot.active_session_title,
        active_session_updated_label: snapshot.active_session_updated_label,
        status: status.to_string(),
    })
}

async fn create_reference_record(
    db: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    payload: &CreateReferenceForm,
) -> Result<(), AppError> {
    get_owned_design_session(db, user_id, session_id).await?;

    let kind = payload.kind.trim().to_lowercase();
    let (title, content_json) = match kind.as_str() {
        "text" => {
            let body = payload.body.as_deref().unwrap_or("").trim().to_string();
            if body.is_empty() {
                return Err(AppError::BadRequest(
                    "Reference text is required.".to_string(),
                ));
            }
            (
                normalize_session_title(payload.title.as_deref().or(Some(&body))),
                json!({ "body": body }),
            )
        }
        "link" => {
            let url = payload.url.as_deref().unwrap_or("").trim().to_string();
            if url.is_empty() {
                return Err(AppError::BadRequest(
                    "Reference URL is required.".to_string(),
                ));
            }
            (
                normalize_session_title(payload.title.as_deref().or(Some(&url))),
                json!({
                    "url": url,
                    "body": payload.body.as_deref().unwrap_or("").trim()
                }),
            )
        }
        _ => {
            return Err(AppError::BadRequest(
                "Reference kind must be text or link.".to_string(),
            ));
        }
    };

    sqlx::query(
        r#"
        INSERT INTO reference_items (owner_user_id, session_id, kind, title, content_json)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(user_id)
    .bind(session_id)
    .bind(kind)
    .bind(title)
    .bind(content_json)
    .execute(db)
    .await?;
    touch_design_session(db, user_id, session_id).await?;
    Ok(())
}

async fn create_image_reference_record(
    db: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    asset: &UploadedAsset,
) -> Result<(), AppError> {
    get_owned_design_session(db, user_id, session_id).await?;
    sqlx::query(
        r#"
        INSERT INTO reference_items (owner_user_id, session_id, kind, title, content_json)
        VALUES ($1, $2, 'image', $3, $4)
        "#,
    )
    .bind(user_id)
    .bind(session_id)
    .bind(normalize_session_title(Some(&asset.file_name)))
    .bind(json!({
        "assetId": asset.id,
        "url": asset.url,
        "contentType": asset.content_type,
        "fileName": asset.file_name,
    }))
    .execute(db)
    .await?;
    touch_design_session(db, user_id, session_id).await?;
    Ok(())
}

fn normalize_reference_handles(handles: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for handle in handles {
        let trimmed = handle.trim();
        if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
            continue;
        }
        normalized.push(trimmed.to_string());
    }
    normalized
}

fn parse_optional_uuid(value: Option<&str>, field: &str) -> Result<Option<Uuid>, AppError> {
    value
        .filter(|candidate| !candidate.trim().is_empty())
        .map(|candidate| {
            Uuid::parse_str(candidate.trim())
                .map_err(|_| AppError::BadRequest(format!("Invalid {field}.")))
        })
        .transpose()
}

fn derive_design_title(prompt: &str) -> String {
    let collapsed = prompt
        .lines()
        .flat_map(|line| line.split_whitespace())
        .take(10)
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        "Untitled Design".to_string()
    } else {
        truncate_for_log(&collapsed, 64)
    }
}

async fn insert_session_message(
    db: &PgPool,
    session_id: Uuid,
    role: &str,
    body: &str,
    design_job_id: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        INSERT INTO session_messages (session_id, role, body, design_job_id)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(session_id)
    .bind(role)
    .bind(body)
    .bind(design_job_id)
    .execute(db)
    .await?;
    Ok(())
}

async fn get_owned_reference_item(
    db: &PgPool,
    user_id: Uuid,
    reference_id: Uuid,
) -> Result<ReferenceItemRow, AppError> {
    sqlx::query_as::<_, ReferenceItemRow>(
        r#"
        SELECT id, owner_user_id, session_id, kind, title, content_json, created_at
        FROM reference_items
        WHERE id = $1 AND owner_user_id = $2
        "#,
    )
    .bind(reference_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::BadRequest("Reference not found.".to_string()))
}

async fn resolve_reference_snapshot(
    state: &AppState,
    user_id: Uuid,
    handles: &[String],
) -> Result<serde_json::Value, AppError> {
    let mut items = Vec::new();
    for handle in handles {
        if let Some(id) = handle.strip_prefix("design:") {
            let run_id = Uuid::parse_str(id)
                .map_err(|_| AppError::BadRequest("Invalid design reference.".to_string()))?;
            let run = get_owned_run(state, user_id, run_id).await?;
            items.push(json!({
                "handle": handle,
                "kind": "design",
                "id": run.id,
                "title": run.title,
                "prompt": run.prompt,
                "summary": run.summary,
                "assistantSummary": run.assistant_summary,
                "previewUrl": run.preview_url,
            }));
            continue;
        }
        if let Some(id) = handle.strip_prefix("ref:") {
            let reference_id = Uuid::parse_str(id)
                .map_err(|_| AppError::BadRequest("Invalid reference handle.".to_string()))?;
            let reference = get_owned_reference_item(&state.db, user_id, reference_id).await?;
            items.push(json!({
                "handle": handle,
                "kind": reference.kind,
                "id": reference.id,
                "title": reference.title,
                "content": reference.content_json,
            }));
            continue;
        }
        return Err(AppError::BadRequest(
            "Unknown reference handle.".to_string(),
        ));
    }
    Ok(serde_json::Value::Array(items))
}

fn render_reference_snapshot_for_prompt(snapshot: &serde_json::Value) -> String {
    let Some(items) = snapshot.as_array() else {
        return "None".to_string();
    };
    if items.is_empty() {
        return "None".to_string();
    }
    items
        .iter()
        .map(|item| {
            let kind = item
                .get("kind")
                .and_then(|value| value.as_str())
                .unwrap_or("reference");
            let handle = item
                .get("handle")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let title = item
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or("Untitled");
            match kind {
                "design" => format!(
                    "- {handle} [design]\n  title: {title}\n  summary: {}\n  prompt: {}",
                    item.get("summary")
                        .and_then(|value| value.as_str())
                        .unwrap_or(""),
                    truncate_for_log(
                        item.get("prompt")
                            .and_then(|value| value.as_str())
                            .unwrap_or(""),
                        240
                    ),
                ),
                "text" => format!(
                    "- {handle} [text]\n  title: {title}\n  body: {}",
                    truncate_for_log(
                        item.get("content")
                            .and_then(|value| value.get("body"))
                            .and_then(|value| value.as_str())
                            .unwrap_or(""),
                        240,
                    ),
                ),
                "link" => format!(
                    "- {handle} [link]\n  title: {title}\n  url: {}\n  note: {}",
                    item.get("content")
                        .and_then(|value| value.get("url"))
                        .and_then(|value| value.as_str())
                        .unwrap_or(""),
                    truncate_for_log(
                        item.get("content")
                            .and_then(|value| value.get("body"))
                            .and_then(|value| value.as_str())
                            .unwrap_or(""),
                        160,
                    ),
                ),
                "image" => format!(
                    "- {handle} [image]\n  title: {title}\n  asset url: {}",
                    item.get("content")
                        .and_then(|value| value.get("url"))
                        .and_then(|value| value.as_str())
                        .unwrap_or(""),
                ),
                _ => format!("- {handle} [{kind}] {title}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_generation_prompt(
    user_prompt: &str,
    reference_snapshot: &serde_json::Value,
    iterates_on: Option<&StormRunRecord>,
) -> (String, Vec<Uuid>, Vec<(Uuid, String)>) {
    let mut sections = vec![user_prompt.trim().to_string()];
    let mut source_ids = Vec::new();
    let mut asset_ids = Vec::new();
    let mut seen_sources = HashSet::new();

    if let Some(parent) = iterates_on {
        if seen_sources.insert(parent.id) {
            source_ids.push(parent.id);
        }
        sections.push(format!(
            "Primary iteration target:\n- title: {}\n- prompt: {}\n- summary: {}",
            parent.title, parent.prompt, parent.summary
        ));
    }

    if let Some(items) = reference_snapshot.as_array()
        && !items.is_empty()
    {
        sections.push("Selected references:".to_string());
        for item in items {
            let kind = item
                .get("kind")
                .and_then(|value| value.as_str())
                .unwrap_or("reference");
            match kind {
                "design" => {
                    if let Some(id) = item
                        .get("id")
                        .and_then(|value| value.as_str())
                        .and_then(|value| Uuid::parse_str(value).ok())
                        && seen_sources.insert(id)
                    {
                        source_ids.push(id);
                    }
                    sections.push(format!(
                        "- design {}: {}\n  summary: {}\n  prompt: {}",
                        item.get("title")
                            .and_then(|value| value.as_str())
                            .unwrap_or("Untitled"),
                        item.get("handle")
                            .and_then(|value| value.as_str())
                            .unwrap_or(""),
                        item.get("summary")
                            .and_then(|value| value.as_str())
                            .unwrap_or(""),
                        truncate_for_log(
                            item.get("prompt")
                                .and_then(|value| value.as_str())
                                .unwrap_or(""),
                            240
                        ),
                    ));
                }
                "text" => {
                    sections.push(format!(
                        "- text note {}: {}",
                        item.get("title")
                            .and_then(|value| value.as_str())
                            .unwrap_or("Untitled"),
                        item.get("content")
                            .and_then(|value| value.get("body"))
                            .and_then(|value| value.as_str())
                            .unwrap_or(""),
                    ));
                }
                "link" => {
                    sections.push(format!(
                        "- link {}: {}\n  note: {}",
                        item.get("title")
                            .and_then(|value| value.as_str())
                            .unwrap_or("Untitled"),
                        item.get("content")
                            .and_then(|value| value.get("url"))
                            .and_then(|value| value.as_str())
                            .unwrap_or(""),
                        item.get("content")
                            .and_then(|value| value.get("body"))
                            .and_then(|value| value.as_str())
                            .unwrap_or(""),
                    ));
                }
                "image" => {
                    let title = item
                        .get("title")
                        .and_then(|value| value.as_str())
                        .unwrap_or("Image");
                    let asset_id = item
                        .get("content")
                        .and_then(|value| value.get("assetId"))
                        .and_then(|value| value.as_str())
                        .and_then(|value| Uuid::parse_str(value).ok());
                    if let Some(asset_id) = asset_id {
                        asset_ids.push((asset_id, title.to_string()));
                    }
                    sections.push(format!(
                        "- image reference {} available in inputs/ when useful",
                        title,
                    ));
                }
                _ => {}
            }
        }
    }

    (sections.join("\n\n"), source_ids, asset_ids)
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

async fn viewer_run_summaries(state: &AppState, user_id: Uuid) -> Vec<StormRunSummary> {
    match sqlx::query_as::<_, StormRunRow>(
        r#"
        SELECT id, owner_user_id, prompt, title, summary, assistant_summary, preview_url,
               submitted, created_at, workspace_dir, parent_ids, session_id, iterates_on_id,
               position_x, position_y, width, height
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
async fn provider_status_view(
    state: &AppState,
    user_id: Uuid,
) -> Result<ProviderStatusView, AppError> {
    let codex_pending = sqlx::query_as::<_, CodexDeviceAuthRow>(
        r#"
        SELECT device_auth_id, user_code
        FROM codex_device_auth_sessions
        WHERE user_id = $1
        "#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let claude_pending = sqlx::query_as::<_, ClaudeOAuthSessionRow>(
        r#"
        SELECT verifier, auth_url
        FROM claude_oauth_sessions
        WHERE user_id = $1
        "#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    match load_provider_for_user(state, user_id).await? {
        Some(provider) => {
            let (label, detail, connected_kind) = match &provider.provider {
                Provider::Codex { account_id, .. } => (
                    "Codex OAuth".to_string(),
                    account_id
                        .as_ref()
                        .map(|account_id| format!("Connected via OpenAI account {account_id}."))
                        .unwrap_or_else(|| "Connected via OpenAI device OAuth.".to_string()),
                    Some("codex".to_string()),
                ),
                Provider::OpenAiGeneric { base_url, .. } => (
                    if matches!(provider.source, ProviderSource::ServerFallback) {
                        "Server Provider".to_string()
                    } else {
                        "OpenAI-Compatible".to_string()
                    },
                    format!("Requests route through {base_url}."),
                    None,
                ),
                Provider::Claude { .. } => (
                    "Claude OAuth".to_string(),
                    "Anthropic OAuth tokens are stored for this account.".to_string(),
                    Some("claude".to_string()),
                ),
                Provider::GoogleOAuth { .. } => (
                    "Google OAuth".to_string(),
                    "Google OAuth tokens are stored for this account.".to_string(),
                    None,
                ),
            };
            let selected_kind = connected_kind
                .clone()
                .or_else(|| claude_pending.as_ref().map(|_| "claude".to_string()))
                .or_else(|| codex_pending.as_ref().map(|_| "codex".to_string()))
                .unwrap_or_else(|| "codex".to_string());
            Ok(ProviderStatusView {
                connected: true,
                using_fallback: matches!(provider.source, ProviderSource::ServerFallback),
                label,
                detail,
                updated_label: provider
                    .updated_at
                    .map(|timestamp| format!("Updated {}", timestamp.format("%Y-%m-%d %H:%M UTC")))
                    .unwrap_or_else(|| "Server-scoped fallback".to_string()),
                selected_kind,
                connected_kind,
                codex_pending_user_code: codex_pending.map(|pending| pending.user_code),
                claude_pending_auth_url: claude_pending.map(|pending| pending.auth_url),
            })
        }
        None => Ok(ProviderStatusView {
            connected: false,
            using_fallback: false,
            label: "No provider connected".to_string(),
            detail:
                "Connect Codex or Claude here. The site stores encrypted OAuth tokens server-side and uses them for storm generation."
                    .to_string(),
            updated_label: "No stored runtime credentials".to_string(),
            selected_kind: if claude_pending.is_some() {
                "claude".to_string()
            } else {
                "codex".to_string()
            },
            connected_kind: None,
            codex_pending_user_code: codex_pending.map(|pending| pending.user_code),
            claude_pending_auth_url: claude_pending.map(|pending| pending.auth_url),
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
        let mut stored =
            decrypt_provider_config(&state.config.session_secret, &row.encrypted_config)?;
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

async fn clear_claude_pending(db: &PgPool, user_id: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM claude_oauth_sessions WHERE user_id = $1")
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(())
}

async fn clear_provider_pending(db: &PgPool, user_id: Uuid) -> Result<(), AppError> {
    clear_codex_pending(db, user_id).await?;
    clear_claude_pending(db, user_id).await?;
    Ok(())
}

fn encrypt_provider_config(
    secret: &str,
    config: &StoredProviderConfig,
) -> Result<String, AppError> {
    let plaintext = serde_json::to_vec(config)?;
    let key = derive_cipher_key(secret);
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|error| AppError::Internal(error.to_string()))?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_ref())
        .map_err(|error| AppError::Internal(error.to_string()))?;
    let mut combined = nonce.to_vec();
    combined.extend(ciphertext);
    Ok(STANDARD.encode(combined))
}

fn decrypt_provider_config(
    secret: &str,
    encrypted: &str,
) -> Result<StoredProviderConfig, AppError> {
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
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|error| AppError::Internal(error.to_string()))?;
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

async fn copy_assets_to_workspace_inputs(
    state: &AppState,
    user_id: Uuid,
    asset_ids: &[(Uuid, String)],
    workspace_dir: &StdPath,
) -> Result<Vec<(String, String)>, AppError> {
    if asset_ids.is_empty() {
        return Ok(Vec::new());
    }
    let storage = match &state.artifact_storage {
        Some(s) => s,
        None => return Ok(Vec::new()),
    };
    let inputs_dir = workspace_dir.join("inputs");
    tokio::fs::create_dir_all(&inputs_dir).await?;

    let mut results = Vec::new();
    let mut used_names = std::collections::HashSet::new();

    for (asset_id, description) in asset_ids {
        let row = sqlx::query_as::<_, (String, String)>(
            "SELECT s3_key, file_name FROM assets WHERE id = $1 AND owner_user_id = $2",
        )
        .bind(asset_id)
        .bind(user_id)
        .fetch_optional(&state.db)
        .await?;

        let Some((s3_key, file_name)) = row else {
            warn!(asset_id = %asset_id, "asset not found when copying to workspace inputs");
            continue;
        };

        let response = match storage
            .client
            .get_object()
            .bucket(&storage.bucket)
            .key(&s3_key)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(asset_id = %asset_id, error = %e, "failed to fetch asset from S3");
                continue;
            }
        };

        let bytes = response
            .body
            .collect()
            .await
            .map_err(|e| AppError::Internal(format!("Failed to read asset body: {e}")))?
            .into_bytes()
            .to_vec();

        // Disambiguate duplicate filenames
        let mut dest_name = file_name.clone();
        if used_names.contains(&dest_name) {
            let short_id = &asset_id.to_string()[..8];
            let path = StdPath::new(&file_name);
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            dest_name = if ext.is_empty() {
                format!("{stem}_{short_id}")
            } else {
                format!("{stem}_{short_id}.{ext}")
            };
        }
        used_names.insert(dest_name.clone());

        let dest_path = inputs_dir.join(&dest_name);
        tokio::fs::write(&dest_path, &bytes).await?;

        let relative = format!("inputs/{dest_name}");
        results.push((relative, description.clone()));
    }

    Ok(results)
}

async fn get_run(state: &AppState, run_id: Uuid) -> Result<StormRunRecord, AppError> {
    let run = sqlx::query_as::<_, StormRunRow>(
        r#"
        SELECT id, owner_user_id, prompt, title, summary, assistant_summary, preview_url,
               submitted, created_at, workspace_dir, parent_ids, session_id, iterates_on_id,
               position_x, position_y, width, height
        FROM storm_runs
        WHERE id = $1
        "#,
    )
    .bind(run_id)
    .fetch_optional(&state.db)
    .await?;

    run.map(StormRunRecord::from)
        .ok_or_else(|| AppError::BadRequest("Storm run not found.".to_string()))
}

async fn get_owned_run(
    state: &AppState,
    user_id: Uuid,
    run_id: Uuid,
) -> Result<StormRunRecord, AppError> {
    let run = get_run(state, run_id).await?;
    if run.owner_user_id != user_id {
        return Err(AppError::BadRequest("Storm run not found.".to_string()));
    }
    Ok(run)
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
            parent_ids,
            session_id,
            iterates_on_id,
            position_x,
            position_y,
            width,
            height
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
        ON CONFLICT (id) DO UPDATE
        SET prompt = EXCLUDED.prompt,
            title = EXCLUDED.title,
            summary = EXCLUDED.summary,
            assistant_summary = EXCLUDED.assistant_summary,
            preview_url = EXCLUDED.preview_url,
            submitted = EXCLUDED.submitted,
            created_at = EXCLUDED.created_at,
            workspace_dir = EXCLUDED.workspace_dir,
            parent_ids = EXCLUDED.parent_ids,
            session_id = EXCLUDED.session_id,
            iterates_on_id = EXCLUDED.iterates_on_id,
            position_x = EXCLUDED.position_x,
            position_y = EXCLUDED.position_y,
            width = EXCLUDED.width,
            height = EXCLUDED.height
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
    .bind(record.session_id)
    .bind(record.iterates_on_id)
    .bind(record.position_x)
    .bind(record.position_y)
    .bind(record.width)
    .bind(record.height)
    .execute(db)
    .await?;

    Ok(())
}

#[derive(Debug, Clone)]
struct StudioMakeDesignCall {
    prompt: String,
    reference_handles: Vec<String>,
    iterates_on_id: Option<Uuid>,
    title: Option<String>,
}

struct SessionToolProvider {
    state: AppState,
    user_id: Uuid,
    session_id: Uuid,
    allowed_reference_handles: Vec<String>,
    default_iterates_on_id: Option<Uuid>,
    pending_call: Arc<Mutex<Option<StudioMakeDesignCall>>>,
}

impl SessionToolProvider {
    fn new(
        state: AppState,
        user_id: Uuid,
        session_id: Uuid,
        allowed_reference_handles: Vec<String>,
        default_iterates_on_id: Option<Uuid>,
        pending_call: Arc<Mutex<Option<StudioMakeDesignCall>>>,
    ) -> Self {
        Self {
            state,
            user_id,
            session_id,
            allowed_reference_handles,
            default_iterates_on_id,
            pending_call,
        }
    }

    fn db(&self) -> &PgPool {
        &self.state.db
    }

    async fn list_designs(&self) -> ToolResult {
        let rows = match sqlx::query_as::<_, StormRunRow>(
            r#"
            SELECT id, owner_user_id, prompt, title, summary, assistant_summary, preview_url,
                   submitted, created_at, workspace_dir, parent_ids, session_id, iterates_on_id,
                   position_x, position_y, width, height
            FROM storm_runs
            WHERE owner_user_id = $1 AND session_id = $2
            ORDER BY created_at ASC
            "#,
        )
        .bind(self.user_id)
        .bind(self.session_id)
        .fetch_all(self.db())
        .await
        {
            Ok(rows) => rows,
            Err(e) => return ToolResult::err_fmt(format_args!("Failed to load designs: {e}")),
        };

        let designs: Vec<serde_json::Value> = rows
            .into_iter()
            .map(StormRunRecord::from)
            .map(|run| {
                json!({
                    "id": run.id,
                    "handle": format!("design:{}", run.id),
                    "title": run.title,
                    "summary": run.summary,
                    "previewUrl": run.preview_url,
                    "iteratesOnId": run.iterates_on_id,
                    "createdAt": run.created_at.to_rfc3339(),
                })
            })
            .collect();

        ToolResult::ok(json!({ "designs": designs, "count": designs.len() }))
    }

    async fn list_references(&self) -> ToolResult {
        let rows = match load_session_references(self.db(), self.user_id, self.session_id).await {
            Ok(rows) => rows,
            Err(e) => {
                return ToolResult::err_fmt(format_args!("Failed to load references: {e}"))
            }
        };

        let refs: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "handle": format!("ref:{}", r.id),
                    "kind": r.kind,
                    "title": r.title,
                    "createdAt": r.created_at.to_rfc3339(),
                })
            })
            .collect();

        ToolResult::ok(json!({ "references": refs, "count": refs.len() }))
    }

    async fn view_design(&self, args: &serde_json::Value) -> ToolResult {
        let Some(design_id_str) = args
            .get("design_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            return ToolResult::err_fmt("Missing required parameter: design_id");
        };
        let design_id = match Uuid::parse_str(design_id_str) {
            Ok(id) => id,
            Err(_) => return ToolResult::err_fmt("design_id must be a valid UUID"),
        };
        let run = match sqlx::query_as::<_, StormRunRow>(
            r#"
            SELECT id, owner_user_id, prompt, title, summary, assistant_summary, preview_url,
                   submitted, created_at, workspace_dir, parent_ids, session_id, iterates_on_id,
                   position_x, position_y, width, height
            FROM storm_runs
            WHERE id = $1 AND owner_user_id = $2
            "#,
        )
        .bind(design_id)
        .bind(self.user_id)
        .fetch_optional(self.db())
        .await
        {
            Ok(Some(row)) => StormRunRecord::from(row),
            Ok(None) => return ToolResult::err_fmt("Design not found"),
            Err(e) => return ToolResult::err_fmt(format_args!("DB error: {e}")),
        };

        let result = json!({
            "id": run.id,
            "handle": format!("design:{}", run.id),
            "title": run.title,
            "summary": run.summary,
            "assistantSummary": run.assistant_summary,
            "prompt": run.prompt,
            "previewUrl": run.preview_url,
            "iteratesOnId": run.iterates_on_id,
            "createdAt": run.created_at.to_rfc3339(),
        });

        let url = format!(
            "http://127.0.0.1:{}{}",
            self.state.config.port, run.preview_url
        );
        let png = self.state.screenshotter.screenshot(&url).await
            .map_err(|e| format!("screenshot failed: {e}")).unwrap();
        let image = lash_core::ToolImage {
            mime: "image/png".to_string(),
            data: png,
            label: format!("{} — screenshot", run.title),
        };
        ToolResult::with_images(true, result, vec![image])
    }

    async fn view_reference(&self, args: &serde_json::Value) -> ToolResult {
        let Some(ref_id_str) = args
            .get("reference_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            return ToolResult::err_fmt("Missing required parameter: reference_id");
        };
        let ref_id = match Uuid::parse_str(ref_id_str) {
            Ok(id) => id,
            Err(_) => return ToolResult::err_fmt("reference_id must be a valid UUID"),
        };
        let reference = match get_owned_reference_item(self.db(), self.user_id, ref_id).await {
            Ok(r) => r,
            Err(_) => return ToolResult::err_fmt("Reference not found"),
        };

        let mut result = json!({
            "id": reference.id,
            "handle": format!("ref:{}", reference.id),
            "kind": reference.kind,
            "title": reference.title,
            "content": reference.content_json,
        });

        // For image references, try to load the image data from S3 and return it
        if reference.kind == "image" {
            if let Some(asset_id_str) = reference.content_json.get("assetId").and_then(|v| v.as_str()) {
                let mime = reference
                    .content_json
                    .get("contentType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("image/png")
                    .to_string();
                if let Ok(asset_id) = Uuid::parse_str(asset_id_str) {
                    if let Some(storage) = self.state.artifact_storage.as_ref() {
                        let s3_key_row = sqlx::query_as::<_, (String,)>(
                            "SELECT s3_key FROM assets WHERE id = $1 AND owner_user_id = $2",
                        )
                        .bind(asset_id)
                        .bind(self.user_id)
                        .fetch_optional(self.db())
                        .await;
                        if let Ok(Some((s3_key,))) = s3_key_row {
                            if let Ok(response) = storage
                                .client
                                .get_object()
                                .bucket(&storage.bucket)
                                .key(&s3_key)
                                .send()
                                .await
                            {
                                if let Ok(body) = response.body.collect().await {
                                    let data = body.into_bytes().to_vec();
                                    let image = lash_core::ToolImage {
                                        mime,
                                        data,
                                        label: reference.title.clone(),
                                    };
                                    return ToolResult::with_images(true, result, vec![image]);
                                }
                            }
                        }
                    }
                }
                result["note"] = json!("Image data could not be loaded, but the asset metadata is available.");
            }
        }

        ToolResult::ok(result)
    }

    async fn make_design(&self, args: &serde_json::Value) -> ToolResult {
        let Some(prompt) = args
            .get("prompt")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return ToolResult::err_fmt("Missing required parameter: prompt");
        };

        let title = args
            .get("title")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);

        let mut reference_handles = args
            .get("reference_ids")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| self.allowed_reference_handles.clone());
        reference_handles = normalize_reference_handles(reference_handles);

        for handle in &reference_handles {
            if !self
                .allowed_reference_handles
                .iter()
                .any(|allowed| allowed == handle)
            {
                return ToolResult::err_fmt(format_args!(
                    "Unknown reference handle: {handle}. Use only handles from the selected references."
                ));
            }
        }

        let iterates_on_id = match args.get("iterates_on_id").and_then(|value| value.as_str()) {
            Some(value) if !value.trim().is_empty() => match Uuid::parse_str(value.trim()) {
                Ok(parsed) => Some(parsed),
                Err(_) => return ToolResult::err_fmt("iterates_on_id must be a UUID string"),
            },
            _ => self.default_iterates_on_id,
        };

        let mut pending = self.pending_call.lock().await;
        if pending.is_some() {
            return ToolResult::err_fmt("make_design can only be called once per turn");
        }
        *pending = Some(StudioMakeDesignCall {
            prompt: prompt.to_string(),
            reference_handles,
            iterates_on_id,
            title: title.clone(),
        });

        ToolResult::ok(json!({
            "queued": true,
            "sessionId": self.session_id,
            "title": title.unwrap_or_else(|| derive_design_title(prompt)),
            "iteratesOnId": iterates_on_id,
        }))
    }
}

#[async_trait::async_trait]
impl ToolProvider for SessionToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let native = [lash_core::ExecutionMode::NativeTools];
        vec![
            ToolDefinition {
                name: "list_designs".into(),
                description: vec![lash_core::ToolText::new(
                    "List all designs created in this session. Returns id, title, summary, and preview URL for each design.",
                    native,
                )],
                params: vec![],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
            ToolDefinition {
                name: "list_references".into(),
                description: vec![lash_core::ToolText::new(
                    "List all references attached to this session (notes, links, images). Returns id, kind, and title for each.",
                    native,
                )],
                params: vec![],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
            ToolDefinition {
                name: "view_design".into(),
                description: vec![lash_core::ToolText::new(
                    "View a design by taking a live screenshot of the rendered page. Also returns metadata (title, summary, prompt). Falls back to HTML/CSS source excerpts if screenshots are unavailable. Use the design id from list_designs.",
                    native,
                )],
                params: vec![ToolParam::typed("design_id", "str")],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
            ToolDefinition {
                name: "view_reference".into(),
                description: vec![lash_core::ToolText::new(
                    "View a specific reference's full content. For text references returns the body, for links the URL and notes, for images returns the image data. Use the reference id from list_references.",
                    native,
                )],
                params: vec![ToolParam::typed("reference_id", "str")],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
            ToolDefinition {
                name: "make_design".into(),
                description: vec![lash_core::ToolText::new(
                    "Queue one asynchronous design generation job for this session. Use it when the user wants new designs, refinements, branches, or iterations. Pass only reference_ids from the selected handles listed in the prompt.",
                    native,
                )],
                params: vec![
                    ToolParam::typed("prompt", "str"),
                    ToolParam::optional("reference_ids", "list[str]"),
                    ToolParam::optional("iterates_on_id", "str"),
                    ToolParam::optional("title", "str"),
                ],
                returns: "dict".into(),
                examples: vec![],
                hidden: false,
                inject_into_prompt: true,
            },
        ]
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolResult {
        info!(
            session_id = %self.session_id,
            tool = name,
            args = %summarize_tool_args(args),
            "session tool call started"
        );
        let started = Instant::now();
        let result = match name {
            "list_designs" => self.list_designs().await,
            "list_references" => self.list_references().await,
            "view_design" => self.view_design(args).await,
            "view_reference" => self.view_reference(args).await,
            "make_design" => self.make_design(args).await,
            _ => ToolResult::err_fmt(format_args!("Unknown tool: {name}")),
        };
        info!(
            session_id = %self.session_id,
            tool = name,
            elapsed_ms = started.elapsed().as_millis(),
            success = result.success,
            result = %truncate_for_log(&result.result.to_string(), 320),
            "session tool call finished"
        );
        result
    }
}

fn session_prompt_overrides() -> Vec<PromptSectionOverride> {
    vec![
        PromptSectionOverride {
            section: PromptSectionName::Identity,
            mode: PromptOverrideMode::Replace,
            content: "You are Design Storm Director, a chat-forward design agent that helps steer a session and can queue async design jobs.".to_string(),
        },
        PromptSectionOverride {
            section: PromptSectionName::Personality,
            mode: PromptOverrideMode::Replace,
            content: "Be direct, visually literate, and concise. Prefer concrete design language over generic product phrasing.".to_string(),
        },
        PromptSectionOverride {
            section: PromptSectionName::ExecutionContract,
            mode: PromptOverrideMode::Replace,
            content: "Either answer conversationally, use read-only tools to inspect session context, or call make_design exactly once when the user is asking for designs to be made, refined, branched, or iterated. Keep the final reply short.".to_string(),
        },
        PromptSectionOverride {
            section: PromptSectionName::ToolAccess,
            mode: PromptOverrideMode::Replace,
            content: "Your tools: list_designs (list session designs), list_references (list session references), view_design (screenshot a rendered design + metadata), view_reference (view a reference's content — returns image data for image refs), make_design (queue a design job — max one per turn). There is no shell, filesystem, or hidden workspace. Never invent reference handles.".to_string(),
        },
        PromptSectionOverride {
            section: PromptSectionName::Guidelines,
            mode: PromptOverrideMode::Replace,
            content: "Use list_designs/list_references to explore session context when the user asks about existing work. Use view_design/view_reference to inspect specific items. Use the selected references when they matter. If the user is still discussing intent, respond without calling make_design. If the user wants output now, queue a job.".to_string(),
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

fn compose_session_agent_prompt(
    messages: &[SessionMessageRow],
    reference_snapshot: &serde_json::Value,
    iterates_on: Option<&StormRunRecord>,
) -> String {
    let transcript = messages
        .iter()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|message| {
            format!(
                "{}: {}",
                if message.role == "user" {
                    "User"
                } else {
                    "Assistant"
                },
                message.body
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let references = render_reference_snapshot_for_prompt(reference_snapshot);
    let iterates_on_text = iterates_on
        .map(|run| {
            format!(
                "Default iteration target:\n- id: {}\n- title: {}\n- summary: {}\n- prompt: {}",
                run.id,
                run.title,
                run.summary,
                truncate_for_log(&run.prompt, 240)
            )
        })
        .unwrap_or_else(|| "Default iteration target: none".to_string());

    format!(
        "You are replying inside a single design session.\n\nRules:\n- If the user wants new output, variations, a branch, or a refinement, call make_design exactly once.\n- If the user is only discussing direction or asking a question, reply without calling tools.\n- Keep the final assistant reply to one or two sentences.\n\nSelected reference handles and details:\n{references}\n\n{iterates_on_text}\n\nRecent conversation:\n{transcript}"
    )
}

async fn run_session_agent(
    state: &AppState,
    viewer: &Viewer,
    session_id: Uuid,
    messages: &[SessionMessageRow],
    selected_reference_handles: Vec<String>,
    reference_snapshot: &serde_json::Value,
    iterates_on: Option<&StormRunRecord>,
    images: Vec<Vec<u8>>,
) -> Result<(lash_core::AssembledTurn, Option<StudioMakeDesignCall>), AppError> {
    let loaded_provider = load_provider_for_user(state, viewer.id).await?;
    let Some(loaded_provider) = loaded_provider else {
        return Err(AppError::BadRequest(
            "Connect Codex in settings before chatting with the design agent.".to_string(),
        ));
    };

    let pending_call = Arc::new(Mutex::new(None));
    let provider = SessionToolProvider::new(
        state.clone(),
        viewer.id,
        session_id,
        selected_reference_handles,
        iterates_on.map(|run| run.id),
        pending_call.clone(),
    );
    let tools = Arc::new(ToolSet::new() + provider);
    let config = RuntimeConfig {
        capabilities: build_runtime_capabilities(false),
        model: loaded_provider
            .provider
            .default_agent_model("high")
            .map(|(model, _)| model.to_string())
            .unwrap_or_else(|| {
                state
                    .config
                    .storm_model
                    .clone()
                    .unwrap_or_else(|| loaded_provider.provider.default_model().to_string())
            }),
        provider: loaded_provider.provider,
        execution_mode: lash_core::ExecutionMode::NativeTools,
        host_profile: HostProfile::Embedded,
        headless: true,
        session_id: Some(format!("studio-{}", Uuid::new_v4())),
        prompt_overrides: session_prompt_overrides(),
        ..RuntimeConfig::default()
    };
    let mut engine = LashRuntime::from_state(config, tools, AgentStateEnvelope::default())
        .await
        .map_err(|error| AppError::Internal(error.to_string()))?;
    let mut items: Vec<InputItem> = vec![InputItem::Text {
        text: compose_session_agent_prompt(messages, reference_snapshot, iterates_on),
    }];
    let mut image_blobs = HashMap::new();
    for (i, blob) in images.into_iter().enumerate() {
        let id = format!("img-{}", i + 1);
        items.push(InputItem::ImageRef { id: id.clone() });
        image_blobs.insert(id, blob);
    }
    let input = TurnInput {
        items,
        image_blobs,
        mode: None,
        plan_file: None,
    };
    let turn = engine.run_turn_assembled(input, CancellationToken::new());
    tokio::pin!(turn);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
        tokio::select! {
            result = &mut turn => {
                break result.map_err(|error| AppError::Internal(error.to_string()))?;
            }
            _ = heartbeat.tick() => {
                info!(session_id = %session_id, user_id = %viewer.id, "session agent still running");
            }
        }
    };
    let call = pending_call.lock().await.clone();
    Ok((result, call))
}

async fn create_design_job_record(
    db: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    call: &StudioMakeDesignCall,
    reference_snapshot_json: serde_json::Value,
) -> Result<DesignJobRow, AppError> {
    let title = call
        .title
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| derive_design_title(&call.prompt));
    let row = sqlx::query_as::<_, DesignJobRow>(
        r#"
        INSERT INTO design_jobs (
            session_id,
            owner_user_id,
            status,
            prompt,
            title,
            iterates_on_id,
            reference_snapshot_json
        )
        VALUES ($1, $2, 'pending', $3, $4, $5, $6)
        RETURNING id, session_id, owner_user_id, status, prompt, title, iterates_on_id,
                  result_run_id, reference_snapshot_json, error, created_at, started_at, completed_at
        "#,
    )
    .bind(session_id)
    .bind(user_id)
    .bind(&call.prompt)
    .bind(title)
    .bind(call.iterates_on_id)
    .bind(reference_snapshot_json)
    .fetch_one(db)
    .await?;
    touch_design_session(db, user_id, session_id).await?;
    Ok(row)
}

async fn load_design_job_record(
    db: &PgPool,
    user_id: Uuid,
    job_id: Uuid,
) -> Result<Option<DesignJobRow>, AppError> {
    Ok(sqlx::query_as::<_, DesignJobRow>(
        r#"
        SELECT id, session_id, owner_user_id, status, prompt, title, iterates_on_id,
               result_run_id, reference_snapshot_json, error, created_at, started_at, completed_at
        FROM design_jobs
        WHERE id = $1 AND owner_user_id = $2
        "#,
    )
    .bind(job_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?)
}

fn spawn_design_job_runner(state: AppState, viewer: Viewer, job_id: Uuid) {
    tokio::spawn(async move {
        if let Err(error) = run_design_job(state.clone(), viewer.clone(), job_id).await {
            error!(user_id = %viewer.id, job_id = %job_id, error = %error, "design job worker failed");
            let _ = sqlx::query(
                r#"
                UPDATE design_jobs
                SET status = 'failed',
                    error = COALESCE(error, $2),
                    completed_at = now()
                WHERE id = $1 AND owner_user_id = $3
                "#,
            )
            .bind(job_id)
            .bind(error.to_string())
            .bind(viewer.id)
            .execute(&state.db)
            .await;
        }
    });
}

async fn run_design_job(state: AppState, viewer: Viewer, job_id: Uuid) -> Result<(), AppError> {
    let Some(job) = load_design_job_record(&state.db, viewer.id, job_id).await? else {
        return Ok(());
    };
    if job.status == "completed" {
        return Ok(());
    }

    sqlx::query(
        r#"
        UPDATE design_jobs
        SET status = 'running',
            started_at = COALESCE(started_at, now()),
            error = NULL
        WHERE id = $1 AND owner_user_id = $2
        "#,
    )
    .bind(job_id)
    .bind(viewer.id)
    .execute(&state.db)
    .await?;
    touch_design_session(&state.db, viewer.id, job.session_id).await?;

    let iterates_on = match job.iterates_on_id {
        Some(run_id) => get_owned_run(&state, viewer.id, run_id).await.ok(),
        None => None,
    };
    let (prompt, source_ids, asset_ids) = build_generation_prompt(
        &job.prompt,
        &job.reference_snapshot_json,
        iterates_on.as_ref(),
    );
    let generation_input = StormGenerationInput {
        prompt,
        draft_mode: None,
        source_ids,
        asset_ids,
        session_id: Some(job.session_id),
        iterates_on_id: job.iterates_on_id,
        fallback_title: Some(job.title.clone()),
    };

    match generate_storm_internal(&state, &viewer, generation_input).await {
        Ok(response) => {
            sqlx::query(
                r#"
                UPDATE design_jobs
                SET status = 'completed',
                    result_run_id = $2,
                    error = NULL,
                    completed_at = now()
                WHERE id = $1 AND owner_user_id = $3
                "#,
            )
            .bind(job_id)
            .bind(response.run.id)
            .bind(viewer.id)
            .execute(&state.db)
            .await?;
            touch_design_session(&state.db, viewer.id, job.session_id).await?;
            Ok(())
        }
        Err(error) => {
            sqlx::query(
                r#"
                UPDATE design_jobs
                SET status = 'failed',
                    error = $2,
                    completed_at = now()
                WHERE id = $1 AND owner_user_id = $3
                "#,
            )
            .bind(job_id)
            .bind(error.to_string())
            .bind(viewer.id)
            .execute(&state.db)
            .await?;
            touch_design_session(&state.db, viewer.id, job.session_id).await?;
            Ok(())
        }
    }
}

async fn handle_session_message(
    state: &AppState,
    viewer: &Viewer,
    session_id: Uuid,
    payload: SessionMessageRequest,
    images: Vec<Vec<u8>>,
) -> Result<String, AppError> {
    let session = get_owned_design_session(&state.db, viewer.id, session_id).await?;
    let has_provider = load_provider_for_user(state, viewer.id).await?.is_some();
    if !has_provider {
        return Err(AppError::BadRequest(
            "Connect Codex in settings before chatting with the design agent.".to_string(),
        ));
    }
    let body = payload.body.trim().to_string();
    if body.is_empty() && images.is_empty() {
        return Err(AppError::BadRequest(
            "Message body is required.".to_string(),
        ));
    }
    let body = if body.is_empty() {
        format!("[{} image{} attached]", images.len(), if images.len() == 1 { "" } else { "s" })
    } else {
        body
    };

    let selected_reference_handles = normalize_reference_handles(payload.reference_ids);
    let default_iterates_on_id =
        parse_optional_uuid(payload.iterates_on_id.as_deref(), "iterates_on_id")?;
    let iterates_on = match default_iterates_on_id {
        Some(run_id) => Some(get_owned_run(state, viewer.id, run_id).await?),
        None => None,
    };
    let reference_snapshot =
        resolve_reference_snapshot(state, viewer.id, &selected_reference_handles).await?;

    insert_session_message(&state.db, session_id, "user", &body, None).await?;
    if session.title == "Untitled Session" {
        update_design_session_title(
            &state.db,
            viewer.id,
            session_id,
            &derive_design_title(&body),
        )
        .await?;
    } else {
        touch_design_session(&state.db, viewer.id, session_id).await?;
    }

    let messages = load_session_messages(&state.db, viewer.id, session_id).await?;
    let (turn, queued_call) = run_session_agent(
        state,
        viewer,
        session_id,
        &messages,
        selected_reference_handles.clone(),
        &reference_snapshot,
        iterates_on.as_ref(),
        images,
    )
    .await?;

    let assistant_body = turn.assistant_output.safe_text.trim().to_string();
    let queued_job = if let Some(call) = queued_call {
        let resolved_snapshot =
            resolve_reference_snapshot(state, viewer.id, &call.reference_handles).await?;
        let job =
            create_design_job_record(&state.db, viewer.id, session_id, &call, resolved_snapshot)
                .await?;
        spawn_design_job_runner(state.clone(), viewer.clone(), job.id);
        Some(job)
    } else {
        None
    };

    let assistant_message = if assistant_body.is_empty() {
        if let Some(job) = queued_job.as_ref() {
            format!("Queued \"{}\".", job.title)
        } else {
            "Captured. Tell me when you want me to make designs from it.".to_string()
        }
    } else {
        assistant_body
    };
    insert_session_message(
        &state.db,
        session_id,
        "assistant",
        &assistant_message,
        queued_job.as_ref().map(|job| job.id),
    )
    .await?;
    touch_design_session(&state.db, viewer.id, session_id).await?;

    Ok(queued_job
        .as_ref()
        .map(|job| format!("Queued {}.", job.title))
        .unwrap_or_else(|| truncate_for_log(&assistant_message, 120)))
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
            "Design a distinctive design-language document for this seed:\n\n{prompt}\n\nRequirements:\n- produce a full static HTML artifact in the workspace\n- use index.html and styles.css as the primary files\n- if web research helps, use search_web/fetch_url selectively\n- iterate yourself instead of delegating to other agents\n- render and inspect the artifact before finishing\n- an inputs/ directory may contain reference assets (images, fonts) — use copy_input to include any you want in the output\n- call submit_result(title=..., summary=...) once the result is coherent"
        ),
    }
}

async fn build_tool_provider(
    role: StormAgentRole,
    workspace: Arc<Mutex<WorkspaceRuntimeState>>,
    runtime: StormRuntimeCtx,
    screenshotter: ScreenshotService,
    port: u16,
) -> Arc<dyn ToolProvider> {
    let custom = StormToolProvider::new(role, workspace, screenshotter, port);
    let mut tools = ToolSet::new() + custom;
    if let Some(key) = runtime.tavily_api_key.as_ref() {
        tools = tools + WebSearch::new(key.clone()) + FetchUrl::new(key.clone());
    }
    Arc::new(tools)
}

async fn run_design_agent(
    role: StormAgentRole,
    workspace: Arc<Mutex<WorkspaceRuntimeState>>,
    runtime: StormRuntimeCtx,
    prompt: String,
    screenshotter: ScreenshotService,
    port: u16,
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
    let tools = build_tool_provider(role, workspace, runtime.clone(), screenshotter, port).await;
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
    let mut engine = LashRuntime::from_state(config, tools, AgentStateEnvelope::default())
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

fn artifact_archive_key(user_id: Uuid, run_id: Uuid) -> String {
    format!("storm-runs/{user_id}/{run_id}.zip")
}

fn sanitize_download_name(title: &str, run_id: Uuid) -> String {
    let mut base = title
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while base.contains("--") {
        base = base.replace("--", "-");
    }
    let base = base.trim_matches('-');
    if base.is_empty() {
        format!("designstorm-{run_id}.zip")
    } else {
        format!("{base}-{run_id}.zip")
    }
}

async fn persist_workspace_archive(
    state: &AppState,
    user_id: Uuid,
    run_id: Uuid,
    workspace_dir: &StdPath,
) -> Result<(), AppError> {
    let Some(storage) = &state.artifact_storage else {
        return Ok(());
    };

    let workspace_dir = workspace_dir.to_path_buf();
    let archive_bytes = tokio::task::spawn_blocking(move || zip_workspace_dir(&workspace_dir))
        .await
        .map_err(|error| AppError::Internal(error.to_string()))??;

    storage
        .client
        .put_object()
        .bucket(&storage.bucket)
        .key(artifact_archive_key(user_id, run_id))
        .content_type("application/zip")
        .body(ByteStream::from(archive_bytes))
        .send()
        .await
        .map_err(|error| {
            AppError::Internal(format!("Failed to upload artifact archive: {error}"))
        })?;

    Ok(())
}

async fn load_persisted_workspace_entry(
    state: &AppState,
    user_id: Uuid,
    run_id: Uuid,
    logical_path: &str,
) -> Result<Option<Vec<u8>>, AppError> {
    let Some(bytes) = load_persisted_workspace_archive(state, user_id, run_id).await? else {
        return Ok(None);
    };

    tokio::task::spawn_blocking({
        let logical_path = logical_path.to_string();
        move || extract_zip_entry(&bytes, &logical_path)
    })
    .await
    .map_err(|error| AppError::Internal(error.to_string()))?
}

async fn load_persisted_workspace_archive(
    state: &AppState,
    user_id: Uuid,
    run_id: Uuid,
) -> Result<Option<Vec<u8>>, AppError> {
    let Some(storage) = &state.artifact_storage else {
        return Ok(None);
    };

    let response = match storage
        .client
        .get_object()
        .bucket(&storage.bucket)
        .key(artifact_archive_key(user_id, run_id))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            warn!(user_id = %user_id, run_id = %run_id, error = %error, "failed to fetch persisted artifact archive");
            return Ok(None);
        }
    };

    let bytes = response
        .body
        .collect()
        .await
        .map_err(|error| {
            AppError::Internal(format!(
                "Failed to read persisted artifact archive: {error}"
            ))
        })?
        .into_bytes()
        .to_vec();
    Ok(Some(bytes))
}

fn zip_workspace_dir(workspace_dir: &StdPath) -> Result<Vec<u8>, AppError> {
    let cursor = Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);

    for entry in WalkDir::new(workspace_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let path = entry.path();
        let relative = path
            .strip_prefix(workspace_dir)
            .map_err(|error| AppError::Internal(error.to_string()))?;
        let name = relative.to_string_lossy().replace('\\', "/");
        if name.starts_with("inputs/") {
            continue;
        }
        writer
            .start_file(name, options)
            .map_err(|error| AppError::Internal(error.to_string()))?;
        let bytes = std::fs::read(path)?;
        writer.write_all(&bytes)?;
    }

    let cursor = writer
        .finish()
        .map_err(|error| AppError::Internal(error.to_string()))?;
    Ok(cursor.into_inner())
}

fn extract_zip_entry(
    archive_bytes: &[u8],
    logical_path: &str,
) -> Result<Option<Vec<u8>>, AppError> {
    let cursor = Cursor::new(archive_bytes.to_vec());
    let mut archive =
        ZipArchive::new(cursor).map_err(|error| AppError::Internal(error.to_string()))?;
    let mut file = match archive.by_name(logical_path) {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(AppError::Internal(error.to_string())),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

fn render_missing_preview_html(run: &StormRunRecord) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"UTF-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\"><title>Preview unavailable</title><style>body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#0f0910;color:#d8e4ee;font-family:'IBM Plex Sans',system-ui,sans-serif}}main{{max-width:32rem;padding:32px;border:1px solid rgba(91,156,184,.2);border-radius:20px;background:rgba(16,17,24,.82);box-shadow:0 24px 80px rgba(0,0,0,.45)}}h1{{margin:0 0 12px;font-size:1.3rem}}p{{margin:0 0 10px;line-height:1.6;color:rgba(216,228,238,.8)}}code{{font-family:'IBM Plex Mono',monospace;font-size:.8rem;color:#8fc7e5}}</style></head><body><main><h1>Preview unavailable</h1><p>This artifact was created on a workspace that is no longer present on this instance.</p><p>The run metadata still exists, but the generated files for <code>{}</code> could not be found.</p><p>Generate it again to restore a live preview.</p></main></body></html>",
        run.id
    )
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
