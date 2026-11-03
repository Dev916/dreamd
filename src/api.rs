//! Loopback HTTP API (`dreamd serve`). JSON over HTTP on 127.0.0.1 only;
//! errors are RFC 7807 `application/problem+json`.
//!
//! Request guards (one middleware layer, in order), because binding to
//! loopback alone does not stop a web page in the user's browser from
//! reaching the API (DNS rebinding, cross-site POST):
//! 1. `Host` must be `127.0.0.1` or `localhost`, bare or with the bound
//!    port → else 403 `forbidden`. Defeats DNS rebinding.
//! 2. An `Origin` header other than exactly `null` → 403 `forbidden`.
//!    Browsers send Origin on cross-site requests; curl/SDKs never do.
//! 3. Every route but `GET /healthz` needs `Authorization: Bearer <token>`
//!    matching `<data_dir>/api-token` (constant-time) → else 401.
//! 4. POST: a `Content-Type`, if present, must be `application/json`
//!    (parameters allowed); `POST /v1/dreams` must send one → else 415.
//!
//! Dreams started here run as background tasks in the server process and
//! persist their state in SQLite, so the CLI sees them. They record no pid:
//! cancel (from the API or the CLI) works through the status column, which
//! the pipeline checks between stages, and never signals the server. They
//! hold the project lock like CLI dreams (`POST /v1/dreams` → 409 `busy`
//! while another live process dreams that project), and `serve` sweeps
//! orphaned dreams at startup.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::apply::{self, ApplyError};
use crate::discover::{self, ScanRow};
use crate::dream::{self, Ctx, DreamOpts, DreamOutcome, Prepared};
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
    #[serde(default)]
    include_archived: bool,
}

#[derive(Debug, Deserialize)]
struct ApplyQuery {
    #[serde(default)]
    force: bool,
}

/// What the request guards check against: the bearer token and the port
/// the server is bound to (for the `Host` allowlist).
#[derive(Debug, Clone)]
pub struct Guard {
    pub token: String,
    pub port: u16,
}

impl Guard {
    fn host_ok(&self, host: &str) -> bool {
        let (name, port) = match host.rsplit_once(':') {
            Some((n, p)) => (n, Some(p)),
            None => (host, None),
        };
        let name_ok = name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost");
        let port_ok = match port {
            None => true,
            Some(p) => p.parse::<u16>().ok() == Some(self.port),
        };
        name_ok && port_ok
    }

    fn token_ok(&self, headers: &HeaderMap) -> bool {
        let Some(v) = headers.get(header::AUTHORIZATION) else {
            return false;
        };
        let Some(given) = v.as_bytes().strip_prefix(b"Bearer ") else {
            return false;
        };
        crate::token::ct_eq(given, self.token.as_bytes())
    }
}

fn forbidden(detail: String) -> Problem {
    Problem::new(StatusCode::FORBIDDEN, "forbidden", detail)
}

fn unsupported(detail: impl Into<String>) -> Problem {
    Problem::new(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported media type",
        detail,
    )
}

/// The request guards (see module docs). Runs before routing, so it also
/// covers the fallback.
async fn guard(State(g): State<Arc<Guard>>, req: Request, next: Next) -> Response {
    match check(&g, &req) {
        Ok(()) => next.run(req).await,
        Err(p) => p.into_response(),
    }
}

fn check(g: &Guard, req: &Request) -> Result<(), Problem> {
    let headers = req.headers();
    // 1. Host (HTTP/1: Host header; HTTP/2: :authority).
    let host = match headers.get(header::HOST) {
        Some(h) => h.to_str().ok().map(str::to_string),
        None => req.uri().authority().map(|a| a.to_string()),
    };
    match host {
        Some(h) if g.host_ok(&h) => {}
        Some(h) => return Err(forbidden(format!("host not allowed: {h:?}"))),
        None => return Err(forbidden("missing host header".into())),
    }
    // 2. Origin.
    if let Some(o) = headers.get(header::ORIGIN) {
        if o.as_bytes() != b"null" {
            return Err(forbidden(format!(
                "cross-origin requests are not allowed (origin {:?})",
                String::from_utf8_lossy(o.as_bytes())
            )));
        }
    }
    // 3. Bearer token on everything but /healthz.
    if req.uri().path() != "/healthz" && !g.token_ok(headers) {
        return Err(Problem::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid bearer token (see `dreamd token`)",
        ));
    }
    // 4. JSON content type on POST.
    if req.method() == Method::POST {
        match headers.get(header::CONTENT_TYPE) {
            Some(ct) => {
                let ct = String::from_utf8_lossy(ct.as_bytes());
                let mime = ct.split(';').next().unwrap_or("").trim();
                if !mime.eq_ignore_ascii_case("application/json") {
                    return Err(unsupported(format!(
                        "content-type must be application/json, got {ct:?}"
                    )));
                }
            }
            None if req.uri().path() == "/v1/dreams" => {
                return Err(unsupported("content-type application/json is required"));
            }
            None => {}
        }
    }
    Ok(())
}

