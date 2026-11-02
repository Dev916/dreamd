//! Loopback HTTP API (`dreamd serve`). JSON over HTTP on 127.0.0.1 only;
//! errors are RFC 7807 `application/problem+json`.
//!
//! Dreams started here run as background tasks in the server process and
//! persist their state in SQLite, so the CLI sees them. They record no pid:
//! cancel (from the API or the CLI) works through the status column, which
//! the pipeline checks between stages, and never signals the server.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::apply::{self, ApplyError};
use crate::discover::{self, ScanRow};
use crate::dream::{self, Ctx, DreamOpts, DreamOutcome};
use crate::model::{Dream, DreamStatus, SessionRecord};

/// Shared handler state.
#[derive(Clone)]
pub struct AppState(pub Arc<Ctx>);

/// An RFC 7807 problem: `{type:"about:blank", title, status, detail}`.
#[derive(Debug)]
pub struct Problem {
    pub status: StatusCode,
    pub title: &'static str,
    pub detail: String,
}

impl Problem {
    pub fn new(status: StatusCode, title: &'static str, detail: impl Into<String>) -> Problem {
        Problem {
            status,
            title,
            detail: detail.into(),
        }
    }

    fn not_found(detail: impl Into<String>) -> Problem {
        Problem::new(StatusCode::NOT_FOUND, "not found", detail)
    }

    fn refused(detail: impl std::fmt::Display) -> Problem {
        Problem::new(
            StatusCode::CONFLICT,
            "refused",
            format!("refused: {detail}"),
        )
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let body = json!({
            "type": "about:blank",
            "title": self.title,
            "status": self.status.as_u16(),
            "detail": self.detail,
        });
        (
            self.status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            body.to_string(),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for Problem {
    fn from(e: anyhow::Error) -> Problem {
        tracing::error!("api: {e:#}");
        Problem::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error",
            format!("{e:#}"),
        )
    }
}

impl From<ApplyError> for Problem {
    fn from(e: ApplyError) -> Problem {
        match e {
            ApplyError::NotFound => Problem::not_found("dream not found"),
            e @ (ApplyError::BadStatus(_) | ApplyError::HashMismatch) => Problem::refused(e),
            ApplyError::Io(e) => {
                tracing::error!("api: apply/rollback: {e}");
                Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "failed", e.to_string())
            }
        }
    }
}

type ApiResult<T> = Result<T, Problem>;

/// `POST /v1/dreams` body.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateDream {
    /// Slug, or a path compared with each project's cwd.
    pub project: String,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub session_ids: Option<Vec<String>>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApplyQuery {
    #[serde(default)]
    force: bool,
}

/// The API routes.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/projects", get(projects))
        .route("/v1/dreams", get(list_dreams).post(create_dream))
        .route("/v1/dreams/{id}", get(show_dream))
        .route("/v1/dreams/{id}/diff", get(diff))
        .route("/v1/dreams/{id}/sessions", get(sessions))
        .route("/v1/dreams/{id}/cancel", post(cancel))
        .route("/v1/dreams/{id}/apply", post(apply_dream))
        .route("/v1/dreams/{id}/rollback", post(rollback_dream))
        .fallback(|| async { Problem::not_found("no such route") })
        .with_state(state)
}

/// Bind `127.0.0.1:port` (0 = ephemeral), print
/// `listening on http://127.0.0.1:<port>`, serve until ctrl-c.
pub async fn serve(ctx: Ctx, port: u16) -> anyhow::Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    println!("listening on http://{bound}");
    let app = router(AppState(Arc::new(ctx)));
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Run blocking store/filesystem work off the async runtime.
async fn blocking<T, F>(f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> ApiResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Problem::from(anyhow::anyhow!("task panicked: {e}")))?
}

fn load(ctx: &Ctx, id: &str) -> ApiResult<Dream> {
    ctx.db
        .get_dream(id)?
        .ok_or_else(|| Problem::not_found(format!("dream not found: {id}")))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({"ok": true}))
}

