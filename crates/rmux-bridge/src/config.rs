//! CLI configuration parsed from command-line arguments. Defines the
//! QUIC listen address, RMUX socket path, TLS certificate/key paths, and
//! the authentication token.

use clap::Parser;
use std::path::PathBuf;

/// Command-line configuration for the rmux-bridge daemon.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "rmux-bridge",
    version,
    about = "RMUX Bridge - QUIC to Unix socket protocol-aware proxy"
)]
pub struct BridgeConfig {
    #[arg(long, default_value = "/tmp/rmux-1000/default")]
    pub rmux_socket: String,

    #[arg(long, default_value = "certs/bridge.crt")]
    pub tls_cert: PathBuf,

    #[arg(long, default_value = "certs/bridge.key")]
    pub tls_key: PathBuf,

    #[arg(long, env = "BRIDGE_AUTH_TOKEN")]
    pub auth_token: String,

    /// QUIC listen address (UDP). Used for terminal operations, file transfers,
    /// tunnels, and interactive sessions.
    #[arg(long, default_value = "0.0.0.0:9778", env = "QUIC_LISTEN_ADDR")]
    pub quic_listen_addr: String,

    /// Maximum concurrent connections. 0 = unlimited.
    #[arg(long, default_value = "256", env = "MAX_CONNECTIONS")]
    pub max_connections: usize,

    /// Interactive session idle timeout in seconds. After this period of
    /// inactivity on the control stream, the bridge disconnects the client
    /// and restores pane layout. 0 = disabled. Default: 28800 (8 hours).
    #[arg(long, default_value = "28800", env = "IDLE_TIMEOUT_SECS")]
    pub idle_timeout_secs: u64,

    /// Log level: trace, debug, info, warn, error.
    #[arg(long, default_value = "info", env = "RUST_LOG")]
    pub log_level: String,

    /// Enable PTY session recording (asciinema v2 format).
    #[arg(long, default_value = "true", env = "RECORDING_ENABLED")]
    pub recording_enabled: bool,

    /// Directory for recording files. Defaults to {binary_dir}/recordings.
    #[arg(long, env = "RECORDING_DIR")]
    pub recording_dir: Option<PathBuf>,

    /// Recording retention in days.
    #[arg(long, default_value = "90", env = "RECORDING_RETENTION_DAYS")]
    pub recording_retention_days: u32,

    /// Maximum total recording size in MB.
    #[arg(long, default_value = "500", env = "RECORDING_MAX_SIZE_MB")]
    pub recording_max_size_mb: u64,

    /// Fsync interval for recording files in seconds.
    #[arg(long, default_value = "5", env = "RECORDING_FSYNC_INTERVAL_SECS")]
    pub recording_fsync_interval_secs: u64,

    /// Bridge audit event database path. Defaults to {binary_dir}/bridge_events.db.
    #[arg(long, env = "BRIDGE_AUDIT_DB")]
    pub bridge_audit_db: Option<PathBuf>,

    /// Central server address (host:port). If set, bridge registers with server.
    #[arg(long, env = "YUNYING_SERVER_ADDR")]
    pub server_addr: Option<String>,

    /// CA certificate for verifying server TLS (private CA). Omit for public CA.
    #[arg(long, env = "YUNYING_CA_CERT")]
    pub ca_cert: Option<PathBuf>,
}

impl BridgeConfig {
    pub fn resolve_recording_dir(&self) -> PathBuf {
        self.recording_dir.clone().unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
                .join("recordings")
        })
    }

    pub fn resolve_audit_db_path(&self) -> PathBuf {
        self.bridge_audit_db.clone().unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
                .join("bridge_events.db")
        })
    }
}
