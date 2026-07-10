//! Core proxy logic: forward a request to the first backend/key that succeeds,
//! streaming the response back to the caller, with error-classified failover.
//!
//! Failover policy:
//! - **Key-exhausted** (401/402/403/429) → advance to the next key of the *same*
//!   backend. When all keys of a backend are exhausted → next backend.
//! - **Backend-unreachable** (transport error or HTTP >= 500) → skip the
//!   remaining keys of that backend and jump to the next backend.
//! - Other 4xx (neither key nor backend problem) → propagate status+body to the
//!   agent verbatim; do not retry.
//! - 2xx → stream the body; once responding we are committed and never retry.
//!
//! Exhausted keys / unreachable backends are reported to the [`Resolver`] which
//! parks them with a cooldown so they recover automatically.

use crate::config::Resolver;
use crate::worker::{ProxyEvent, ProxyJob};
use futures_util::StreamExt;

/// Default `Content-Type` when the upstream omits the header on a 2xx stream.
const DEFAULT_CONTENT_TYPE: &str = "text/event-stream";

/// Pure classification of an HTTP status into a failover category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// 2xx — stream the upstream body.
    Success,
    /// 401/402/403/429 — the key looks spent; try the next key.
    KeyExhausted,
    /// Transport error or HTTP >= 500 — the backend is down; skip its keys.
    BackendUnreachable,
    /// Any other 4xx — the request itself is bad; propagate verbatim.
    ClientError,
}

/// Map a raw upstream HTTP status to its failover [`Class`].
fn classify_status(status: u16) -> Class {
    if status >= 500 {
        Class::BackendUnreachable
    } else if matches!(status, 401 | 402 | 403 | 429) {
        Class::KeyExhausted
    } else if (200..300).contains(&status) {
        Class::Success
    } else {
        Class::ClientError
    }
}

/// Outcome of a single upstream attempt.
enum Attempt {
    /// 2xx: stream this response back to the caller.
    Streaming(reqwest::Response),
    /// 401/402/403/429: this key is exhausted.
    KeyExhausted,
    /// Transport error or >= 500: the backend is unreachable.
    BackendUnreachable,
    /// Other 4xx: propagate `(status, body)` to the agent verbatim.
    ClientError(u16, String),
}

/// Perform one upstream attempt and classify its outcome.
async fn attempt(
    client: &reqwest::Client,
    url: &str,
    key: &str,
    body: bytes::Bytes,
) -> Attempt {
    let resp = match client
        .post(url)
        .header("Authorization", format!("Bearer {}", key))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "upstream transport error");
            return Attempt::BackendUnreachable;
        }
    };
    let status = resp.status().as_u16();
    match classify_status(status) {
        Class::Success => Attempt::Streaming(resp),
        Class::KeyExhausted => {
            tracing::warn!(status, "upstream key-exhausted status");
            Attempt::KeyExhausted
        }
        Class::BackendUnreachable => {
            tracing::warn!(status, "upstream >= 500; backend unreachable");
            Attempt::BackendUnreachable
        }
        Class::ClientError => {
            let body = resp.text().await.unwrap_or_default();
            Attempt::ClientError(status, body)
        }
    }
}

