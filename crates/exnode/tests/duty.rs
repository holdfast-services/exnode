//! The agent's duty, against a mock hub: what it tells the hub in its heartbeats, and that it stops
//! and starts asking for jobs as the schedule and a pause say.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::{get, post}, Json, Router};
use serde_json::{json, Value};

use ex_common::identity::{generate_keypair, load_signing_key, node_id_for};
use exnode::{
    agent::Agent,
    config::{Blackout, ModelConfig, NodeConfig, ScheduleRule},
    duty::Control,
    hours,
};

#[derive(Default)]
struct Seen {
    heartbeats: Mutex<Vec<Value>>,
    polls:      AtomicUsize,
}

async fn mock_hub(seen: Arc<Seen>) -> SocketAddr {
    async fn register() -> Json<Value> { Json(json!({"status": "active", "tier": "community"})) }
    async fn heartbeat(State(s): State<Arc<Seen>>, Json(body): Json<Value>) -> Json<Value> {
        s.heartbeats.lock().unwrap().push(body);
        Json(json!({"status": "active"}))
    }
    async fn next(State(s): State<Arc<Seen>>) -> impl IntoResponse {
        s.polls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        StatusCode::NO_CONTENT
    }
    let app = Router::new()
        .route("/v0/nodes/register", post(register))
        .route("/v0/nodes/{id}/heartbeat", post(heartbeat))
        .route("/v0/nodes/{id}/jobs/next", get(next))
        .route("/v0/market", get(|| async { Json(json!({"models": []})) }))
        .with_state(seen);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn hhmm(m: u16) -> String { format!("{:02}:{:02}", (m / 60) % 24, m % 60) }

fn dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("exnode-duty-it-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn config(hub: SocketAddr, key_file: String, max: usize) -> NodeConfig {
    NodeConfig {
        hub_url: format!("http://{hub}"),
        key_file,
        enroll_token: None,
        backend: "openai".into(),
        backend_url: Some("http://127.0.0.1:9".into()),
        ollama_url: String::new(),
        backend_api_key_env: None,
        models: vec![ModelConfig { name: "m".into(), hash: format!("sha256:{}", "a".repeat(64)), context: 4096, tools: false }],
        heartbeat_secs: 1,
        poll_secs: 1,
        max_concurrency: max,
        serve_hours: None,
        timezone: None,
        schedule: Vec::new(),
        blackout: Blackout::default(),
    }
}

async fn start(cfg: NodeConfig) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    cfg.validate().unwrap();
    let (sk_b64, pk_b64) = generate_keypair();
    let sk = load_signing_key(&sk_b64).unwrap();
    let agent = Agent::new(cfg, node_id_for(&pk_b64).unwrap(), sk).await.unwrap().with_duty_tick(Duration::from_millis(200));
    tokio::spawn(agent.run(pk_b64))
}

fn last_availability(seen: &Seen) -> Value {
    seen.heartbeats.lock().unwrap().last().expect("a heartbeat")["availability"].clone()
}

#[tokio::test]
async fn off_duty_by_the_schedule_the_node_says_so_and_takes_no_jobs() {
    let seen = Arc::new(Seen::default());
    let hub = mock_hub(seen.clone()).await;
    let now = hours::local_minute();
    let mut cfg = config(hub, dir().join("node.key").to_str().unwrap().into(), 4);
    // a window that opens in two hours
    cfg.schedule = vec![ScheduleRule { days: vec![], hours: format!("{}-{}", hhmm(now + 120), hhmm(now + 180)), share: 100 }];
    let task = start(cfg).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    task.abort();
    let a = last_availability(&seen);
    assert_eq!((a["state"].clone(), a["serving"].clone(), a["capacity"].clone(), a["of"].clone()), (json!("scheduled"), json!(false), json!(0), json!(4)));
    let next = a["next_change_at"].as_u64().expect("it says when the window opens");
    let in_secs = next.saturating_sub(ex_common::identity::unix_now());
    assert!((100 * 60..=121 * 60).contains(&in_secs), "the window opens in about two hours: {in_secs}s");
    assert_eq!(seen.polls.load(Ordering::SeqCst), 0, "no job polls while off duty");
    assert!(seen.heartbeats.lock().unwrap().len() >= 2, "heartbeats go on");
}

