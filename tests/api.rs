//! `dreamd serve`: Task 10 acceptance criteria, in process. The router runs
//! on an ephemeral loopback listener with the fixture `Ctx` (stub claude in
//! `auto` mode, notifications off); requests go through `reqwest`.
//!
//! Everything is one test because the fixture is wired through process env
//! (`config::load` reads `DREAMD_*`), which tests in one binary would share.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dreamd::api::{router, AppState};
use dreamd::claude::{Claude, ClaudeError, ClaudeRequest, ClaudeResult};
use dreamd::dream::Ctx;
use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};

/// Start `router` on 127.0.0.1:0; return the base URL.
async fn start(ctx: Ctx) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(AppState(Arc::new(ctx)));
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
    let c = Client::new();

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
