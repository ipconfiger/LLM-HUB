//! HTTP server: the axum router, request handlers, and the `serve` entry point.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::config;
use crate::error;
use crate::worker::{self, ProxyEvent, ProxyJob};

/// Maximum request body size accepted by the proxy handler (16 MiB).
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Buffer size for the per-request response channel (events streamed from the
/// worker back to the handler).
const RESPONSE_CHANNEL_BUFFER: usize = 128;

/// Shared application state threaded through every handler via
/// [`axum::extract::State`].
#[derive(Clone)]
pub struct AppState {
    /// Sender half of the proxy job queue consumed by the worker. The worker
    /// itself owns the shared reqwest client used to talk to upstream backends.
    pub worker_tx: mpsc::Sender<ProxyJob>,
    /// Loaded configuration, shared with the worker.
    pub config: Arc<config::Config>,
}

/// Start the llm-hub HTTP server bound to `addr`.
///
/// Loads configuration, builds a shared reqwest client (rustls, 300s timeout)
/// and the proxy worker, then serves the axum router until shutdown.
pub async fn serve(addr: std::net::SocketAddr) -> error::Result<()> {
    let config = config::Config::load()?;
    let config = Arc::new(config);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let (worker_tx, worker_rx) = mpsc::channel::<ProxyJob>(128);
    let _worker = worker::spawn(client.clone(), config.clone(), worker_rx);

    let state = AppState {
        worker_tx,
        config,
    };

    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("llm-hub listening on http://{}/v1", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the application router with all routes wired to `state`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/{*path}", post(proxy))
        .route("/v1/models", get(models))
        .route("/", get(health))
        .route("/health", get(health))
        .with_state(state)
}

/// Main proxy handler (`POST /v1/{*path}`).
///
/// Reads the request body, extracts the `model` field, enqueues a [`ProxyJob`],
/// then translates the worker's first [`ProxyEvent`] into the HTTP response
/// (streaming the remainder of the body back to the client).
async fn proxy(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_owned();

    let body = match to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => {
            return bad_request(format!("failed to read request body: {e}"));
        }
    };

    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return bad_request(format!("invalid JSON body: {e}"));
        }
    };

    let model = match value.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_owned(),
        None => {
            return bad_request("missing 'model' field".to_string());
        }
    };

    let (response_tx, response_rx) = mpsc::channel::<ProxyEvent>(RESPONSE_CHANNEL_BUFFER);

    let job = ProxyJob {
        path,
        body,
        model,
        response_tx,
    };

    if state.worker_tx.send(job).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "proxy worker is not available" })),
        )
            .into_response();
    }

    translate_response(response_rx).await
}

/// Translate the worker's event stream into an HTTP response.
async fn translate_response(mut response_rx: mpsc::Receiver<ProxyEvent>) -> Response {
    match response_rx.recv().await {
        Some(ProxyEvent::Respond {
            status,
            content_type,
        }) => {
            // Hand the remainder of the channel off to a byte stream.
            let stream = ReceiverStream::new(response_rx).filter_map(|ev| async move {
                match ev {
                    ProxyEvent::Chunk(b) => Some(Ok::<bytes::Bytes, std::io::Error>(b)),
                    // `Failed` / further `Respond` are not expected after the
                    // first respond; ignore them gracefully.
                    _ => None,
                }
            });
            match Response::builder()
                .status(status_from_u16(status))
                .header("content-type", content_type)
                .body(Body::from_stream(stream))
            {
                Ok(resp) => resp,
                Err(e) => internal_error(format!("failed to build response: {e}")),
            }
        }
        Some(ProxyEvent::Failed(msg)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": msg })),
        )
            .into_response(),
        // A chunk before the initial `Respond` violates the protocol; treat it
        // as a worker error.
        Some(ProxyEvent::Chunk(_)) | None => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "proxy worker closed unexpectedly" })),
        )
            .into_response(),
    }
}

/// `GET /v1/models` — the union of all configured models in OpenAI-style JSON.
async fn models(State(state): State<AppState>) -> impl IntoResponse {
    let mut seen = std::collections::HashSet::new();
    let mut data = Vec::new();
    for backend in &state.config.backends {
        for model in &backend.models {
            if seen.insert(model.clone()) {
                data.push(serde_json::json!({
                    "id": model,
                    "object": "model",
                    "owned_by": backend.name,
                }));
            }
        }
    }
    Json(serde_json::json!({ "object": "list", "data": data }))
}

/// `GET /` and `GET /health` — liveness probe.
async fn health() -> &'static str {
    "llm-hub ok"
}

/// Build a `400 Bad Request` JSON response carrying `msg`.
fn bad_request(msg: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

/// Build a `500 Internal Server Error` JSON response carrying `msg`.
fn internal_error(msg: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

/// Convert a raw status code to [`StatusCode`], falling back to 502 on failure.
fn status_from_u16(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_state() -> AppState {
        let (tx, _rx) = mpsc::channel::<ProxyJob>(8);
        let config = Arc::new(config::Config::default());
        AppState {
            worker_tx: tx,
            config,
        }
    }

    #[test]
    fn router_builds_without_panic() {
        // Guards against overlapping static (`/v1/models`) + catch-all
        // (`/v1/{*path}`) route conflicts at registration time.
        let _router = build_router(dummy_state());
    }

    #[test]
    fn status_from_u16_maps_known_and_falls_back() {
        assert_eq!(status_from_u16(200), StatusCode::OK);
        assert_eq!(status_from_u16(404), StatusCode::NOT_FOUND);
        // Any 3-digit code is accepted by the http crate.
        assert_eq!(status_from_u16(999).as_u16(), 999);
        // Out-of-range codes fall back to 502 Bad Gateway.
        assert_eq!(status_from_u16(99), StatusCode::BAD_GATEWAY);
        assert_eq!(status_from_u16(1000), StatusCode::BAD_GATEWAY);
    }
}
