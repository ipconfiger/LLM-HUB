//! Core proxy logic: forward a request to the first backend that succeeds,
//! streaming the response back to the caller, with ordered failover.
//!
//! The failover rule is strict: a backend is only retried past if it failed to
//! connect or returned a non-2xx status. Once [`ProxyEvent::Respond`] has been
//! emitted the handler is committed to that backend and the upstream body is
//! streamed until it ends or errors.

use crate::config::Config;
use crate::worker::{ProxyEvent, ProxyJob};
use futures_util::StreamExt;

/// Default `Content-Type` when the upstream omits the header.
const DEFAULT_CONTENT_TYPE: &str = "text/event-stream";

/// Run a single proxy job to completion.
///
/// Resolves the job's model to an ordered list of backends via
/// [`Config::resolve`] and tries each in turn. On a 2xx response the upstream
/// body is streamed back over `job.response_tx`; on a connect error or
/// non-2xx status the next backend is attempted. If no backend succeeds a
/// single [`ProxyEvent::Failed`] is emitted, joining all per-backend reasons.
pub async fn run(job: ProxyJob, client: &reqwest::Client, config: &Config) {
    let resolved = config.resolve(&job.model);
    tracing::info!(
        model = %job.model,
        backends = resolved.len(),
        "resolved backends for model"
    );

    if resolved.is_empty() {
        let msg = format!("no backend available for model '{}'", job.model);
        tracing::warn!(model = %job.model, "no backend resolved for model");
        let _ = job.response_tx.send(ProxyEvent::Failed(msg)).await;
        return;
    }

    // Per-backend failure reasons, accumulated for the final error detail.
    let mut reasons: Vec<String> = Vec::new();

    for backend in &resolved {
        let url = format!("{}{}", backend.base_url, job.path);
        tracing::debug!(
            backend = %backend.backend_name,
            url = %url,
            "attempting backend"
        );

        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", backend.key))
            .header("Content-Type", "application/json")
            .body(job.body.clone())
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let reason =
                    format!("{}: connect/send error: {}", backend.backend_name, e);
                tracing::warn!(
                    backend = %backend.backend_name,
                    error = %e,
                    "backend request failed; will try next"
                );
                reasons.push(reason);
                continue;
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let reason = format!("{}: HTTP {}", backend.backend_name, status.as_u16());
            tracing::warn!(
                backend = %backend.backend_name,
                status = status.as_u16(),
                "backend returned non-success status; failing over"
            );
            reasons.push(reason);
            continue;
        }

        // Success: we are now committed to this backend. Stream the body.
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(DEFAULT_CONTENT_TYPE)
            .to_owned();

        if job
            .response_tx
            .send(ProxyEvent::Respond {
                status: status.as_u16(),
                content_type,
            })
            .await
            .is_err()
        {
            // The client (handler) went away before we could respond; stop.
            tracing::debug!("client disconnected before respond; aborting job");
            return;
        }

        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if job.response_tx.send(ProxyEvent::Chunk(bytes)).await.is_err() {
                        tracing::debug!("client disconnected mid-stream; aborting job");
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "upstream stream error; ending stream");
                    break;
                }
            }
        }
        // Stream finished (cleanly or on error). We're done with this job.
        return;
    }

    // Loop exhausted without a successful response.
    let detail = reasons.join("; ");
    tracing::warn!(model = %job.model, "all configured backends failed: {}", detail);
    let _ = job
        .response_tx
        .send(ProxyEvent::Failed(format!(
            "all configured backends failed for model '{}': {}",
            job.model, detail
        )))
        .await;
}