async fn projects(State(AppState(ctx)): State<AppState>) -> ApiResult<Json<Vec<ScanRow>>> {
    blocking(move || {
        let found = discover::discover(&ctx.cfg, &ctx.db, chrono::Utc::now())?;
        Ok(Json(
            found.iter().map(|d| ScanRow::new(d, &ctx.cfg)).collect(),
        ))
    })
    .await
}

async fn list_dreams(
    State(AppState(ctx)): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Dream>>> {
    let status = match q.status.as_deref() {
        None | Some("") => None,
        Some(s) => Some(s.parse::<DreamStatus>().map_err(|_| {
            Problem::new(
                StatusCode::BAD_REQUEST,
                "bad request",
                format!("unknown status: {s}"),
            )
        })?),
    };
    blocking(move || Ok(Json(ctx.db.list_dreams(status)?))).await
}

async fn create_dream(
    State(AppState(ctx)): State<AppState>,
    Json(body): Json<CreateDream>,
) -> ApiResult<(StatusCode, Json<Dream>)> {
    let opts = DreamOpts {
        force: body.force,
        model: body.model,
        instructions: body.instructions,
        session_ids: body.session_ids.filter(|v| !v.is_empty()),
        dry_run: false,
        no_pid: true,
    };
    let selector = body.project;
    let ctx2 = Arc::clone(&ctx);
    let opts2 = opts.clone();
    let outcome = blocking(move || {
        let found = discover::discover(&ctx2.cfg, &ctx2.db, chrono::Utc::now())?;
        let Some(d) = discover::select_projects(found, Some(&selector))
            .into_iter()
            .next()
        else {
            return Err(Problem::not_found(format!("project not found: {selector}")));
        };
        Ok(dream::prepare(&ctx2, &d, &opts2)?)
    })
    .await?;
    let pending = match outcome {
        DreamOutcome::Done(d) => d,
        DreamOutcome::Skipped(why) => {
            return Err(Problem::new(StatusCode::CONFLICT, "skipped", why));
        }
    };
    let task_dream = pending.clone();
    tokio::spawn(async move {
        let id = task_dream.id.clone();
        match dream::execute(&ctx, task_dream, opts).await {
            Ok(d) => tracing::info!("dream {id} {}", d.status),
            Err(e) => tracing::error!("dream {id}: {e:#}"),
        }
    });
    Ok((StatusCode::ACCEPTED, Json(pending)))
}

async fn show_dream(
    State(AppState(ctx)): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Dream>> {
    blocking(move || Ok(Json(load(&ctx, &id)?))).await
}

async fn diff(
    State(AppState(ctx)): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    blocking(move || {
        let d = load(&ctx, &id)?;
        match std::fs::read(dream::dir_of(&d).join("dream.diff")) {
            Ok(bytes) => {
                Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], bytes).into_response())
            }
            Err(_) => Err(Problem::new(
                StatusCode::CONFLICT,
                "not ready",
                format!("no dream.diff for {id} (dream is {})", d.status),
            )),
        }
    })
    .await
}

async fn sessions(
    State(AppState(ctx)): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<SessionRecord>>> {
    blocking(move || {
        load(&ctx, &id)?;
        Ok(Json(ctx.db.list_sessions(&id)?))
    })
    .await
}

/// Mark a pending/running dream canceled (DB only; no signal).
async fn cancel(
    State(AppState(ctx)): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Dream>> {
    blocking(move || {
        let d = load(&ctx, &id)?;
        if d.status.is_terminal() {
            return Err(Problem::refused(format!("dream is {}", d.status)));
        }
        ctx.db.update_status(&id, DreamStatus::Canceled, None)?;
        Ok(Json(load(&ctx, &id)?))
    })
    .await
}

async fn apply_dream(
    State(AppState(ctx)): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ApplyQuery>,
) -> ApiResult<Json<Dream>> {
    blocking(move || {
        apply::apply(&ctx, &id, q.force)?;
        Ok(Json(load(&ctx, &id)?))
    })
    .await
}

async fn rollback_dream(
    State(AppState(ctx)): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Dream>> {
    blocking(move || {
        apply::rollback(&ctx, &id)?;
        Ok(Json(load(&ctx, &id)?))
    })
    .await
}
