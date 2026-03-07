# Design Storm

Design Storm is a Rust + Datastar web app for exploring AI-generated design languages.

## Stack

- Rust + Axum server
- Askama server-rendered templates
- Datastar for HTML-over-the-wire interactions
- Clerk JS SDK for login
- Bun for building the small frontend auth asset
- Fly.io for deploy
- Fly Managed Postgres for user persistence

## Local development

1. Copy `.env.example` to `.env` and fill in real values.
2. Run `bun install`.
3. Run `bun run build:assets`.
4. Start Postgres and set `DATABASE_URL`.
5. Run `cargo run`.

## Deploy

- GitHub Actions workflow: `.github/workflows/deploy.yml`
- Fly app config: `fly.toml`
- The workflow expects secrets and variables on the GitHub `production` environment.
