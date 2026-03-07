use askama::Template;
use axum::{
    Router,
    extract::{Json, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CONTENT_TYPE, COOKIE, SET_COOKIE},
    },
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cookie::{Cookie, SameSite};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, PgPool, postgres::PgPoolOptions};
use std::{env, net::SocketAddr, sync::Arc};
use thiserror::Error;
use time::OffsetDateTime;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::info;
use uuid::Uuid;

const SESSION_COOKIE_NAME: &str = "designstorm_session";
const DATSTAR_CDN: &str =
    "https://cdn.jsdelivr.net/gh/starfederation/[email protected]/bundles/datastar.js";

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
            | AppError::Migration(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
}

#[derive(Template)]
#[template(path = "auth_panel.html")]
struct AuthPanelTemplate<'a> {
    viewer: Option<&'a Viewer>,
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    dotenvy::dotenv().ok();
    init_tracing();

    let config = Arc::new(Config::from_env()?);
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
        config,
    };

    info!(
        clerk_issuer = %state.config.clerk_issuer,
        clerk_jwks_url = %state.config.clerk_jwks_url,
        bucket_name = ?state.config.bucket_name,
        aws_region = ?state.config.aws_region,
        aws_endpoint_url_s3 = ?state.config.aws_endpoint_url_s3,
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
        datastar_cdn: DATSTAR_CDN,
        auth_panel: &auth_panel,
        app_config_json: &config_json,
    };

    Ok(Html(page.render()?))
}

async fn app_page(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    let Some(viewer) = current_viewer(&state, &headers).await? else {
        return Ok(Redirect::temporary("/").into_response());
    };

    let config_json = json!({
        "clerkPublishableKey": state.config.clerk_publishable_key,
        "appUrl": state.config.app_url,
        "hasServerSession": true,
        "currentPath": "/app",
    })
    .to_string();

    let page = AppTemplate {
        title: "Design Storm / App",
        body_class: "app-page",
        datastar_cdn: DATSTAR_CDN,
        viewer: &viewer,
        app_config_json: &config_json,
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