/// The API routes, behind the request guards.
pub fn router(state: AppState, guard_cfg: Guard) -> Router {
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
        .route("/v1/dreams/{id}/archive", post(archive_dream))
        .fallback(|| async { Problem::not_found("no such route") })
        .with_state(state)
        .layer(middleware::from_fn_with_state(Arc::new(guard_cfg), guard))
}

/// Bind `127.0.0.1:port` (0 = ephemeral), load or create the API token,
/// print `listening on http://127.0.0.1:<port>` then `token file: <path>`
/// (never the token), serve until ctrl-c.
pub async fn serve(ctx: Ctx, port: u16) -> anyhow::Result<()> {
    // Dreams left pending/running by a dead process (e.g. a killed server).
    for id in dream::sweep_orphans(&ctx)? {
        tracing::warn!("swept orphaned dream {id}");
    }
    let token = crate::token::ensure(&ctx.cfg.data_dir)?;
    let token_file = crate::token::path(&ctx.cfg.data_dir);
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    println!("listening on http://{bound}");
    println!("token file: {}", token_file.display());
    let guard_cfg = Guard {
        token,
        port: bound.port(),
    };
    let app = router(AppState(Arc::new(ctx)), guard_cfg);
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
    blocking(move || Ok(Json(ctx.db.list_dreams(status, q.include_archived)?))).await
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
        max_sessions: None,
        dry_run: false,
        no_pid: true,
    };
    let selector = body.project;
    let ctx2 = Arc::clone(&ctx);
    let opts2 = opts.clone();
    let outcome = blocking(move || {
        let found =
            discover::discover_with(&ctx2.cfg, &ctx2.db, chrono::Utc::now(), None, opts2.force)?;
        let Some(d) = discover::select_projects(found, Some(&selector))
            .into_iter()
            .next()
        else {
            return Err(Problem::not_found(format!("project not found: {selector}")));
        };
        Ok(dream::prepare(&ctx2, &d, &opts2)?)
    })
    .await?;
    let prepared = match outcome {
        Prepared::Ready(p) => p,
        Prepared::Stop(DreamOutcome::Skipped(why)) => {
            return Err(Problem::new(StatusCode::CONFLICT, "skipped", why));
        }
        Prepared::Stop(DreamOutcome::Busy(why)) => {
            return Err(Problem::new(StatusCode::CONFLICT, "busy", why));
        }
        Prepared::Stop(DreamOutcome::Done(d)) => {
            return Err(Problem::from(anyhow::anyhow!(
                "prepare returned a finished dream: {}",
                d.id
            )));
        }
    };
    let pending = prepared.dream.clone();
    tokio::spawn(async move {
        let id = prepared.dream.id.clone();
        match dream::execute(&ctx, prepared, opts).await {
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

/// Archive a terminal dream (409 `refused` while pending/running).
async fn archive_dream(
    State(AppState(ctx)): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Dream>> {
    blocking(move || match dream::archive(&ctx.db, &id) {
        Ok(d) => Ok(Json(d)),
        Err(dream::ArchiveError::NotFound) => {
            Err(Problem::not_found(format!("dream not found: {id}")))
        }
        Err(e @ dream::ArchiveError::Active(_)) => Err(Problem::refused(e)),
        Err(dream::ArchiveError::Db(e)) => Err(e.into()),
    })
    .await
}
