use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use askama::Template;
use async_stream::stream;
use aws_config::BehaviorVersion;
use aws_sdk_s3::{Client as S3Client, config::Region, primitives::ByteStream};
use axum::{
    Router,
    body::Body,
    extract::{Form, Json, Multipart, Path, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{
            CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, SET_COOKIE,
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
    io::{Cursor, Read, Write},
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
use walkdir::WalkDir;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::FileOptions};

const SESSION_COOKIE_NAME: &str = "designstorm_session";
const DATASTAR_CDN: &str =
    "https://cdn.jsdelivr.net/gh/starfederation/datastar@1.0.0-RC.8/bundles/datastar.js";

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    db: PgPool,
    http: Client,
    artifact_storage: Option<ArtifactStorage>,
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

async fn build_artifact_storage(
    config: &Config,
) -> Result<Option<ArtifactStorage>, AppError> {
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
    position_x: Option<f64>,
    position_y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
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
            position_x: row.position_x,
            position_y: row.position_y,
            width: row.width,
            height: row.height,
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

        if !tokio::fs::try_exists(&source_resolved).await.unwrap_or(false) {
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

// ─── Presets ───

struct Preset {
    id: &'static str,
    label: &'static str,
    icon: &'static str,
    items: &'static [&'static str],
}

static PRESETS: &[Preset] = &[
    Preset { id: "oblique_strategy", label: "Oblique Strategy", icon: "🃏", items: &[
        "Use an old idea", "Honor thy error as a hidden intention",
        "What would your closest friend do?", "Emphasize differences",
        "Remove specifics and convert to ambiguities", "Don't be frightened of clichés",
        "What is the reality of the situation?", "Are there sections? Consider transitions",
        "Turn it upside down", "Think of the radio", "Allow an easement",
        "Bridges —build —burn", "Change instrument roles", "Cluster analysis",
        "Consider different fading systems", "Courage!", "Cut a vital connection",
        "Decorate, decorate", "Discover the recipes you are using and abandon them",
        "Disconnect from desire", "Do nothing for as long as possible",
        "Do something boring", "Do the washing up", "Don't be afraid of things because they're easy to do",
        "Don't be frightened to display your talents", "Faced with a choice, do both",
        "Go slowly all the way round the outside", "How would you have done it?",
        "Humanize something free of error", "Infinitesimal gradations",
        "Into the impossible", "Is it finished?", "Just carry on",
        "Look at the order in which you do things", "Lowest common denominator",
        "Make a sudden, destructive unpredictable action; incorporate",
        "Mute and continue", "Only one element of each kind",
        "Remove ambiguities and convert to specifics", "Repetition is a form of change",
        "Reverse", "Short circuit", "Shut the door and listen from outside",
        "Simple subtraction", "Spectrum analysis", "Take a break",
        "Take away the elements in order of apparent non-importance",
        "The inconsistency principle", "The tape is now the music",
        "Think —inside— the work", "Tidy up", "Trust in the you of now",
        "Try faking it!", "Use 'unqualified' people", "Use fewer notes",
        "Water", "What mistakes did you make last time?",
        "What wouldn't you do?", "Work at a different speed",
        "You are an engineer", "You can only make one dot at a time",
        "Your mistake was a hidden intention",
    ]},
    Preset { id: "art_movement", label: "Art Movement", icon: "🏛", items: &[
        "Brutalism", "Art Deco", "Bauhaus", "Memphis", "De Stijl", "Art Nouveau",
        "Swiss International", "Constructivism", "Psychedelic", "Futurism",
        "Minimalism", "Maximalism", "Ukiyo-e", "Op Art", "Pop Art",
        "Dadaism", "Surrealism", "Expressionism", "Cubism", "Gothic",
        "Victorian", "Mid-Century Modern", "Streamline Moderne",
        "Vaporwave", "Y2K", "Neoclassical", "Rococo", "Arts & Crafts",
        "Suprematism", "Abstract Expressionism", "Post-Punk", "Acid House",
        "Cyberpunk", "Solarpunk", "Afrofuturism", "Wabi-Sabi",
    ]},
    Preset { id: "material", label: "Material", icon: "🧱", items: &[
        "Brushed copper", "Wet concrete", "Cracked porcelain", "Woven linen",
        "Oxidized steel", "Raw marble", "Frosted glass", "Charred wood",
        "Hammered brass", "Liquid mercury", "Dried clay", "Polished obsidian",
        "Torn paper", "Rusted iron", "Silk velvet", "Rough sandstone",
        "Molten gold", "Patinated bronze", "Cork", "Volcanic basalt",
        "Bleached bone", "Smoked oak", "Anodized aluminum", "Raw denim",
        "Sea glass", "Terracotta", "Resin", "Pressed tin",
        "Weathered leather", "Ice", "Bamboo", "Carbon fiber",
    ]},
    Preset { id: "mood", label: "Mood", icon: "◑", items: &[
        "Caustic", "Devotional", "Glacial", "Feral", "Saccharine",
        "Liminal", "Euphoric", "Melancholic", "Abrasive", "Ethereal",
        "Monastic", "Volcanic", "Tender", "Corrosive", "Luminous",
        "Desolate", "Riotous", "Hypnotic", "Fragile", "Tectonic",
        "Nocturnal", "Ceremonial", "Volatile", "Pristine", "Ancient",
        "Electric", "Submerged", "Feverish", "Meditative", "Carnivalesque",
        "Austere", "Radiant",
    ]},
    Preset { id: "era", label: "Era", icon: "⏳", items: &[
        "1920s Jazz Age", "1930s Propaganda poster", "1950s Atomic Age",
        "1960s Counterculture", "1970s Disco", "1972 Swiss typography",
        "1980s MTV", "1984 Macintosh", "1990s Grunge", "1995 Early web",
        "1998 Flash era", "2000s Web 2.0", "2004 MySpace", "2007 iPhone launch",
        "2010s Flat design", "2013 Skeuomorphism's death", "2016 Brutalist web",
        "2020s Glassmorphism", "2025 AI-native", "Near future 2035",
        "Retro-future 1960s vision of 2000", "Cassette futurism",
    ]},
    Preset { id: "type_pairing", label: "Type Pairing", icon: "Aa", items: &[
        "Playfair Display + Source Sans Pro — editorial elegance",
        "Space Grotesk + Inter — technical clarity",
        "Cormorant Garamond + Manrope — warm contrast",
        "DM Serif Display + DM Sans — modern authority",
        "Libre Baskerville + Karla — bookish digital",
        "Syne + Work Sans — geometric energy",
        "Fraunces + Commissioner — quirky editorial",
        "Instrument Serif + Instrument Sans — matched pair",
        "Anybody + IBM Plex Mono — variable futurism",
        "Bricolage Grotesque + Geist — startup clean",
        "Crimson Pro + Lato — longform readability",
        "Archivo Black + Archivo — bold system",
        "EB Garamond + Fira Sans — classical meets code",
        "Unbounded + Noto Sans — expressive + neutral",
    ]},
];

fn find_preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

fn random_preset_value(preset: &Preset) -> &'static str {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let idx = rng.gen_range(0..preset.items.len());
    preset.items[idx]
}

// ─── Board nodes & edges ───

#[derive(Debug, Clone, FromRow)]
struct BoardNodeRow {
    id: Uuid,
    #[allow(dead_code)]
    owner_user_id: Uuid,
    node_type: String,
    position_x: f64,
    position_y: f64,
    content: serde_json::Value,
    locked: bool,
    #[allow(dead_code)]
    created_at: DateTime<Utc>,
    width: Option<f64>,
    height: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BoardNodeSummary {
    id: Uuid,
    node_type: String,
    position_x: f64,
    position_y: f64,
    content: serde_json::Value,
    locked: bool,
    width: Option<f64>,
    height: Option<f64>,
}

impl BoardNodeSummary {
    fn content_json(&self) -> String {
        self.content.to_string()
    }

    fn entropy_title(&self) -> &str {
        self.content
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Random page")
    }

    fn entropy_url(&self) -> &str {
        self.content
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    fn input_text(&self) -> &str {
        self.content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    fn input_images(&self) -> Vec<&str> {
        self.content
            .get("images")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default()
    }

    fn palette_colors(&self) -> Vec<&str> {
        self.content
            .get("colors")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default()
    }

    fn pick_k_value(&self) -> i64 {
        self.content.get("k").and_then(|v| v.as_i64()).unwrap_or(1)
    }

    fn pick_k_replace(&self) -> bool {
        self.content.get("replace").and_then(|v| v.as_bool()).unwrap_or(false)
    }

    fn set_title(&self) -> &str {
        self.content.get("title").and_then(|v| v.as_str()).unwrap_or("Set")
    }

    fn set_description(&self) -> &str {
        self.content.get("description").and_then(|v| v.as_str()).unwrap_or("")
    }

    fn image_url(&self) -> &str {
        self.content.get("url").and_then(|v| v.as_str()).unwrap_or("")
    }

    fn image_alt(&self) -> &str {
        self.content.get("alt").and_then(|v| v.as_str()).unwrap_or("")
    }

    fn int_value(&self) -> i64 {
        self.content.get("value").and_then(|v| v.as_i64()).unwrap_or(0)
    }

    fn float_value(&self) -> f64 {
        self.content.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0)
    }

    fn string_value(&self) -> &str {
        self.content.get("value").and_then(|v| v.as_str()).unwrap_or("")
    }

    fn bool_value(&self) -> bool {
        self.content.get("value").and_then(|v| v.as_bool()).unwrap_or(false)
    }

    fn int_min(&self) -> i64 {
        self.content.get("min").and_then(|v| v.as_i64()).unwrap_or(0)
    }

    fn int_max(&self) -> i64 {
        self.content.get("max").and_then(|v| v.as_i64()).unwrap_or(100)
    }

    fn float_min(&self) -> f64 {
        self.content.get("min").and_then(|v| v.as_f64()).unwrap_or(0.0)
    }

    fn float_max(&self) -> f64 {
        self.content.get("max").and_then(|v| v.as_f64()).unwrap_or(1.0)
    }

    fn float_step(&self) -> f64 {
        self.content.get("step").and_then(|v| v.as_f64()).unwrap_or(0.01)
    }

    fn random_enabled(&self) -> bool {
        self.content.get("random").and_then(|v| v.as_bool()).unwrap_or(false)
    }

    fn font_family(&self) -> &str {
        self.content.get("family").and_then(|v| v.as_str()).unwrap_or("")
    }

    fn font_weight(&self) -> &str {
        self.content.get("weight").and_then(|v| v.as_str()).unwrap_or("400")
    }

    fn font_size(&self) -> &str {
        self.content.get("size").and_then(|v| v.as_str()).unwrap_or("16")
    }

    fn font_line_height(&self) -> &str {
        self.content.get("lineHeight").and_then(|v| v.as_str()).unwrap_or("1.5")
    }

    fn font_file_name(&self) -> &str {
        self.content.get("file_name").and_then(|v| v.as_str()).unwrap_or("")
    }

    fn has_preset(&self) -> bool {
        self.content.get("preset").and_then(|v| v.as_str()).is_some()
    }

    fn preset_id(&self) -> &str {
        self.content.get("preset").and_then(|v| v.as_str()).unwrap_or("")
    }

    fn preset_label(&self) -> &str {
        let id = self.preset_id();
        find_preset(id).map(|p| p.label).unwrap_or("Preset")
    }
}

impl From<BoardNodeRow> for BoardNodeSummary {
    fn from(row: BoardNodeRow) -> Self {
        Self {
            id: row.id,
            node_type: row.node_type,
            position_x: row.position_x,
            position_y: row.position_y,
            content: row.content,
            locked: row.locked,
            width: row.width,
            height: row.height,
        }
    }
}

#[derive(Debug, Clone, FromRow)]
struct BoardEdgeRow {
    id: Uuid,
    #[allow(dead_code)]
    owner_user_id: Uuid,
    source_id: Uuid,
    source_type: String,
    target_id: Uuid,
    target_type: String,
    source_anchor_side: Option<String>,
    source_anchor_t: Option<f64>,
    target_anchor_side: Option<String>,
    target_anchor_t: Option<f64>,
    source_slot: String,
    target_slot: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BoardEdgeSummary {
    id: Uuid,
    source_id: Uuid,
    source_type: String,
    target_id: Uuid,
    target_type: String,
    source_anchor_side: Option<String>,
    source_anchor_t: Option<f64>,
    target_anchor_side: Option<String>,
    target_anchor_t: Option<f64>,
    source_slot: String,
    target_slot: String,
}

impl From<BoardEdgeRow> for BoardEdgeSummary {
    fn from(row: BoardEdgeRow) -> Self {
        Self {
            id: row.id,
            source_id: row.source_id,
            source_type: row.source_type,
            target_id: row.target_id,
            target_type: row.target_type,
            source_anchor_side: row.source_anchor_side,
            source_anchor_t: row.source_anchor_t,
            target_anchor_side: row.target_anchor_side,
            target_anchor_t: row.target_anchor_t,
            source_slot: row.source_slot,
            target_slot: row.target_slot,
        }
    }
}

#[derive(Debug, Deserialize)]
struct CreateNodeRequest {
    node_type: String,
    position_x: f64,
    position_y: f64,
}

#[derive(Debug, Deserialize)]
struct DuplicateNodeRequest {
    source_id: Uuid,
    position_x: Option<f64>,
    position_y: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct CreateNodeFormRequest {
    node_type: String,
    position_x: f64,
    position_y: f64,
    source_id: Option<Uuid>,
    source_type: Option<String>,
    source_anchor_side: Option<String>,
    source_anchor_t: Option<f64>,
    preset: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdatePositionRequest {
    position_x: f64,
    position_y: f64,
    width: Option<f64>,
    height: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct CreateEdgeRequest {
    source_id: Uuid,
    source_type: String,
    target_id: Uuid,
    target_type: String,
    source_anchor_side: Option<String>,
    source_anchor_t: Option<f64>,
    target_anchor_side: Option<String>,
    target_anchor_t: Option<f64>,
    #[serde(default = "default_out_slot")]
    source_slot: String,
    #[serde(default = "default_sources_slot")]
    target_slot: String,
}

fn default_out_slot() -> String { "out".to_string() }
fn default_sources_slot() -> String { "sources".to_string() }

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
    board_nodes: &'a [BoardNodeSummary],
    edges_json: &'a str,
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
    asset_ids: Vec<(Uuid, String)>,
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

    let artifact_storage = build_artifact_storage(&config).await?;

    let state = AppState {
        db,
        http: Client::new(),
        artifact_storage,
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
        .route("/storms/{id}", axum::routing::delete(delete_storm_run))
        .route("/storms/{id}/download", get(download_storm_run_archive))
        .route("/storms/{id}/position", post(update_storm_run_position))
        .route("/telemetry/client", post(client_telemetry))
        .route("/api/storms", get(list_storms).post(create_storm))
        .route("/preview/{run_id}", get(preview_index_redirect))
        .route("/preview/{run_id}/", get(preview_index))
        .route("/preview/{run_id}/{*path}", get(preview_asset))
        .route("/presets", get(list_presets))
        .route("/nodes/create", post(create_board_node_datastar))
        .route("/nodes", post(create_board_node))
        .route("/nodes/duplicate", post(duplicate_board_node))
        .route("/nodes/{id}/reroll", post(reroll_board_node))
        .route("/nodes/{id}/lock", post(toggle_board_node_lock))
        .route("/nodes/{id}/position", post(update_board_node_position))
        .route("/nodes/{id}/content", post(update_board_node_content))
        .route("/nodes/{id}/run", post(run_generate_node))
        .route("/nodes/{id}", axum::routing::delete(delete_board_node))
        .route("/assets", post(upload_asset))
        .route("/assets/{id}/{file_name}", get(serve_asset))
        .route("/edges", post(create_board_edge))
        .route("/edges/{id}", axum::routing::delete(delete_board_edge))
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
                match render_storm_board_html(&state, viewer.id).await {
                    Ok(board_html) => {
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
                    Err(error) => {
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

async fn run_generate_node(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let viewer = require_viewer(&state, &headers).await?;

    // Load the generate node and verify ownership/type
    let node = sqlx::query_as::<_, BoardNodeRow>(
        "SELECT id, owner_user_id, node_type, position_x, position_y, content, locked, created_at, width, height FROM board_nodes WHERE id = $1 AND owner_user_id = $2",
    )
    .bind(node_id)
    .bind(viewer.id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::BadRequest("Generate node not found.".to_string()))?;

    if node.node_type != "generate" {
        return Err(AppError::BadRequest("Node is not a generate node.".to_string()));
    }

    // Collect all inputs wired into this generate node
    let edges = sqlx::query_as::<_, BoardEdgeRow>(
        r#"SELECT id, owner_user_id, source_id, source_type, target_id, target_type,
                  source_anchor_side, source_anchor_t, target_anchor_side, target_anchor_t,
                  source_slot, target_slot
           FROM board_edges WHERE target_id = $1 AND owner_user_id = $2"#,
    )
    .bind(node_id)
    .bind(viewer.id)
    .fetch_all(&state.db)
    .await?;

    // Build prompt from wired inputs
    let mut entropy_parts = Vec::new();
    let mut user_parts = Vec::new();
    let mut design_parts = Vec::new();
    let mut asset_ids: Vec<(Uuid, String)> = Vec::new();

    let board_node_sql = "SELECT id, owner_user_id, node_type, position_x, position_y, content, locked, created_at, width, height FROM board_nodes WHERE id = $1 AND owner_user_id = $2";

    for edge in &edges {
        match edge.source_type.as_str() {
            "entropy" => {
                if let Ok(Some(src)) = sqlx::query_as::<_, BoardNodeRow>(board_node_sql)
                    .bind(edge.source_id).bind(viewer.id).fetch_optional(&state.db).await {
                    let title = src.content.get("title").and_then(|v| v.as_str()).unwrap_or("Random page");
                    let url = src.content.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    entropy_parts.push(format!("Reference: \"{}\" — {}", title, url));
                }
            }
            "user_input" => {
                if let Ok(Some(src)) = sqlx::query_as::<_, BoardNodeRow>(board_node_sql)
                    .bind(edge.source_id).bind(viewer.id).fetch_optional(&state.db).await {
                    let text = src.content.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if !text.is_empty() {
                        user_parts.push(format!("User direction: \"{}\"", text));
                    }
                }
            }
            "design" => {
                if let Ok(Some(run)) = sqlx::query_as::<_, StormRunRow>(
                    "SELECT id, owner_user_id, prompt, title, summary, assistant_summary, preview_url, submitted, created_at, workspace_dir, parent_ids, position_x, position_y, width, height FROM storm_runs WHERE id = $1 AND owner_user_id = $2",
                )
                .bind(edge.source_id)
                .bind(viewer.id)
                .fetch_optional(&state.db)
                .await
                {
                    let record = StormRunRecord::from(run);
                    design_parts.push(format!(
                        "Build on: \"{}\" — {}\nOriginal seed: {}",
                        record.title, record.summary, record.prompt
                    ));
                }
            }
            "color_palette" => {
                if let Ok(Some(src)) = sqlx::query_as::<_, BoardNodeRow>(board_node_sql)
                    .bind(edge.source_id).bind(viewer.id).fetch_optional(&state.db).await {
                    let colors: Vec<&str> = src.content.get("colors")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                        .unwrap_or_default();
                    if !colors.is_empty() {
                        user_parts.push(format!("Color palette constraint: use these colors: {}", colors.join(", ")));
                    }
                }
            }
            "image" => {
                if let Ok(Some(src)) = sqlx::query_as::<_, BoardNodeRow>(board_node_sql)
                    .bind(edge.source_id).bind(viewer.id).fetch_optional(&state.db).await {
                    let alt = src.content.get("alt").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(aid) = src.content.get("asset_id").and_then(|v| v.as_str()) {
                        if let Ok(asset_uuid) = Uuid::from_str(aid) {
                            let desc = if alt.is_empty() { "image".to_string() } else { alt.to_string() };
                            asset_ids.push((asset_uuid, desc.clone()));
                            user_parts.push(format!("Reference image available at: inputs/{} — use copy_input to include it in your output if needed", desc));
                        }
                    } else {
                        let url = src.content.get("url").and_then(|v| v.as_str()).unwrap_or("");
                        if !url.is_empty() {
                            user_parts.push(format!("Reference image: {} ({})", url, alt));
                        }
                    }
                }
            }
            "font" => {
                if let Ok(Some(src)) = sqlx::query_as::<_, BoardNodeRow>(board_node_sql)
                    .bind(edge.source_id).bind(viewer.id).fetch_optional(&state.db).await {
                    let family = src.content.get("family").and_then(|v| v.as_str()).unwrap_or("");
                    let weight = src.content.get("weight").and_then(|v| v.as_str()).unwrap_or("400");
                    let size = src.content.get("size").and_then(|v| v.as_str()).unwrap_or("16");
                    let line_height = src.content.get("lineHeight").and_then(|v| v.as_str()).unwrap_or("1.5");
                    let mut parts = Vec::new();
                    if !family.is_empty() { parts.push(format!("font-family: {}", family)); }
                    parts.push(format!("font-weight: {}", weight));
                    parts.push(format!("font-size: {}px", size));
                    parts.push(format!("line-height: {}", line_height));
                    user_parts.push(format!("Typography constraint: {}", parts.join(", ")));
                    if let Some(aid) = src.content.get("asset_id").and_then(|v| v.as_str()) {
                        if let Ok(asset_uuid) = Uuid::from_str(aid) {
                            let file_name = src.content.get("file_name").and_then(|v| v.as_str()).unwrap_or("font.woff2");
                            asset_ids.push((asset_uuid, file_name.to_string()));
                            user_parts.push(format!("Custom font file available at: inputs/{} — use copy_input to copy it into your workspace, then add a @font-face declaration pointing to it", file_name));
                        }
                    }
                }
            }
            "set" => {
                // Expand set: resolve all members recursively
                let member_edges = sqlx::query_as::<_, BoardEdgeRow>(
                    r#"SELECT id, owner_user_id, source_id, source_type, target_id, target_type,
                              source_anchor_side, source_anchor_t, target_anchor_side, target_anchor_t,
                              source_slot, target_slot
                       FROM board_edges WHERE target_id = $1 AND owner_user_id = $2 AND target_slot = 'members'"#,
                )
                .bind(edge.source_id)
                .bind(viewer.id)
                .fetch_all(&state.db)
                .await
                .unwrap_or_default();
                for member_edge in &member_edges {
                    if let Ok(Some(src)) = sqlx::query_as::<_, BoardNodeRow>(board_node_sql)
                            .bind(member_edge.source_id).bind(viewer.id).fetch_optional(&state.db).await {
                        match src.node_type.as_str() {
                            "entropy" => {
                                let title = src.content.get("title").and_then(|v| v.as_str()).unwrap_or("Random page");
                                let url = src.content.get("url").and_then(|v| v.as_str()).unwrap_or("");
                                entropy_parts.push(format!("Reference: \"{}\" — {}", title, url));
                            }
                            "user_input" => {
                                let text = src.content.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                if !text.is_empty() {
                                    user_parts.push(format!("User direction: \"{}\"", text));
                                }
                            }
                            "color_palette" => {
                                let colors: Vec<&str> = src.content.get("colors")
                                    .and_then(|v| v.as_array())
                                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                                    .unwrap_or_default();
                                if !colors.is_empty() {
                                    user_parts.push(format!("Color palette constraint: use these colors: {}", colors.join(", ")));
                                }
                            }
                            "image" => {
                                let alt = src.content.get("alt").and_then(|v| v.as_str()).unwrap_or("");
                                if let Some(aid) = src.content.get("asset_id").and_then(|v| v.as_str()) {
                                    if let Ok(asset_uuid) = Uuid::from_str(aid) {
                                        let desc = if alt.is_empty() { "image".to_string() } else { alt.to_string() };
                                        asset_ids.push((asset_uuid, desc.clone()));
                                        user_parts.push(format!("Reference image available at: inputs/{} — use copy_input to include it in your output if needed", desc));
                                    }
                                } else {
                                    let url = src.content.get("url").and_then(|v| v.as_str()).unwrap_or("");
                                    if !url.is_empty() {
                                        user_parts.push(format!("Reference image: {} ({})", url, alt));
                                    }
                                }
                            }
                            "font" => {
                                let family = src.content.get("family").and_then(|v| v.as_str()).unwrap_or("");
                                let weight = src.content.get("weight").and_then(|v| v.as_str()).unwrap_or("400");
                                let size = src.content.get("size").and_then(|v| v.as_str()).unwrap_or("16");
                                let line_height = src.content.get("lineHeight").and_then(|v| v.as_str()).unwrap_or("1.5");
                                let mut parts = Vec::new();
                                if !family.is_empty() { parts.push(format!("font-family: {}", family)); }
                                parts.push(format!("font-weight: {}", weight));
                                parts.push(format!("font-size: {}px", size));
                                parts.push(format!("line-height: {}", line_height));
                                user_parts.push(format!("Typography constraint: {}", parts.join(", ")));
                                if let Some(aid) = src.content.get("asset_id").and_then(|v| v.as_str()) {
                                    if let Ok(asset_uuid) = Uuid::from_str(aid) {
                                        let file_name = src.content.get("file_name").and_then(|v| v.as_str()).unwrap_or("font.woff2");
                                        asset_ids.push((asset_uuid, file_name.to_string()));
                                        user_parts.push(format!("Custom font file available at: inputs/{} — use copy_input to copy it into your workspace, then add a @font-face declaration pointing to it", file_name));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            "pick_k" => {
                // Expand pick_k: resolve members, pick up to K (with or without replacement)
                if let Ok(Some(pk_node)) = sqlx::query_as::<_, BoardNodeRow>(board_node_sql)
                    .bind(edge.source_id).bind(viewer.id).fetch_optional(&state.db).await {
                    let k = pk_node.content.get("k").and_then(|v| v.as_i64()).unwrap_or(1) as usize;
                    let with_replacement = pk_node.content.get("replace").and_then(|v| v.as_bool()).unwrap_or(false);
                    let member_edges = sqlx::query_as::<_, BoardEdgeRow>(
                        r#"SELECT id, owner_user_id, source_id, source_type, target_id, target_type,
                                  source_anchor_side, source_anchor_t, target_anchor_side, target_anchor_t,
                                  source_slot, target_slot
                           FROM board_edges WHERE target_id = $1 AND owner_user_id = $2 AND target_slot = 'sources'"#,
                    )
                    .bind(edge.source_id)
                    .bind(viewer.id)
                    .fetch_all(&state.db)
                    .await
                    .unwrap_or_default();

                    // Resolve all source nodes once
                    let mut resolved_sources = Vec::new();
                    for member_edge in &member_edges {
                        if let Ok(Some(src)) = sqlx::query_as::<_, BoardNodeRow>(board_node_sql)
                            .bind(member_edge.source_id).bind(viewer.id).fetch_optional(&state.db).await {
                            resolved_sources.push(src);
                        }
                    }

                    if with_replacement && !resolved_sources.is_empty() {
                        // With replacement: cycle through sources repeatedly up to K
                        let mut count = 0usize;
                        let mut idx = 0usize;
                        while count < k {
                            let src = &resolved_sources[idx % resolved_sources.len()];
                            match src.node_type.as_str() {
                                "entropy" => {
                                    let title = src.content.get("title").and_then(|v| v.as_str()).unwrap_or("Random page");
                                    let url = src.content.get("url").and_then(|v| v.as_str()).unwrap_or("");
                                    entropy_parts.push(format!("Reference: \"{}\" — {}", title, url));
                                    count += 1;
                                }
                                "user_input" => {
                                    let text = src.content.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                    if !text.is_empty() {
                                        user_parts.push(format!("User direction: \"{}\"", text));
                                        count += 1;
                                    }
                                }
                                _ => { count += 1; }
                            }
                            idx += 1;
                            // Safety: if all sources are empty user_inputs, avoid infinite loop
                            if idx >= k * resolved_sources.len() { break; }
                        }
                    } else {
                        // Without replacement: take up to K unique sources
                        let mut count = 0usize;
                        for src in &resolved_sources {
                            if count >= k { break; }
                            match src.node_type.as_str() {
                                "entropy" => {
                                    let title = src.content.get("title").and_then(|v| v.as_str()).unwrap_or("Random page");
                                    let url = src.content.get("url").and_then(|v| v.as_str()).unwrap_or("");
                                    entropy_parts.push(format!("Reference: \"{}\" — {}", title, url));
                                    count += 1;
                                }
                                "user_input" => {
                                    let text = src.content.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                    if !text.is_empty() {
                                        user_parts.push(format!("User direction: \"{}\"", text));
                                        count += 1;
                                    }
                                }
                                _ => { count += 1; }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if entropy_parts.is_empty() && user_parts.is_empty() && design_parts.is_empty() {
        return Ok(datastar_event_stream(Box::pin(stream! {
            yield Ok::<_, Infallible>(patch_signals(json!({
                "_generating": false,
                "_status": "Wire some inputs to the Generate node first.",
            }).to_string()));
        })));
    }

    let mut prompt = String::from("Design Direction:\n\n");
    if !entropy_parts.is_empty() {
        prompt.push_str(&entropy_parts.join("\n"));
        prompt.push('\n');
    }
    if !user_parts.is_empty() {
        if !entropy_parts.is_empty() { prompt.push('\n'); }
        prompt.push_str(&user_parts.join("\n"));
        prompt.push('\n');
    }
    if !design_parts.is_empty() {
        if !entropy_parts.is_empty() || !user_parts.is_empty() { prompt.push('\n'); }
        prompt.push_str(&design_parts.join("\n\n"));
        prompt.push('\n');
    }
    prompt.push_str("\n---\nCreate a bold design language artifact that synthesizes these inputs.");

    let input = StormGenerationInput {
        prompt,
        draft_mode: None,
        source_ids: vec![],
        asset_ids,
    };

    let gen_node_id = node_id;

    Ok(datastar_event_stream(Box::pin(stream! {
        yield Ok::<_, Infallible>(patch_signals(json!({
            "_generating": true,
            "_status": "Generating storm...",
            "_latestRunId": "",
        }).to_string()));

        match generate_storm_internal(&state, &viewer, input).await {
            Ok(response) => {
                let new_run_id = response.run.id;

                // Create board_edge from generate node → new design run
                let _ = sqlx::query(
                    r#"INSERT INTO board_edges (owner_user_id, source_id, source_type, target_id, target_type, source_slot, target_slot)
                       VALUES ($1, $2, 'generate', $3, 'design', 'out', 'sources')
                       ON CONFLICT (source_id, source_slot, target_id, target_slot) DO NOTHING"#,
                )
                .bind(viewer.id)
                .bind(gen_node_id)
                .bind(new_run_id)
                .execute(&state.db)
                .await;

                match render_storm_board_html(&state, viewer.id).await {
                    Ok(board_html) => {
                        yield Ok::<_, Infallible>(
                            PatchElements::new(board_html)
                                .selector("#storm-runs")
                                .mode(ElementPatchMode::Outer)
                                .write_as_axum_sse_event(),
                        );
                        yield Ok::<_, Infallible>(patch_signals(json!({
                            "_generating": false,
                            "_status": "Storm generated.",
                            "_latestRunId": new_run_id.to_string(),
                        }).to_string()));
                    }
                    Err(error) => {
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
        DELETE FROM board_edges
        WHERE owner_user_id = $1 AND (source_id = $2 OR target_id = $2)
        "#,
    )
    .bind(viewer.id)
    .bind(id)
    .execute(&state.db)
    .await?;

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

    let archive_bytes = if tokio::fs::try_exists(&run.workspace_dir).await.unwrap_or(false) {
        let workspace_dir = run.workspace_dir.clone();
        tokio::task::spawn_blocking(move || zip_workspace_dir(&workspace_dir))
            .await
            .map_err(|error| AppError::Internal(error.to_string()))??
    } else if let Some(bytes) = load_persisted_workspace_archive(&state, viewer.id, id).await? {
        bytes
    } else {
        return Err(AppError::BadRequest("Artifact archive unavailable.".to_string()));
    };

    let filename = sanitize_download_name(&run.title, id);
    let mut response = Response::new(Body::from(archive_bytes));
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|error| AppError::Internal(error.to_string()))?,
    );
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("private, max-age=60"));
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
    let storage = state
        .artifact_storage
        .as_ref()
        .ok_or_else(|| AppError::Internal("Asset storage not configured.".to_string()))?;

    let field = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("Invalid multipart request: {e}")))?
        .ok_or_else(|| AppError::BadRequest("No file field in upload.".to_string()))?;

    let file_name = field
        .file_name()
        .unwrap_or("upload")
        .to_string();
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
    let s3_key = format!("assets/{}/{}/{}", viewer.id, asset_id, file_name);

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
    .bind(viewer.id)
    .bind(&file_name)
    .bind(&content_type)
    .bind(data.len() as i64)
    .bind(&s3_key)
    .execute(&state.db)
    .await?;

    let url = format!("/assets/{}/{}", asset_id, file_name);
    Ok(Json(json!({ "id": asset_id.to_string(), "url": url })))
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
        let copied = copy_assets_to_workspace_inputs(state, viewer.id, &input.asset_ids, &workspace_dir).await?;
        info!(
            user_id = %viewer.id,
            run_id = %run_id,
            asset_count = copied.len(),
            "copied assets to workspace inputs/"
        );
    }

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
    headers: HeaderMap,
    Path(run_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let run = get_owned_run(&state, viewer.id, run_id).await?;
    info!(user_id = %viewer.id, run_id = %run_id, "serving preview index");
    let html = match tokio::fs::read_to_string(run.workspace_dir.join("index.html")).await {
        Ok(html) => html,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match load_persisted_workspace_entry(&state, viewer.id, run_id, "index.html").await? {
                Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                None => {
                    warn!(
                        user_id = %viewer.id,
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
    let bytes = match tokio::fs::read(&asset_path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match load_persisted_workspace_entry(&state, viewer.id, run_id, &normalized_path).await? {
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
    let board_nodes = load_board_nodes(state, user_id).await;
    let edges = load_board_edges(state, user_id).await;
    let edges_json = serde_json::to_string(&edges).unwrap_or_else(|_| "[]".to_string());
    Ok(StormBoardTemplate {
        runs: &runs,
        board_nodes: &board_nodes,
        edges_json: &edges_json,
    }
    .render()?)
}

async fn viewer_run_summaries(state: &AppState, user_id: Uuid) -> Vec<StormRunSummary> {
    match sqlx::query_as::<_, StormRunRow>(
        r#"
        SELECT id, owner_user_id, prompt, title, summary, assistant_summary, preview_url,
               submitted, created_at, workspace_dir, parent_ids, position_x, position_y, width, height
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

async fn get_owned_run(
    state: &AppState,
    user_id: Uuid,
    run_id: Uuid,
) -> Result<StormRunRecord, AppError> {
    let run = sqlx::query_as::<_, StormRunRow>(
        r#"
        SELECT id, owner_user_id, prompt, title, summary, assistant_summary, preview_url,
               submitted, created_at, workspace_dir, parent_ids, position_x, position_y, width, height
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
            parent_ids,
            position_x,
            position_y,
            width,
            height
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
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
    .bind(record.position_x)
    .bind(record.position_y)
    .bind(record.width)
    .bind(record.height)
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
            "Design a distinctive design-language document for this seed:\n\n{prompt}\n\nRequirements:\n- produce a full static HTML artifact in the workspace\n- use index.html and styles.css as the primary files\n- if web research helps, use search_web/fetch_url selectively\n- iterate yourself instead of delegating to other agents\n- render and inspect the artifact before finishing\n- an inputs/ directory may contain reference assets (images, fonts) — use copy_input to include any you want in the output\n- call submit_result(title=..., summary=...) once the result is coherent"
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

fn artifact_archive_key(user_id: Uuid, run_id: Uuid) -> String {
    format!("storm-runs/{user_id}/{run_id}.zip")
}

fn sanitize_download_name(title: &str, run_id: Uuid) -> String {
    let mut base = title
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_lowercase() } else { '-' })
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
        .map_err(|error| AppError::Internal(format!("Failed to upload artifact archive: {error}")))?;

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
        .map_err(|error| AppError::Internal(format!("Failed to read persisted artifact archive: {error}")))?
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

fn extract_zip_entry(archive_bytes: &[u8], logical_path: &str) -> Result<Option<Vec<u8>>, AppError> {
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

// ─── Board nodes & edges: data loaders ───

async fn load_board_nodes(state: &AppState, user_id: Uuid) -> Vec<BoardNodeSummary> {
    match sqlx::query_as::<_, BoardNodeRow>(
        r#"
        SELECT id, owner_user_id, node_type, position_x, position_y, content, locked, created_at, width, height
        FROM board_nodes
        WHERE owner_user_id = $1
        ORDER BY created_at ASC
        "#,
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows.into_iter().map(BoardNodeSummary::from).collect(),
        Err(error) => {
            error!(user_id = %user_id, error = %error, "failed to load board nodes");
            Vec::new()
        }
    }
}

async fn load_board_edges(state: &AppState, user_id: Uuid) -> Vec<BoardEdgeSummary> {
    match sqlx::query_as::<_, BoardEdgeRow>(
        r#"
        SELECT id, owner_user_id, source_id, source_type, target_id, target_type,
               source_anchor_side, source_anchor_t, target_anchor_side, target_anchor_t,
               source_slot, target_slot
        FROM board_edges
        WHERE owner_user_id = $1
        ORDER BY created_at ASC
        "#,
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows.into_iter().map(BoardEdgeSummary::from).collect(),
        Err(error) => {
            error!(user_id = %user_id, error = %error, "failed to load board edges");
            Vec::new()
        }
    }
}

// ─── Wikipedia random page ───

async fn fetch_wikipedia_random(http: &Client) -> Result<serde_json::Value, AppError> {
    // Follow the random redirect to get the resolved page URL and title
    let resp = http
        .get("https://en.wikipedia.org/wiki/Special:Random")
        .header("User-Agent", "DesignStorm/1.0 (contact@designstorm.app)")
        .send()
        .await?;
    let url = resp.url().to_string();
    let title = url
        .rsplit_once("/wiki/")
        .map(|(_, slug)| {
            // Decode percent-encoded UTF-8 in the slug
            let bytes: Vec<u8> = slug.as_bytes().iter().copied().collect();
            let mut decoded = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' && i + 2 < bytes.len() {
                    if let (Some(hi), Some(lo)) = (
                        char::from(bytes[i + 1]).to_digit(16),
                        char::from(bytes[i + 2]).to_digit(16),
                    ) {
                        decoded.push((hi * 16 + lo) as u8);
                        i += 3;
                        continue;
                    }
                }
                decoded.push(bytes[i]);
                i += 1;
            }
            String::from_utf8(decoded)
                .unwrap_or_else(|_| slug.to_string())
                .replace('_', " ")
        })
        .unwrap_or_else(|| "Random page".to_string());
    Ok(json!({
        "title": title,
        "url": url,
    }))
}

// ─── Board node route handlers ───

fn valid_board_node_type(node_type: &str) -> bool {
    matches!(
        node_type,
        "entropy"
            | "user_input"
            | "generate"
            | "color_palette"
            | "pick_k"
            | "set"
            | "image"
            | "int_value"
            | "float_value"
            | "string_value"
            | "bool_value"
            | "font"
    )
}

struct SlotDef {
    name: &'static str,
    direction: &'static str,
    value_type: &'static str,
    multiple: bool,
}

fn node_slots(node_type: &str) -> &'static [SlotDef] {
    match node_type {
        "entropy" => &[SlotDef { name: "out", direction: "out", value_type: "node_ref", multiple: false }],
        "user_input" => &[SlotDef { name: "out", direction: "out", value_type: "node_ref", multiple: false }],
        "design" => &[SlotDef { name: "out", direction: "out", value_type: "node_ref", multiple: false }],
        "generate" => &[
            SlotDef { name: "sources", direction: "in", value_type: "node_ref", multiple: true },
            SlotDef { name: "out", direction: "out", value_type: "node_ref", multiple: false },
        ],
        "color_palette" => &[
            SlotDef { name: "out", direction: "out", value_type: "node_ref", multiple: false },
            SlotDef { name: "colors_out", direction: "out", value_type: "color[]", multiple: false },
        ],
        "image" => &[
            SlotDef { name: "out", direction: "out", value_type: "node_ref", multiple: false },
            SlotDef { name: "image_out", direction: "out", value_type: "image", multiple: false },
        ],
        "set" => &[
            SlotDef { name: "members", direction: "in", value_type: "node_ref", multiple: true },
            SlotDef { name: "out", direction: "out", value_type: "node_ref", multiple: false },
        ],
        "pick_k" => &[
            SlotDef { name: "sources", direction: "in", value_type: "node_ref", multiple: true },
            SlotDef { name: "k", direction: "in", value_type: "int", multiple: false },
            SlotDef { name: "out", direction: "out", value_type: "node_ref", multiple: false },
        ],
        "int_value" => &[SlotDef { name: "value", direction: "out", value_type: "int", multiple: false }],
        "float_value" => &[SlotDef { name: "value", direction: "out", value_type: "float", multiple: false }],
        "string_value" => &[SlotDef { name: "value", direction: "out", value_type: "string", multiple: false }],
        "bool_value" => &[SlotDef { name: "value", direction: "out", value_type: "bool", multiple: false }],
        "font" => &[
            SlotDef { name: "out", direction: "out", value_type: "node_ref", multiple: false },
        ],
        _ => &[],
    }
}

fn valid_slot_connection(
    source_type: &str, source_slot: &str,
    target_type: &str, target_slot: &str,
) -> bool {
    let src_slots = node_slots(source_type);
    let tgt_slots = node_slots(target_type);
    let src = src_slots.iter().find(|s| s.name == source_slot && s.direction == "out");
    let tgt = tgt_slots.iter().find(|s| s.name == target_slot && s.direction == "in");
    match (src, tgt) {
        (Some(s), Some(t)) => {
            s.value_type == t.value_type
                || s.value_type == "any"
                || t.value_type == "any"
        }
        _ => false,
    }
}

fn valid_board_connection(source_type: &str, target_type: &str) -> bool {
    // Check if there's any valid slot connection between these types
    let src_slots = node_slots(source_type);
    let tgt_slots = node_slots(target_type);
    for s in src_slots {
        if s.direction != "out" { continue; }
        for t in tgt_slots {
            if t.direction != "in" { continue; }
            if s.value_type == t.value_type || s.value_type == "any" || t.value_type == "any" {
                return true;
            }
        }
    }
    false
}

fn default_slots_for_connection(source_type: &str, target_type: &str) -> (&'static str, &'static str) {
    let src_slots = node_slots(source_type);
    let tgt_slots = node_slots(target_type);
    for s in src_slots {
        if s.direction != "out" { continue; }
        for t in tgt_slots {
            if t.direction != "in" { continue; }
            if s.value_type == t.value_type || s.value_type == "any" || t.value_type == "any" {
                return (s.name, t.name);
            }
        }
    }
    ("out", "sources")
}

async fn default_board_node_content(
    state: &AppState,
    node_type: &str,
) -> Result<serde_json::Value, AppError> {
    match node_type {
        "entropy" => fetch_wikipedia_random(&state.http).await,
        "color_palette" => Ok(json!({"colors": []})),
        "pick_k" => Ok(json!({"k": 1, "replace": false})),
        "set" => Ok(json!({"title": "Set", "description": ""})),
        "image" => Ok(json!({"url": "", "alt": ""})),
        "int_value" => Ok(json!({"value": 0, "min": 0, "max": 100, "random": false})),
        "float_value" => Ok(json!({"value": 0.0, "min": 0.0, "max": 1.0, "step": 0.01, "random": false})),
        "string_value" => Ok(json!({"value": ""})),
        "bool_value" => Ok(json!({"value": false})),
        "font" => Ok(json!({"family": "", "weight": "400", "size": "16", "lineHeight": "1.5"})),
        _ => Ok(json!({})),
    }
}

async fn insert_board_node(
    state: &AppState,
    user_id: Uuid,
    node_type: &str,
    position_x: f64,
    position_y: f64,
) -> Result<BoardNodeRow, AppError> {
    insert_board_node_with_preset(state, user_id, node_type, position_x, position_y, None).await
}

async fn insert_board_node_with_preset(
    state: &AppState,
    user_id: Uuid,
    node_type: &str,
    position_x: f64,
    position_y: f64,
    preset_id: Option<&str>,
) -> Result<BoardNodeRow, AppError> {
    let content = if let Some(pid) = preset_id {
        if let Some(preset) = find_preset(pid) {
            let value = random_preset_value(preset);
            json!({"value": value, "preset": pid})
        } else {
            default_board_node_content(state, node_type).await?
        }
    } else {
        default_board_node_content(state, node_type).await?
    };
    Ok(sqlx::query_as::<_, BoardNodeRow>(
        r#"
        INSERT INTO board_nodes (owner_user_id, node_type, position_x, position_y, content)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, owner_user_id, node_type, position_x, position_y, content, locked, created_at, width, height
        "#,
    )
    .bind(user_id)
    .bind(node_type)
    .bind(position_x)
    .bind(position_y)
    .bind(&content)
    .fetch_one(&state.db)
    .await?)
}

async fn duplicate_board_node(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<DuplicateNodeRequest>,
) -> Result<Json<BoardNodeSummary>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    let source = sqlx::query_as::<_, BoardNodeRow>(
        "SELECT id, owner_user_id, node_type, position_x, position_y, content, locked, created_at, width, height FROM board_nodes WHERE id = $1 AND owner_user_id = $2",
    )
    .bind(payload.source_id)
    .bind(viewer.id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::BadRequest("Source node not found.".to_string()))?;

    let offset = 40.0;
    let row = sqlx::query_as::<_, BoardNodeRow>(
        r#"
        INSERT INTO board_nodes (owner_user_id, node_type, position_x, position_y, content, width, height)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, owner_user_id, node_type, position_x, position_y, content, locked, created_at, width, height
        "#,
    )
    .bind(viewer.id)
    .bind(&source.node_type)
    .bind(payload.position_x.unwrap_or(source.position_x + offset))
    .bind(payload.position_y.unwrap_or(source.position_y + offset))
    .bind(&source.content)
    .bind(source.width)
    .bind(source.height)
    .fetch_one(&state.db)
    .await?;

    info!(
        user_id = %viewer.id,
        node_id = %row.id,
        source_id = %payload.source_id,
        node_type = %row.node_type,
        "duplicated board node"
    );

    Ok(Json(BoardNodeSummary::from(row)))
}

async fn create_board_node(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateNodeRequest>,
) -> Result<Json<BoardNodeSummary>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    if !valid_board_node_type(&payload.node_type) {
        return Err(AppError::BadRequest(format!(
            "Invalid node_type: {}",
            payload.node_type
        )));
    }
    let row = insert_board_node(
        &state,
        viewer.id,
        &payload.node_type,
        payload.position_x,
        payload.position_y,
    )
    .await?;

    info!(
        user_id = %viewer.id,
        node_id = %row.id,
        node_type = %row.node_type,
        "created board node"
    );

    Ok(Json(BoardNodeSummary::from(row)))
}

async fn create_board_node_datastar(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(payload): Form<CreateNodeFormRequest>,
) -> Result<impl IntoResponse, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    if !valid_board_node_type(&payload.node_type) {
        return Ok(datastar_event_stream(Box::pin(stream! {
            yield Ok::<_, Infallible>(patch_signals(json!({
                "_status": format!("Invalid node type: {}", payload.node_type),
            }).to_string()));
            })));
    }

    if let Some(source_type) = payload.source_type.as_deref()
        && payload.source_id.is_some()
        && !valid_board_connection(source_type, &payload.node_type)
    {
        return Ok(datastar_event_stream(Box::pin(stream! {
            yield Ok::<_, Infallible>(patch_signals(json!({
                "_status": "Invalid node connection.",
            }).to_string()));
        })));
    }

    let preset_id = payload.preset.as_deref().filter(|s| !s.is_empty());
    let (effective_node_type, effective_preset) = if preset_id.is_some() {
        ("string_value", preset_id)
    } else {
        (payload.node_type.as_str(), None)
    };

    let row = insert_board_node_with_preset(
        &state,
        viewer.id,
        effective_node_type,
        payload.position_x,
        payload.position_y,
        effective_preset,
    )
    .await?;

    if let (Some(source_id), Some(source_type)) = (payload.source_id, payload.source_type.as_deref()) {
        let (src_slot, tgt_slot) = default_slots_for_connection(source_type, &payload.node_type);
        let _ = sqlx::query_as::<_, BoardEdgeRow>(
            r#"
            INSERT INTO board_edges (owner_user_id, source_id, source_type, target_id, target_type,
                                     source_anchor_side, source_anchor_t, target_anchor_side, target_anchor_t,
                                     source_slot, target_slot)
            VALUES ($1, $2, $3, $4, $5, $6, $7, 'top', 0.5, $8, $9)
            ON CONFLICT (source_id, source_slot, target_id, target_slot) DO UPDATE SET
                source_type = EXCLUDED.source_type,
                source_anchor_side = EXCLUDED.source_anchor_side,
                source_anchor_t = EXCLUDED.source_anchor_t,
                target_anchor_side = EXCLUDED.target_anchor_side,
                target_anchor_t = EXCLUDED.target_anchor_t
            RETURNING id, owner_user_id, source_id, source_type, target_id, target_type,
                      source_anchor_side, source_anchor_t, target_anchor_side, target_anchor_t,
                      source_slot, target_slot
            "#,
        )
        .bind(viewer.id)
        .bind(source_id)
        .bind(source_type)
        .bind(row.id)
        .bind(&payload.node_type)
        .bind(&payload.source_anchor_side)
        .bind(payload.source_anchor_t)
        .bind(src_slot)
        .bind(tgt_slot)
        .fetch_one(&state.db)
        .await?;
    }

    let board_html = render_storm_board_html(&state, viewer.id).await?;
    Ok(datastar_event_stream(Box::pin(stream! {
        yield Ok::<_, Infallible>(
            PatchElements::new(board_html)
                .selector("#storm-runs")
                .mode(ElementPatchMode::Outer)
                .write_as_axum_sse_event(),
        );
    })))
}

async fn list_presets() -> impl IntoResponse {
    let list: Vec<serde_json::Value> = PRESETS.iter().map(|p| json!({
        "id": p.id, "label": p.label, "icon": p.icon,
    })).collect();
    Json(list)
}

async fn reroll_board_node(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let viewer = require_viewer(&state, &headers).await?;

    // Check if this is a preset node — if so, reroll from the preset list
    let existing = sqlx::query_as::<_, BoardNodeRow>(
        "SELECT id, owner_user_id, node_type, position_x, position_y, content, locked, created_at, width, height FROM board_nodes WHERE id = $1 AND owner_user_id = $2",
    )
    .bind(id)
    .bind(viewer.id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::BadRequest("Node not found.".to_string()))?;

    let content = if let Some(preset_id) = existing.content.get("preset").and_then(|v| v.as_str()) {
        if let Some(preset) = find_preset(preset_id) {
            let value = random_preset_value(preset);
            json!({"value": value, "preset": preset_id})
        } else {
            fetch_wikipedia_random(&state.http).await?
        }
    } else {
        fetch_wikipedia_random(&state.http).await?
    };

    sqlx::query(
        r#"UPDATE board_nodes SET content = $1 WHERE id = $2 AND owner_user_id = $3"#,
    )
    .bind(&content)
    .bind(id)
    .bind(viewer.id)
    .execute(&state.db)
    .await?;

    let board_html = render_storm_board_html(&state, viewer.id).await?;
    Ok(datastar_event_stream(Box::pin(stream! {
        yield Ok::<_, Infallible>(
            PatchElements::new(board_html)
                .selector("#storm-runs")
                .mode(ElementPatchMode::Outer)
                .write_as_axum_sse_event(),
        );
    })))
}

async fn toggle_board_node_lock(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let viewer = require_viewer(&state, &headers).await?;

    sqlx::query(
        r#"UPDATE board_nodes SET locked = NOT locked WHERE id = $1 AND owner_user_id = $2"#,
    )
    .bind(id)
    .bind(viewer.id)
    .execute(&state.db)
    .await?;

    let board_html = render_storm_board_html(&state, viewer.id).await?;
    Ok(datastar_event_stream(Box::pin(stream! {
        yield Ok::<_, Infallible>(
            PatchElements::new(board_html)
                .selector("#storm-runs")
                .mode(ElementPatchMode::Outer)
                .write_as_axum_sse_event(),
        );
    })))
}

async fn update_board_node_position(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdatePositionRequest>,
) -> Result<StatusCode, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    sqlx::query(
        r#"UPDATE board_nodes SET position_x = $1, position_y = $2, width = $3, height = $4 WHERE id = $5 AND owner_user_id = $6"#,
    )
    .bind(payload.position_x)
    .bind(payload.position_y)
    .bind(payload.width)
    .bind(payload.height)
    .bind(id)
    .bind(viewer.id)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn update_storm_run_position(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdatePositionRequest>,
) -> Result<StatusCode, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    sqlx::query(
        r#"UPDATE storm_runs SET position_x = $1, position_y = $2, width = $3, height = $4 WHERE id = $5 AND owner_user_id = $6"#,
    )
    .bind(payload.position_x)
    .bind(payload.position_y)
    .bind(payload.width)
    .bind(payload.height)
    .bind(id)
    .bind(viewer.id)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn update_board_node_content(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(payload): Json<serde_json::Value>,
) -> Result<StatusCode, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    sqlx::query(
        r#"UPDATE board_nodes SET content = $1 WHERE id = $2 AND owner_user_id = $3"#,
    )
    .bind(&payload)
    .bind(id)
    .bind(viewer.id)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_board_node(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let viewer = require_viewer(&state, &headers).await?;

    // Delete edges referencing this node
    sqlx::query(
        r#"DELETE FROM board_edges WHERE owner_user_id = $1 AND (source_id = $2 OR target_id = $2)"#,
    )
    .bind(viewer.id)
    .bind(id)
    .execute(&state.db)
    .await?;

    sqlx::query(r#"DELETE FROM board_nodes WHERE id = $1 AND owner_user_id = $2"#)
        .bind(id)
        .bind(viewer.id)
        .execute(&state.db)
        .await?;

    let board_html = render_storm_board_html(&state, viewer.id).await?;

    Ok(datastar_event_stream(Box::pin(stream! {
        yield Ok::<_, Infallible>(
            PatchElements::new(board_html)
                .selector("#storm-runs")
                .mode(ElementPatchMode::Outer)
                .write_as_axum_sse_event(),
        );
    })))
}

async fn create_board_edge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateEdgeRequest>,
) -> Result<Json<BoardEdgeSummary>, AppError> {
    let viewer = require_viewer(&state, &headers).await?;

    if !valid_slot_connection(
        &payload.source_type, &payload.source_slot,
        &payload.target_type, &payload.target_slot,
    ) {
        return Err(AppError::BadRequest(format!(
            "Invalid connection: {}.{} → {}.{}",
            payload.source_type, payload.source_slot, payload.target_type, payload.target_slot
        )));
    }

    let row = sqlx::query_as::<_, BoardEdgeRow>(
        r#"
        INSERT INTO board_edges (owner_user_id, source_id, source_type, target_id, target_type,
                                 source_anchor_side, source_anchor_t, target_anchor_side, target_anchor_t,
                                 source_slot, target_slot)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (source_id, source_slot, target_id, target_slot) DO UPDATE SET
            source_type = EXCLUDED.source_type,
            source_anchor_side = EXCLUDED.source_anchor_side,
            source_anchor_t = EXCLUDED.source_anchor_t,
            target_anchor_side = EXCLUDED.target_anchor_side,
            target_anchor_t = EXCLUDED.target_anchor_t
        RETURNING id, owner_user_id, source_id, source_type, target_id, target_type,
                  source_anchor_side, source_anchor_t, target_anchor_side, target_anchor_t,
                  source_slot, target_slot
        "#,
    )
    .bind(viewer.id)
    .bind(payload.source_id)
    .bind(&payload.source_type)
    .bind(payload.target_id)
    .bind(&payload.target_type)
    .bind(&payload.source_anchor_side)
    .bind(payload.source_anchor_t)
    .bind(&payload.target_anchor_side)
    .bind(payload.target_anchor_t)
    .bind(&payload.source_slot)
    .bind(&payload.target_slot)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(BoardEdgeSummary::from(row)))
}

async fn delete_board_edge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let viewer = require_viewer(&state, &headers).await?;
    sqlx::query(r#"DELETE FROM board_edges WHERE id = $1 AND owner_user_id = $2"#)
        .bind(id)
        .bind(viewer.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
