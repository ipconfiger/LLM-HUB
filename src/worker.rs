//! Background worker owning the proxy job queue.
//!
//! The worker receives [`ProxyJob`]s from the HTTP layer and dispatches each
//! one to its own [`tokio::task`], so concurrent requests never serialize.
//! Each task runs [`crate::proxy::run`] to perform the actual failover + streaming.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::Resolver;
use crate::proxy;

/// A single unit of proxy work, enqueued by the HTTP handler and consumed by
/// the worker.
pub struct ProxyJob {
    /// The request path to forward upstream, e.g. `/v1/chat/completions`.
    pub path: String,
    /// The raw upstream request body, forwarded verbatim to the backend.
    pub body: bytes::Bytes,
    /// The model name extracted from the request body; used for resolution.
    pub model: String,
    /// Channel over which the worker streams the upstream response back to the
    /// handler that created this job.
    pub response_tx: mpsc::Sender<ProxyEvent>,
}

/// Events streamed from the worker back to the HTTP handler.
pub enum ProxyEvent {
    /// Sent exactly once before any body data: the chosen upstream's status and
    /// content type. Receiving this commits the handler to a streaming response.
    Respond {
        /// HTTP status code returned by the upstream backend.
        status: u16,
        /// `Content-Type` header value (defaults to `text/event-stream` when the
        /// upstream omits it).
        content_type: String,
    },
    /// A chunk of the upstream response body.
    Chunk(bytes::Bytes),
    /// Terminal failure: no backend succeeded. Carries a human-readable detail
    /// suitable for surfacing to the client as an error message.
    Failed(String),
}

/// Spawn the proxy worker.
///
/// The worker owns `client` and `resolver`, receiving [`ProxyJob`]s from `rx`.
/// Each job is dispatched to a fresh [`tokio::task`] (cloning the shared client
/// and resolver) so concurrent requests are handled in parallel rather than
/// serialized behind the queue.
///
/// Returns a [`JoinHandle`] for the worker's driver task.
pub fn spawn(
    client: reqwest::Client,
    resolver: Arc<Resolver>,
    mut rx: mpsc::Receiver<ProxyJob>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("proxy worker started");
        while let Some(job) = rx.recv().await {
            let client = client.clone();
            let resolver = resolver.clone();
            tokio::spawn(async move {
                tracing::info!(
                    model = %job.model,
                    path = %job.path,
                    "dispatching proxy job"
                );
                proxy::run(job, &client, &resolver).await;
            });
        }
        tracing::info!("proxy worker stopped (job channel closed)");
    })
}
