# Design Storm

Design Storm is a Rust + Datastar web app for exploring AI-generated design languages on a persistent, node-based board.

## Stack

- Rust + Axum server
- Askama server-rendered templates
- Datastar for server-first HTML-over-the-wire interactions and SSE patches
- Clerk JS SDK for login
- PostgreSQL for runs, board nodes, edges, and layout persistence
- S3-compatible object storage for archived generated workspaces and preview recovery
- Bun for building the frontend asset
- Fly.io for deploy
- Fly Managed Postgres for primary data persistence

## Current App Model

- `/app` is a server-rendered Datastar workspace, not an SPA shell.
- Storm runs, board nodes, edges, roots, and layout are persisted in Postgres.
- Node creation is server-first: Datastar posts to the backend and the board morphs from server-rendered HTML.
- Long-running generation flows stream Datastar SSE patches for board updates, roots updates, and local UI signals.
- Generated artifact workspaces are zipped and uploaded to S3-compatible storage after successful runs.
- Preview routes serve local workspace files when available and fall back to the archived workspace bundle when needed.
- The board uses a seamless world-space background, DOM-rendered nodes, and SVG connection lines.
- The board supports Figma-style controls: wheel pan, `Ctrl`/`Cmd` + wheel zoom, `Space` to pan-drag, right-click radial creation, and a bottom tool dock.
- Result cards and board nodes support persisted drag and resize.
- Output cards expose a top-right menu with `Download ZIP`.

## Local development

1. Copy `.env.example` to `.env` and fill in real values.
2. Run `bun install`.
3. Run `bun run build:assets`.
4. Start Postgres and set `DATABASE_URL`.
5. If you want archived workspace persistence locally, also set `AWS_REGION`, `BUCKET_NAME`, and an S3-compatible `AWS_ENDPOINT_URL_S3`.
6. Run `cargo run`.

## Deploy

- GitHub Actions workflow: `.github/workflows/deploy.yml`
- Fly app config: `fly.toml`
- The workflow expects secrets and variables on the GitHub `production` environment.
