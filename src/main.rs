use std::{
    collections::HashMap,
    fmt::Display,
    sync::{Arc, LazyLock},
};

use axum::{
    Json, Router,
    extract::Query,
    http::StatusCode,
    routing::{get, post},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::{sync::Mutex, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route("/", get(root))
        .route("/monitor", post(post_monitor))
        .route("/monitor", get(get_monitor));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn root() -> &'static str {
    "server-monitor v0.1.0"
}

static MONITORS: LazyLock<Arc<Mutex<HashMap<String, Monitor>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

async fn monitor_server(endpoint: String) {
    let client = Client::new();
    loop {
        {
            let mut mutex_guard = MONITORS.lock().await;
            let monitor = mutex_guard.get_mut(&endpoint).unwrap();
            // let endpoint = &monitor.endpoint;

            let current_status = match client.get(&endpoint).send().await {
                Ok(ok) => {
                    if ok.status().is_success() {
                        tracing::info!("{} is up (returned {})", endpoint, ok.status());

                        Status::Up
                    } else {
                        tracing::warn!("{} is failing (returned {})", endpoint, ok.status());

                        Status::Failing
                    }
                }
                Err(err) => {
                    tracing::error!("{} is down: {}", endpoint, err);

                    Status::Down
                }
            };

            let mut done = false;

            if let Some(last) = monitor.history.last_mut() {
                if last.status == current_status {
                    last.hits += 1;
                    last.timestamp_end = jiff::Timestamp::now().to_string();
                    done = true;
                }
            }

            if !done {
                monitor.history.push(HistoryLog {
                    status: current_status,
                    timestamp_start: jiff::Timestamp::now().to_string(),
                    timestamp_end: jiff::Timestamp::now().to_string(),
                    duration: Duration::default(),
                    hits: 1,
                });

                if let Some(discord_webhook) = &monitor.discord_webhook {
                    send_discord_webhook_message(
                        discord_webhook,
                        &format!("{endpoint} {current_status}"),
                    )
                    .await;
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

async fn send_discord_webhook_message(webhook_url: &str, message: &str) {
    let message = format!(r#"{{"content":"{message}"}}"#);

    let client = Client::new();
    let Ok(response) = client
        .post(webhook_url)
        .header("Content-Type", "application/json")
        .body(message)
        .send()
        .await
    else {
        tracing::error!("Failed to send message");
        return;
    };

    if response.status().is_success() {
        tracing::debug!("Message sent successfully!");
    } else {
        tracing::error!("Failed to send message: {:?}", response.status());
    }
}

async fn post_monitor(Json(payload): Json<PostMonitorRequest>) -> StatusCode {
    let monitor = Monitor {
        // endpoint: payload.endpoint.clone(),
        discord_webhook: payload.discord_webhook,
        cancellation_token: CancellationToken::new(),
        history: vec![],
    };
    let cloned_token = monitor.cancellation_token.clone();

    let mut table = MONITORS.lock().await;

    if table.contains_key(&payload.endpoint) {
        return StatusCode::CONFLICT;
    }

    table.insert(payload.endpoint.clone(), monitor);

    tokio::spawn(async move {
        tokio::select! {
            _ = cloned_token.cancelled() => {}
            _ = monitor_server(payload.endpoint) => {}
        }
    });

    StatusCode::CREATED
}

async fn get_monitor(
    Query(request): Query<GetMonitorRequest>,
) -> (StatusCode, Json<Option<GetMonitorResponse>>) {
    if let Some(monitor) = MONITORS.lock().await.get(&request.endpoint) {
        let resp = GetMonitorResponse {
            status: monitor
                .history
                .last()
                .map(|x| x.status)
                .unwrap_or(Status::Down),
            history: monitor.history.clone(),
        };

        (StatusCode::OK, Json(Some(resp)))
    } else {
        (StatusCode::NOT_FOUND, Json(None))
    }
}

#[derive(Serialize, PartialEq, Eq, Clone, Copy)]
enum Status {
    Up,
    Down,
    Failing,
}

impl Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Up => write!(f, "Up"),
            Status::Down => write!(f, "Down"),
            Status::Failing => write!(f, "Failing"),
        }
    }
}

#[derive(Serialize, Clone)]
struct HistoryLog {
    status: Status,
    timestamp_start: String,
    timestamp_end: String,
    duration: Duration,
    hits: u64,
}

struct Monitor {
    // endpoint: String,
    discord_webhook: Option<String>,
    cancellation_token: CancellationToken,
    history: Vec<HistoryLog>,
}

#[derive(Deserialize)]
struct PostMonitorRequest {
    endpoint: String,
    discord_webhook: Option<String>,
}

#[derive(Deserialize)]
struct GetMonitorRequest {
    endpoint: String,
}

#[derive(Serialize)]
struct GetMonitorResponse {
    status: Status,
    history: Vec<HistoryLog>,
}
