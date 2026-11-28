//! Serving hours: outside the window the agent asks the hub for no jobs but
//! keeps heartbeating; inside it, it polls as usual.  A mock hub counts both.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use ex_common::identity::{generate_keypair, load_signing_key, node_id_for};
use exnode::{
    agent::Agent,
    config::{ModelConfig, NodeConfig},
    hours,
};

#[derive(Default)]
struct Counts {
    heartbeats: AtomicUsize,
    polls:      AtomicUsize,
}

async fn mock_hub(counts: Arc<Counts>) -> SocketAddr {
    async fn register() -> Json<serde_json::Value> { Json(json!({"status": "active", "tier": "community"})) }
    async fn heartbeat(State(c): State<Arc<Counts>>) -> Json<serde_json::Value> {
        c.heartbeats.fetch_add(1, Ordering::SeqCst);
        Json(json!({"status": "active"}))
    }
    async fn next(State(c): State<Arc<Counts>>) -> impl IntoResponse {
        c.polls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await; // a short long-poll
        StatusCode::NO_CONTENT
    }
    let app = Router::new()
        .route("/v0/nodes/register", post(register))
        .route("/v0/nodes/{id}/heartbeat", post(heartbeat))
        .route("/v0/nodes/{id}/jobs/next", get(next))
        .route("/v0/market", get(|| async { Json(json!({"models": []})) }))
        .with_state(counts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn hhmm(m: u16) -> String { format!("{:02}:{:02}", (m / 60) % 24, m % 60) }

async fn run_for(serve_hours: String) -> Arc<Counts> {
    let counts = Arc::new(Counts::default());
    let hub = mock_hub(counts.clone()).await;
    let (sk_b64, pk_b64) = generate_keypair();
    let sk = load_signing_key(&sk_b64).unwrap();
    let cfg = NodeConfig {
        hub_url: format!("http://{hub}"),
        key_file: String::new(),
        enroll_token: None,
        backend: "openai".into(),
        backend_url: Some("http://127.0.0.1:9".into()),
        ollama_url: String::new(),
        backend_api_key_env: None,
        models: vec![ModelConfig { name: "m".into(), hash: format!("sha256:{}", "a".repeat(64)), context: 4096, tools: false }],
        heartbeat_secs: 1,
        poll_secs: 1,
        max_concurrency: 1,
        serve_hours: Some(serve_hours),
        timezone: None,
        schedule: Vec::new(),
        blackout: Default::default(),
    };
    cfg.validate().unwrap();
    let agent = Agent::new(cfg, node_id_for(&pk_b64).unwrap(), sk).await.unwrap();
    let task = tokio::spawn(agent.run(pk_b64));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    task.abort();
    counts
}

#[tokio::test]
async fn outside_its_hours_a_node_heartbeats_and_takes_no_jobs() {
    let now = hours::local_minute();
    // a window that opens two hours from now and closes an hour later
    let closed = format!("{}-{}", hhmm(now + 120), hhmm(now + 180));
    // a window around now: opened an hour ago, closes in two
    let open = format!("{}-{}", hhmm(now + 1440 - 60), hhmm(now + 120));

    let (c, o) = tokio::join!(run_for(closed.clone()), run_for(open.clone()));
    assert!(c.heartbeats.load(Ordering::SeqCst) >= 2, "heartbeats go on outside {closed}");
    assert_eq!(c.polls.load(Ordering::SeqCst), 0, "no job polls outside {closed}");
    assert!(o.heartbeats.load(Ordering::SeqCst) >= 2);
    assert!(o.polls.load(Ordering::SeqCst) >= 3, "polls inside {open}");
}
