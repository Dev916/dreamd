//! `dreamd serve`: Task 10 acceptance criteria, in process. The router runs
//! on an ephemeral loopback listener with the fixture `Ctx` (stub claude in
//! `auto` mode, notifications off); requests go through `reqwest`.
//!
//! Everything is one test because the fixture is wired through process env
//! (`config::load` reads `DREAMD_*`), which tests in one binary would share.
//!
//! The request guards (bearer token, Host, Origin, JSON content type) are
//! exercised raw first; every other request uses a client that sends the
//! token from `<data_dir>/api-token`.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dreamd::api::{router, AppState, Guard};
use dreamd::claude::{Claude, ClaudeError, ClaudeRequest, ClaudeResult};
use dreamd::dream::Ctx;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};

/// Start `router` on 127.0.0.1:0 with the token from the fixture data dir;
/// return the base URL.
async fn start(ctx: Ctx) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let token = dreamd::token::ensure(&ctx.cfg.data_dir).unwrap();
    let guard = Guard {
        token,
        port: addr.port(),
    };
    let app = router(AppState(Arc::new(ctx)), guard);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn content_type(r: &Response) -> String {
    r.headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

/// Assert an RFC 7807 response; return its body.
async fn problem(r: Response, status: StatusCode, title: &str) -> Value {
    assert_eq!(r.status(), status);
    assert_eq!(content_type(&r), "application/problem+json");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["type"], "about:blank", "{body}");
    assert_eq!(body["title"], title, "{body}");
    assert_eq!(body["status"], status.as_u16(), "{body}");
    assert!(body["detail"].is_string(), "{body}");
    body
}