/// Run a single proxy job to completion.
///
/// Resolves the job's model to candidate backends via [`Resolver::resolve`],
/// then tries keys/backends per the failover policy. On a 2xx response the
/// upstream body is streamed back over `job.response_tx`. If no backend/key
/// succeeds a single [`ProxyEvent::Failed`] is emitted, joining all reasons.
pub async fn run(job: ProxyJob, client: &reqwest::Client, resolver: &Resolver) {
    let groups = resolver.resolve(&job.model);
    tracing::info!(
        model = %job.model,
        backends = groups.len(),
        "resolved backends for model"
    );

    if groups.is_empty() {
        let msg = format!("no backend available for model '{}'", job.model);
        tracing::warn!(model = %job.model, "no backend resolved for model");
        let _ = job.response_tx.send(ProxyEvent::Failed(msg)).await;
        return;
    }

    // Per-backend failure reasons, accumulated for the final error detail.
    let mut reasons: Vec<String> = Vec::new();

    for g in &groups {
        let url = format!("{}{}", g.base_url, job.path);
        tracing::debug!(
            backend = %g.backend_name,
            url = %url,
            "attempting backend"
        );

        let mut broke_unreachable = false;
        for k in &g.keys {
            match attempt(client, &url, &k.key, job.body.clone()).await {
                Attempt::Streaming(resp) => {
                    resolver.mark_key_ok(g.backend_index, k.key_index);
                    let status = resp.status().as_u16();
                    let content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or(DEFAULT_CONTENT_TYPE)
                        .to_owned();

                    if job
                        .response_tx
                        .send(ProxyEvent::Respond {
                            status,
                            content_type,
                        })
                        .await
                        .is_err()
                    {
                        tracing::debug!("client disconnected before respond; aborting job");
                        return;
                    }

                    let mut stream = resp.bytes_stream();
                    while let Some(chunk) = stream.next().await {
                        match chunk {
                            Ok(bytes) => {
                                if job
                                    .response_tx
                                    .send(ProxyEvent::Chunk(bytes))
                                    .await
                                    .is_err()
                                {
                                    tracing::debug!(
                                        "client disconnected mid-stream; aborting job"
                                    );
                                    return;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "upstream stream error; ending stream");
                                break;
                            }
                        }
                    }
                    // Committed: streamed to completion.
                    return;
                }
                Attempt::KeyExhausted => {
                    resolver.mark_key_exhausted(g.backend_index, k.key_index);
                    tracing::warn!(
                        backend = %g.backend_name,
                        key_index = k.key_index,
                        "key exhausted; trying next key"
                    );
                    continue; // next key, same backend
                }
                Attempt::BackendUnreachable => {
                    resolver.mark_backend_unreachable(g.backend_index);
                    tracing::warn!(
                        backend = %g.backend_name,
                        "backend unreachable; skipping to next backend"
                    );
                    reasons.push(format!("{}: unreachable", g.backend_name));
                    broke_unreachable = true;
                    break; // next BACKEND — skip remaining keys
                }
                Attempt::ClientError(status, body) => {
                    // Request-level problem; propagate verbatim, stop retrying.
                    tracing::info!(
                        backend = %g.backend_name,
                        status,
                        "client error; propagating response verbatim"
                    );
                    if job
                        .response_tx
                        .send(ProxyEvent::Respond {
                            status,
                            content_type: "application/json".into(),
                        })
                        .await
                        .is_err()
                    {
                        tracing::debug!("client disconnected before respond; aborting job");
                        return;
                    }
                    if job
                        .response_tx
                        .send(ProxyEvent::Chunk(bytes::Bytes::from(body)))
                        .await
                        .is_err()
                    {
                        tracing::debug!("client disconnected; aborting job");
                        return;
                    }
                    return;
                }
            }
        }

        // Inner key loop ended without success / ClientError / unreachable-break:
        // every key of this backend was exhausted.
        if !broke_unreachable {
            reasons.push(format!("{}: all keys exhausted", g.backend_name));
        }
    }

    // Every backend tried without success.
    let detail = reasons.join("; ");
    tracing::warn!(model = %job.model, "all backends failed: {}", detail);
    let _ = job
        .response_tx
        .send(ProxyEvent::Failed(format!(
            "all configured backends failed for model '{}': {}",
            job.model, detail
        )))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_status_maps_all_categories() {
        // Success: 2xx.
        assert_eq!(classify_status(200), Class::Success);
        assert_eq!(classify_status(204), Class::Success);
        assert_eq!(classify_status(299), Class::Success);
        // Key-exhausted.
        for s in [401, 402, 403, 429] {
            assert_eq!(classify_status(s), Class::KeyExhausted, "status {s}");
        }
        // Backend-unreachable: 5xx.
        for s in [500, 502, 503, 599] {
            assert_eq!(classify_status(s), Class::BackendUnreachable, "status {s}");
        }
        // Client error: any other 4xx.
        for s in [400, 404, 405, 408, 413, 422] {
            assert_eq!(classify_status(s), Class::ClientError, "status {s}");
        }
        // Boundary: 300/399 are "other" → ClientError (not 2xx, not classified).
        assert_eq!(classify_status(300), Class::ClientError);
        assert_eq!(classify_status(399), Class::ClientError);
    }
}
