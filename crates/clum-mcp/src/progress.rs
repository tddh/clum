use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const THROTTLE_INTERVAL: Duration = Duration::from_millis(200);
/// Match the design default: a human has 120s to answer an elicitation.
const ELICITATION_TIMEOUT: Duration = Duration::from_secs(120);

enum ReporterBackend {
    Stdout(Arc<Mutex<tokio::io::Stdout>>),
    Peer(rmcp::service::Peer<rmcp::RoleServer>),
}

pub struct ProgressReporter {
    token: Option<rmcp::model::ProgressToken>,
    backend: ReporterBackend,
    last_sent: Instant,
    failed: bool,
}

impl Clone for ProgressReporter {
    fn clone(&self) -> Self {
        let backend = match &self.backend {
            ReporterBackend::Stdout(w) => ReporterBackend::Stdout(Arc::clone(w)),
            ReporterBackend::Peer(p) => ReporterBackend::Peer(p.clone()),
        };
        Self {
            token: self.token.clone(),
            backend,
            last_sent: Instant::now()
                .checked_sub(THROTTLE_INTERVAL)
                .unwrap_or_else(Instant::now),
            failed: false,
        }
    }
}

impl ProgressReporter {
    pub fn new_stdout(token: Option<Value>, writer: Arc<Mutex<tokio::io::Stdout>>) -> Self {
        let progress_token = token.map(|v| match v {
            Value::String(s) => {
                rmcp::model::ProgressToken(rmcp::model::NumberOrString::String(s.into()))
            }
            Value::Number(n) => rmcp::model::ProgressToken(rmcp::model::NumberOrString::Number(
                n.as_i64().unwrap_or(0),
            )),
            _ => rmcp::model::ProgressToken(rmcp::model::NumberOrString::String("default".into())),
        });
        Self {
            token: progress_token,
            backend: ReporterBackend::Stdout(writer),
            last_sent: Instant::now()
                .checked_sub(THROTTLE_INTERVAL)
                .unwrap_or_else(Instant::now),
            failed: false,
        }
    }

    pub fn new_peer(
        token: Option<rmcp::model::ProgressToken>,
        peer: rmcp::service::Peer<rmcp::RoleServer>,
    ) -> Self {
        Self {
            token,
            backend: ReporterBackend::Peer(peer),
            last_sent: Instant::now()
                .checked_sub(THROTTLE_INTERVAL)
                .unwrap_or_else(Instant::now),
            failed: false,
        }
    }

    /// The MCP peer for this request, if the transport has one. stdio mode uses
    /// `Stdout` and therefore falls back to the CLI approval flow.
    pub fn peer(&self) -> Option<&rmcp::service::Peer<rmcp::RoleServer>> {
        match &self.backend {
            ReporterBackend::Peer(peer) => Some(peer),
            ReporterBackend::Stdout(_) => None,
        }
    }

    /// True when the client declared form/url elicitation during initialize.
    pub fn supports_elicitation(&self) -> bool {
        self.peer()
            .and_then(|peer| peer.peer_info())
            .and_then(|info| info.capabilities.elicitation.clone())
            .is_some()
    }

    /// Ask the client to show an approval prompt. Returns `None` when the
    /// client does not support elicitation or the request times out — callers
    /// must fall back to `APPROVAL_REQUIRED` (fail-closed).
    pub async fn elicit(&self, message: String) -> Option<rmcp::model::ElicitationAction> {
        let peer = self.peer()?;
        if !self.supports_elicitation() {
            return None;
        }
        let schema = rmcp::model::ElicitationSchema::builder().build().ok()?;
        let request = rmcp::model::ElicitRequest::new(
            rmcp::model::ElicitRequestParams::FormElicitationParams {
                meta: None,
                message,
                requested_schema: schema,
            },
        );
        let response = tokio::time::timeout(
            ELICITATION_TIMEOUT,
            peer.send_request(rmcp::model::ServerRequest::ElicitRequest(request)),
        )
        .await
        .ok()?
        .ok()?;
        match response {
            rmcp::model::ClientResult::ElicitResult(result) => Some(result.action),
            _ => None,
        }
    }

    pub async fn report(&mut self, progress: u64, total: u64, message: &str) {
        let token = match &self.token {
            Some(t) if !self.failed => t,
            _ => return,
        };
        if self.last_sent.elapsed() < THROTTLE_INTERVAL && progress < total {
            return;
        }
        self.last_sent = Instant::now();

        match &self.backend {
            ReporterBackend::Stdout(writer) => {
                let notification = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": token,
                        "progress": progress,
                        "total": total,
                        "message": message,
                    }
                });
                use tokio::io::AsyncWriteExt;
                let mut w = writer.lock().await;
                let result = async {
                    w.write_all(notification.to_string().as_bytes()).await?;
                    w.write_all(b"\n").await?;
                    w.flush().await
                }
                .await;
                if result.is_err() {
                    self.failed = true;
                }
            }
            ReporterBackend::Peer(peer) => {
                let mut params =
                    rmcp::model::ProgressNotificationParam::new(token.clone(), progress as f64);
                params.total = Some(total as f64);
                params.message = Some(message.to_string());

                let result = peer
                    .send_notification(rmcp::model::ServerNotification::ProgressNotification(
                        rmcp::model::ProgressNotification::new(params),
                    ))
                    .await;
                if result.is_err() {
                    self.failed = true;
                }
            }
        }
    }
}
