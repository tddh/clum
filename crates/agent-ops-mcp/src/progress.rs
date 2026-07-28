use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

const THROTTLE_INTERVAL: Duration = Duration::from_millis(200);

pub struct ProgressReporter {
    token: Option<Value>,
    writer: Arc<Mutex<tokio::io::Stdout>>,
    last_sent: Instant,
    failed: bool,
}

impl ProgressReporter {
    pub fn new(token: Option<Value>, writer: Arc<Mutex<tokio::io::Stdout>>) -> Self {
        Self {
            token,
            writer,
            last_sent: Instant::now()
                .checked_sub(THROTTLE_INTERVAL)
                .unwrap_or_else(Instant::now),
            failed: false,
        }
    }

    pub fn noop() -> Self {
        Self {
            token: None,
            writer: Arc::new(Mutex::new(tokio::io::stdout())),
            last_sent: Instant::now(),
            failed: true,
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
        let mut w = self.writer.lock().await;
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
}