async fn get_dream(c: &Client, base: &str, id: &str) -> Value {
    let r = c
        .get(format!("{base}/v1/dreams/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

/// Poll until the dream leaves pending/running (30 s cap).
async fn wait_terminal(c: &Client, base: &str, id: &str) -> Value {
    let start = Instant::now();
    loop {
        let d = get_dream(c, base, id).await;
        if d["status"] != "pending" && d["status"] != "running" {
            return d;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "dream {id} stuck: {d}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A model that takes 3 s and then fails: keeps a dream running long
/// enough to cancel it deterministically.
struct SlowClaude;

#[async_trait]
impl Claude for SlowClaude {
    async fn run(&self, _req: ClaudeRequest) -> Result<ClaudeResult, ClaudeError> {
        tokio::time::sleep(Duration::from_secs(3)).await;
        Err(ClaudeError::ModelError("slow stub".into()))
    }
}

/// Guard cases against a live server. `raw` sends no token by default.
async fn guards(base: &str, token: &str) {
    let raw = Client::new();
    let bearer = format!("Bearer {token}");
    let port = base.rsplit(':').next().unwrap().to_string();

    // healthz: open without a token.
    let r = raw.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 200);

    // 401: missing token, wrong token, wrong scheme — on GET and POST routes.
    for (path, post) in [
        ("/v1/projects", false),
        ("/v1/dreams", false),
        ("/v1/dreams/drm_x", false),
        ("/v1/dreams/drm_x/diff", false),
        ("/v1/dreams/drm_x/sessions", false),
        ("/v1/dreams/drm_x/cancel", true),
        ("/v1/dreams/drm_x/apply", true),
        ("/v1/dreams/drm_x/rollback", true),
    ] {
        let url = format!("{base}{path}");
        let req = |auth: Option<String>| {
            let b = if post { raw.post(&url) } else { raw.get(&url) };
            match auth {
                Some(a) => b.header(AUTHORIZATION, a),
                None => b,
            }
        };
        problem(
            req(None).send().await.unwrap(),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        )
        .await;
        let wrong = format!("Bearer {}", "0".repeat(64));
        problem(
            req(Some(wrong)).send().await.unwrap(),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        )
        .await;
        problem(
            req(Some(format!("Basic {token}"))).send().await.unwrap(),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        )
        .await;
        problem(
            req(Some(token.to_string())).send().await.unwrap(),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        )
        .await;
    }
    let r = raw
        .post(format!("{base}/v1/dreams"))
        .json(&json!({"project": "x"}))
        .send()
        .await
        .unwrap();
    problem(r, StatusCode::UNAUTHORIZED, "unauthorized").await;

    // Allowed Host forms (authenticated).
    for host in [
        "127.0.0.1".to_string(),
        format!("127.0.0.1:{port}"),
        "localhost".to_string(),
        format!("localhost:{port}"),
    ] {
        let r = raw
            .get(format!("{base}/v1/dreams"))
            .header("host", &host)
            .header(AUTHORIZATION, &bearer)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "host {host}");
    }

    // 403: bad Host — even on healthz and even with a valid token.
    for host in [
        "evil.example".to_string(),
        format!("evil.example:{port}"),
        "127.0.0.1:1".to_string(),
        "localhost.evil.example".to_string(),
        format!("127.0.0.2:{port}"),
    ] {
        for path in ["/healthz", "/v1/projects"] {
            let r = raw
                .get(format!("{base}{path}"))
                .header("host", &host)
                .header(AUTHORIZATION, &bearer)
                .send()
                .await
                .unwrap();
            let body = problem(r, StatusCode::FORBIDDEN, "forbidden").await;
            assert!(
                body["detail"].as_str().unwrap().contains("host"),
                "{host} {path}: {body}"
            );
        }
    }

    // 403: any Origin but exactly `null` — even with a valid token.
    for origin in [
        "http://evil.example".to_string(),
        format!("http://127.0.0.1:{port}"),
        "http://localhost".to_string(),
        "NULL".to_string(),
    ] {
        for (path, post) in [
            ("/healthz", false),
            ("/v1/projects", false),
            ("/v1/dreams/drm_x/apply", true),
        ] {
            let url = format!("{base}{path}");
            let b = if post { raw.post(&url) } else { raw.get(&url) };
            let r = b
                .header("origin", &origin)
                .header(AUTHORIZATION, &bearer)
                .send()
                .await
                .unwrap();
            let body = problem(r, StatusCode::FORBIDDEN, "forbidden").await;
            assert!(
                body["detail"].as_str().unwrap().contains("origin"),
                "{origin} {path}: {body}"
            );
        }
    }
    // Origin: null passes (to the route's own 404).
    let r = raw
        .post(format!("{base}/v1/dreams/drm_x/apply"))
        .header("origin", "null")
        .header(AUTHORIZATION, &bearer)
        .send()
        .await
        .unwrap();
    problem(r, StatusCode::NOT_FOUND, "not found").await;

    // 415: POST /v1/dreams without Content-Type, or with a non-JSON one.
    let body = r#"{"project":"-no-such-project"}"#;
    let r = raw
        .post(format!("{base}/v1/dreams"))
        .header(AUTHORIZATION, &bearer)
        .body(body)
        .send()
        .await
        .unwrap();
    problem(
        r,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported media type",
    )
    .await;
    for ct in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
        "application/jsonx",
    ] {
        let r = raw
            .post(format!("{base}/v1/dreams"))
            .header(AUTHORIZATION, &bearer)
            .header("content-type", ct)
            .body(body)
            .send()
            .await
            .unwrap();
        problem(
            r,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported media type",
        )
        .await;
        // Bodyless POSTs reject a non-JSON header too.
        let r = raw
            .post(format!("{base}/v1/dreams/drm_x/cancel"))
            .header(AUTHORIZATION, &bearer)
            .header("content-type", ct)
            .send()
            .await
            .unwrap();
        problem(
            r,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported media type",
        )
        .await;
    }
    // JSON with parameters is accepted (route's own 404).
    let r = raw
        .post(format!("{base}/v1/dreams"))
        .header(AUTHORIZATION, &bearer)
        .header("content-type", "Application/JSON; charset=utf-8")
        .body(body)
        .send()
        .await
        .unwrap();
    problem(r, StatusCode::NOT_FOUND, "not found").await;
    // Bodyless POST with JSON header is accepted.
    let r = raw
        .post(format!("{base}/v1/dreams/drm_x/rollback"))
        .header(AUTHORIZATION, &bearer)
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    problem(r, StatusCode::NOT_FOUND, "not found").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_end_to_end() {
    let f = common::fixture();
    // Same env as `common::cmd`, applied to this process for `config::load`.
    std::env::set_var("DREAMD_CONFIG", f.config_file());
    std::env::set_var("DREAMD_CONFIG_DIRS", &f.config_dir);
    std::env::set_var("DREAMD_DATA_DIR", &f.data_dir);
    std::env::set_var(
        "DREAMD_CLAUDE_BIN",
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/stub-claude.sh"),
    );
    std::env::set_var("DREAMD_NO_NOTIFY", "1");
    std::env::set_var("DREAMD_STUB_MODE", "auto");
    let ctx = Ctx::from_env().unwrap();
    let base = start(ctx.clone()).await;
    let token = std::fs::read_to_string(f.data_dir.join("api-token")).unwrap();
    let token = token.trim().to_string();
    assert_eq!(token.len(), 64);
    guards(&base, &token).await;

    let mut auth = HeaderMap::new();
    auth.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    let c = Client::builder().default_headers(auth).build().unwrap();

    // healthz
    let r = c.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"ok": true}));

    // projects == scan --json
    let r = c.get(format!("{base}/v1/projects")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(content_type(&r).starts_with("application/json"));
    let projects: Value = r.json().await.unwrap();
    let scan = common::cmd(&f)
        .args(["scan", "--json"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let scan: Value = serde_json::from_slice(&scan).unwrap();
    assert_eq!(projects, scan);
    assert_eq!(projects.as_array().unwrap().len(), 1);
    assert_eq!(projects[0]["slug"], common::SLUG);

    // 404s
    let r = c
        .get(format!("{base}/v1/dreams/drm_nope"))
        .send()
        .await
        .unwrap();
    problem(r, StatusCode::NOT_FOUND, "not found").await;
    for path in ["diff", "sessions"] {
        let r = c
            .get(format!("{base}/v1/dreams/drm_nope/{path}"))
            .send()
            .await
            .unwrap();
        problem(r, StatusCode::NOT_FOUND, "not found").await;
    }
    for action in ["apply", "rollback", "cancel"] {
        let r = c
            .post(format!("{base}/v1/dreams/drm_nope/{action}"))
            .send()
            .await
            .unwrap();
        problem(r, StatusCode::NOT_FOUND, "not found").await;
    }
    let r = c
        .post(format!("{base}/v1/dreams"))
        .json(&json!({"project": "-no-such-project", "force": true}))
        .send()
        .await
        .unwrap();
    let body = problem(r, StatusCode::NOT_FOUND, "not found").await;
    assert!(body["detail"]
        .as_str()
        .unwrap()
        .contains("-no-such-project"));

    // Unknown status filter.
    let r = c
        .get(format!("{base}/v1/dreams?status=bogus"))
        .send()
        .await
        .unwrap();
    problem(r, StatusCode::BAD_REQUEST, "bad request").await;

    // Cancel: a dream on the slow model is canceled through the DB only
    // and records no pid (the server must never be signalled).
    let slow = Ctx {
        claude: Arc::new(SlowClaude),
        ..ctx.clone()
    };
    let slow_base = start(slow).await;
    let r = c
        .post(format!("{slow_base}/v1/dreams"))
        .json(&json!({"project": common::SLUG, "force": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let d: Value = r.json().await.unwrap();
    let slow_id = d["id"].as_str().unwrap().to_string();
    // Wait for the pipeline to reach the workers.
    let start_t = Instant::now();
    loop {
        let d = get_dream(&c, &base, &slow_id).await;
        if d["status"] == "running" {
            assert!(d["pid"].is_null(), "server dream recorded a pid: {d}");
            break;
        }
        assert!(start_t.elapsed() < Duration::from_secs(10), "{d}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A second dream of the same project while this one runs → 409 busy.
    let r = c
        .post(format!("{base}/v1/dreams"))
        .json(&json!({"project": common::SLUG, "force": true}))
        .send()
        .await
        .unwrap();
    let body = problem(r, StatusCode::CONFLICT, "busy").await;
    assert_eq!(
        body["detail"],
        format!(
            "project is already being dreamed by pid {}",
            std::process::id()
        )
    );
    let r = c
        .post(format!("{base}/v1/dreams/{slow_id}/cancel"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d: Value = r.json().await.unwrap();
    assert_eq!(d["status"], "canceled");
    // The background task observes the cancel and leaves it canceled.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let d = get_dream(&c, &base, &slow_id).await;
    assert_eq!(d["status"], "canceled", "{d}");
    let r = c
        .post(format!("{base}/v1/dreams/{slow_id}/cancel"))
        .send()
        .await
        .unwrap();
    let body = problem(r, StatusCode::CONFLICT, "refused").await;
    assert!(body["detail"]
        .as_str()
        .unwrap()
        .contains("dream is canceled"));

    // The canceled dream's task releases the project lock when it returns.
    let start_t = Instant::now();
    let locks = f.data_dir.join("locks");
    while std::fs::read_dir(&locks).is_ok_and(|mut d| d.next().is_some()) {
        assert!(
            start_t.elapsed() < Duration::from_secs(10),
            "lock not released"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Create → 202 → completed.
    let r = c
        .post(format!("{base}/v1/dreams"))
        .json(&json!({"project": common::SLUG, "force": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let d: Value = r.json().await.unwrap();
    let id = d["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("drm_"), "{d}");
    assert!(d["status"] == "pending" || d["status"] == "running", "{d}");
    let d = wait_terminal(&c, &base, &id).await;
    assert_eq!(d["status"], "completed", "{d}");

    // diff
    let r = c
        .get(format!("{base}/v1/dreams/{id}/diff"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(content_type(&r).starts_with("text/plain"));
    let diff = r.text().await.unwrap();
    assert!(diff.starts_with("--- a/"), "{diff}");

    // sessions
    let r = c
        .get(format!("{base}/v1/dreams/{id}/sessions"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let sessions: Value = r.json().await.unwrap();
    let sessions = sessions.as_array().unwrap();
    assert_eq!(sessions.len(), 3);
    assert!(sessions.iter().all(|s| s["status"] == "ok"), "{sessions:?}");

    // list filter
    let list: Value = c
        .get(format!("{base}/v1/dreams?status=completed"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![id.as_str()]);
    let all: Value = c
        .get(format!("{base}/v1/dreams"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all.as_array().unwrap().len(), 2);

    // cancel a terminal dream → 409
    let r = c
        .post(format!("{base}/v1/dreams/{id}/cancel"))
        .send()
        .await
        .unwrap();
    problem(r, StatusCode::CONFLICT, "refused").await;

    // apply → applied; again → 409; rollback → rolled_back
    let r = c
        .post(format!("{base}/v1/dreams/{id}/apply"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d: Value = r.json().await.unwrap();
    assert_eq!(d["status"], "applied", "{d}");
    assert!(f.memory_dir.join("dreamed.md").is_file());
    let r = c
        .post(format!("{base}/v1/dreams/{id}/apply"))
        .send()
        .await
        .unwrap();
    let body = problem(r, StatusCode::CONFLICT, "refused").await;
    assert_eq!(body["detail"], "refused: dream is applied");
    let r = c
        .post(format!("{base}/v1/dreams/{id}/rollback"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d: Value = r.json().await.unwrap();
    assert_eq!(d["status"], "rolled_back", "{d}");
    assert!(!f.memory_dir.join("dreamed.md").exists());

    // Nothing new since the dream: an unforced create is gate-skipped.
    let r = c
        .post(format!("{base}/v1/dreams"))
        .json(&json!({"project": common::SLUG}))
        .send()
        .await
        .unwrap();
    problem(r, StatusCode::CONFLICT, "skipped").await;
}
