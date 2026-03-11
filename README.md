# Design Storm

Design Storm is a Rust + Datastar app for chat-first design iteration. Each workspace is a session with one agent thread, a reference shelf, an async job queue, and a gallery of generated designs.

## Product model

- `/app` is a server-rendered studio, not a client SPA shell.
- Users work inside sessions. Each session has:
  - one chat thread
  - session-scoped references (`text`, `link`, `image`)
  - async design jobs
  - a gallery of finished designs
- The agent can answer conversationally or call `make_design()` once per turn.
- `make_design()` snapshots the selected references, optionally links to a prior design with `iterates_on_id`, and spawns an async generation job.
- Pending jobs render immediately in the gallery and resolve into finished design cards when generation completes.
- Generated workspaces are persisted locally and optionally archived to S3-compatible object storage for preview/download recovery.

## Stack

- Rust + Axum
- Askama templates
- Datastar for HTML-first interactivity
- PostgreSQL via SQLx
- Bun for frontend bundling
- Clerk for auth
- Optional S3-compatible archive storage for generated workspaces

## Main routes

- `GET /app`
- `POST /sessions`
- `POST /sessions/{id}/rename`
- `GET /sessions/{id}/snapshot`
- `POST /sessions/{id}/messages`
- `POST /sessions/{id}/references`
- `POST /sessions/{id}/references/image`
- `GET /storms/{id}/download`
- `GET /preview/{run_id}/...`

## Persistence

Core tables:

- `design_sessions`
- `session_messages`
- `reference_items`
- `design_jobs`
- `storm_runs`

Important relationships:

- `design_jobs.session_id -> design_sessions.id`
- `session_messages.design_job_id -> design_jobs.id`
- `reference_items.session_id -> design_sessions.id`
- `storm_runs.session_id -> design_sessions.id`
- `storm_runs.iterates_on_id -> storm_runs.id`
- `design_jobs.iterates_on_id -> storm_runs.id`

Migration `0013_chat_sessions.sql` also removes the old `board_nodes` and `board_edges` tables.

## Local development

1. Copy `.env.example` to `.env` and fill in real values.
2. Run `bun install`.
3. Run `bun run build:assets`.
4. Start Postgres and set `DATABASE_URL`.
5. If you want archived workspace persistence locally, also set `AWS_REGION`, `BUCKET_NAME`, and an S3-compatible `AWS_ENDPOINT_URL_S3`.
6. Run `cargo run`.

The app runs SQL migrations automatically on startup.

## Notes

- Preview routes prefer the live workspace on disk and fall back to the archived ZIP when needed.
- The active UI is the session sidebar + gallery + chat rail in [templates/app.html](/home/sam/code/designstorm/templates/app.html).
- The old board/node runtime has been removed from the active product and schema.