#[tokio::test]
async fn in_a_window_the_node_says_how_much_of_the_machine_it_brings() {
    let seen = Arc::new(Seen::default());
    let hub = mock_hub(seen.clone()).await;
    let mut cfg = config(hub, dir().join("node.key").to_str().unwrap().into(), 4);
    cfg.schedule = vec![ScheduleRule { days: vec![], hours: "all day".into(), share: 50 }];
    let task = start(cfg).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    task.abort();
    let a = last_availability(&seen);
    assert_eq!((a["state"].clone(), a["serving"].clone(), a["capacity"].clone(), a["of"].clone()), (json!("serving"), json!(true), json!(2), json!(4)), "half of 4");
    assert!(seen.polls.load(Ordering::SeqCst) >= 2, "it asks for jobs");
}

#[tokio::test]
async fn a_blackout_date_is_off_and_says_so() {
    let seen = Arc::new(Seen::default());
    let hub = mock_hub(seen.clone()).await;
    let mut cfg = config(hub, dir().join("node.key").to_str().unwrap().into(), 2);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    cfg.blackout = Blackout { dates: vec![today] };
    let task = start(cfg).await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    task.abort();
    assert_eq!(last_availability(&seen)["state"], "blackout");
    assert_eq!(seen.polls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_pause_stops_the_node_and_resuming_starts_it_again_without_a_restart() {
    let seen = Arc::new(Seen::default());
    let hub = mock_hub(seen.clone()).await;
    let d = dir();
    let key_file: String = d.join("node.key").to_str().unwrap().into();
    let path = Control::path_for(&key_file);
    Control { paused: true, until: None }.save(&path).unwrap();
    let task = start(config(hub, key_file, 2)).await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(last_availability(&seen)["state"], "paused");
    assert_eq!(seen.polls.load(Ordering::SeqCst), 0, "paused: no job polls");
    // `exnode resume` writes the file; the running agent notices
    Control::default().save(&path).unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(last_availability(&seen)["state"], "serving");
    assert!(seen.polls.load(Ordering::SeqCst) >= 2, "resumed: it asks for jobs again");
    // and a pause set while it runs takes hold too
    Control { paused: true, until: None }.save(&path).unwrap();
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert_eq!(last_availability(&seen)["state"], "paused");
    let polls = seen.polls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(seen.polls.load(Ordering::SeqCst) <= polls + 1, "paused again: at most the poll already under way");
    task.abort();
    std::fs::remove_dir_all(d).unwrap();
}

#[tokio::test]
async fn serve_hours_and_a_schedule_together_are_refused_with_a_reason() {
    let hub: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let mut cfg = config(hub, "node.key".into(), 1);
    cfg.serve_hours = Some("18:00-08:00".into());
    cfg.schedule = vec![ScheduleRule { days: vec![], hours: "all day".into(), share: 100 }];
    let e = cfg.validate().unwrap_err().to_string();
    assert!(e.contains("not both"), "{e}");
    // and every part of a schedule is checked with a message that says which rule
    cfg.serve_hours = None;
    cfg.schedule = vec![ScheduleRule { days: vec!["someday".into()], hours: "all day".into(), share: 100 }];
    assert!(cfg.validate().unwrap_err().to_string().contains("rule 1"));
    cfg.schedule = vec![ScheduleRule { days: vec![], hours: "6pm-8am".into(), share: 100 }];
    assert!(cfg.validate().is_err());
    cfg.schedule = vec![ScheduleRule { days: vec![], hours: "all day".into(), share: 150 }];
    assert!(cfg.validate().is_err());
    cfg.schedule = vec![];
    cfg.timezone = Some("Mars/Olympus".into());
    assert!(cfg.validate().unwrap_err().to_string().contains("America/Chicago"));
    cfg.timezone = Some("America/Chicago".into());
    cfg.blackout = Blackout { dates: vec!["tomorrow".into()] };
    assert!(cfg.validate().is_err());
}
