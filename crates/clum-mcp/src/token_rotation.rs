use std::sync::Arc;
use std::time::Duration;

use crate::bridge_store::BridgeStore;
use crate::registry::BridgeRegistry;

pub async fn run_rotation_loop(
    registry: Arc<BridgeRegistry>,
    store: Arc<BridgeStore>,
    ttl_hours: u64,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    interval.tick().await; // skip first immediate tick

    loop {
        interval.tick().await;
        rotate_expired_tokens(&registry, &store, ttl_hours).await;
    }
}

async fn rotate_expired_tokens(
    registry: &Arc<BridgeRegistry>,
    store: &Arc<BridgeStore>,
    ttl_hours: u64,
) {
    let bridges = store.list().await;
    let now = chrono::Utc::now();

    for bridge in bridges {
        if bridge.revoked {
            continue;
        }

        let created = match chrono::DateTime::parse_from_rfc3339(&bridge.created_at) {
            Ok(t) => t.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };

        let age_hours = (now - created).num_hours();
        if age_hours < ttl_hours as i64 {
            continue;
        }

        let conn = match registry.get(&bridge.hostname).await {
            Some(c) => c,
            None => {
                tracing::debug!(hostname = %bridge.hostname, "token expired but bridge offline, skipping rotation");
                continue;
            }
        };

        let new_token = match crate::bridge_store::generate_bridge_token() {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("failed to generate bridge token: {e}");
                continue;
            }
        };
        let msg = serde_json::json!({
            "type": "token_rotate",
            "new_token": new_token,
        });

        match conn.send_control_frame(&msg).await {
            Ok(()) => {
                if let Err(e) = store.rotate_token(&bridge.hostname, &new_token).await {
                    tracing::error!(hostname = %bridge.hostname, "failed to update token in db: {e}");
                } else {
                    tracing::info!(hostname = %bridge.hostname, "token rotated successfully");
                }
            }
            Err(e) => {
                tracing::warn!(hostname = %bridge.hostname, "failed to push token rotation: {e}");
            }
        }
    }
}
