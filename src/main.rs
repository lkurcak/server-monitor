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
    "server-monitor 0.3.0"
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
            }

            let last = monitor.history.last().unwrap();
            let (new_in_alert, notify_status) = evaluate_status_transition(
                monitor.in_alert,
                last.status,
                last.hits,
                &monitor.settings,
                monitor.history.len() == 1,
            );
            monitor.in_alert = new_in_alert;

            if let Some(status_to_notify) = notify_status {
                for discord_webhook in &monitor.discord_webhook {
                    send_discord_webhook_message(
                        discord_webhook,
                        &format!("{endpoint} {status_to_notify}"),
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
        settings: MonitorSettings {
            fail_hits: payload.fail_hits.unwrap_or(1),
            success_hits: payload.success_hits.unwrap_or(1),
        },
        in_alert: false,
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

#[derive(Serialize, PartialEq, Eq, Clone, Copy, Debug)]
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

/// Evaluate the current streak and alert state to decide whether to notify.
/// Returns (new_in_alert, optional_status_to_notify)
fn evaluate_status_transition(
    in_alert: bool,
    status: Status,
    hits: u64,
    settings: &MonitorSettings,
    is_first_segment: bool,
) -> (bool, Option<Status>) {
    match status {
        Status::Up => {
            // Send OK either when recovering from alert or on the very first segment
            if (in_alert || is_first_segment) && hits == settings.success_hits {
                (false, Some(Status::Up))
            } else {
                (in_alert, None)
            }
        }
        Status::Down | Status::Failing => {
            if hits == settings.fail_hits {
                (true, Some(status))
            } else {
                (in_alert, None)
            }
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

struct MonitorSettings {
    // How many times in a row a check must fail to send a warning
    fail_hits: u64,
    // How many times in a row a check must success to send a message
    success_hits: u64,
}

struct Monitor {
    discord_webhook: Vec<String>,
    cancellation_token: CancellationToken,
    history: Vec<HistoryLog>,
    settings: MonitorSettings,
    // Whether we've sent a failure alert and are waiting to send a recovery OK
    in_alert: bool,
}

#[derive(Deserialize)]
struct PostMonitorRequest {
    endpoint: String,
    discord_webhook: Vec<String>,
    fail_hits: Option<u64>,
    success_hits: Option<u64>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run_sequence(sequence: &[Status], fail_hits: u64, success_hits: u64) -> Vec<Status> {
        let settings = MonitorSettings {
            fail_hits,
            success_hits,
        };
        let mut in_alert = false;
        let mut result = Vec::new();
        let mut last_status: Option<Status> = None;
        let mut hits: u64 = 0;
        let mut in_first_segment: bool = true;

        for &s in sequence {
            if Some(s) == last_status {
                hits += 1;
            } else {
                // If we've seen a status before and it changes now, we exit the first segment
                if last_status.is_some() {
                    in_first_segment = false;
                }
                last_status = Some(s);
                hits = 1;
            }
            let (new_alert, notify) =
                evaluate_status_transition(in_alert, s, hits, &settings, in_first_segment);
            in_alert = new_alert;
            if let Some(n) = notify {
                result.push(n);
            }
        }
        result
    }

    #[test]
    fn ok_only_after_bad_alerted() {
        // fail after 2, recover after 3 OK
        let seq = [
            Status::Up,
            Status::Up,
            Status::Up,      // first OK will be sent after threshold
            Status::Failing, // 1/2 fail
            Status::Failing, // 2/2 fail -> BAD
            Status::Up,      // 1/3 ok
            Status::Up,      // 2/3 ok
            Status::Up,      // 3/3 ok -> OK
            Status::Up,      // continue ok, no more OKs
        ];
        let notifications = run_sequence(&seq, 2, 3);
        assert_eq!(notifications, vec![Status::Up, Status::Failing, Status::Up]);
    }

    #[test]
    fn repeated_oks_without_prior_bad_do_not_notify() {
        let seq = [Status::Up, Status::Up, Status::Up, Status::Up, Status::Up];
        let notifications = run_sequence(&seq, 2, 2);
        // First OK after reaching success threshold should be sent once
        assert_eq!(notifications, vec![Status::Up]);
    }

    #[test]
    fn down_threshold_triggers_once() {
        let seq = [Status::Down, Status::Down, Status::Down];
        let notifications = run_sequence(&seq, 2, 1);
        assert_eq!(notifications, vec![Status::Down]);
    }

    #[test]
    fn alternating_failures_below_threshold_do_not_trigger() {
        let seq = [
            Status::Failing,
            Status::Up,
            Status::Failing,
            Status::Up,
            Status::Failing,
            Status::Up,
        ];
        let notifications = run_sequence(&seq, 2, 1);
        assert!(notifications.is_empty());
    }
}
