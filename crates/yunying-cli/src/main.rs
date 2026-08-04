mod ai;
mod connect;
mod protocol;
mod replay;
mod transfer;
mod tui;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "yunying-cli", about = "AI Agent 远程运维 CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(long, default_value = "~/.yunying/hosts.yaml")]
    hosts_file: String,

    #[arg(long, default_value = "~/.yunying/ca.crt")]
    ca_cert: String,

    /// Central server address. If set, connect via server relay instead of direct to bridge.
    #[arg(long, env = "YUNYING_SERVER_ADDR")]
    server_addr: Option<String>,

    /// API key for server authentication.
    #[arg(long, env = "YUNYING_API_KEY")]
    api_key: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    Connect {
        host: String,

        #[arg(long, default_value = "yunying")]
        session: String,

        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        readonly: bool,

        #[arg(long, default_value = ".")]
        opencode_dir: String,
    },

    List {
        host: String,
    },

    /// Replay a recorded terminal session (.cast file)
    Replay {
        /// Path to the .cast recording file
        file: String,

        /// Playback speed multiplier (e.g. 2.0 = 2x faster)
        #[arg(long, default_value = "1.0")]
        speed: f64,

        /// Cap idle time between events (seconds)
        #[arg(long)]
        idle: Option<f64>,
    },

    /// Upload a file or directory to a remote host
    Upload {
        host: String,
        local_path: String,
        remote_path: String,

        /// Glob patterns to exclude when uploading a directory (repeatable)
        #[arg(long)]
        exclude: Vec<String>,
    },

    /// Download a file or directory from a remote host
    Download {
        host: String,
        remote_path: String,
        local_path: String,
    },

    /// Create a local port forward to a remote service
    Tunnel {
        host: String,

        /// Local port to listen on
        #[arg(long)]
        local: u16,

        /// Remote target (host:port)
        #[arg(long)]
        remote: String,
    },
}

fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") || path.starts_with("~\\") {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .ok();
        if let Some(home) = home {
            return home + &path[1..];
        }
    }
    path.to_string()
}

fn load_host_config(hosts_file: &str, host_name: &str) -> anyhow::Result<yunying_core::HostConfig> {
    let contents = std::fs::read_to_string(hosts_file)?;
    let registry: yunying_core::HostRegistry = serde_yml::from_str(&contents)?;
    registry
        .hosts
        .into_iter()
        .find(|h| h.name == host_name)
        .ok_or_else(|| anyhow::anyhow!("host '{}' not found in {}", host_name, hosts_file))
}

async fn get_connection(
    server_addr: &Option<String>,
    ca_cert: &str,
    api_key: &Option<String>,
    hosts_file: &str,
    host: &str,
    purpose: &str,
) -> anyhow::Result<quinn::Connection> {
    if let Some(addr) = server_addr {
        connect::connect_via_server(addr, ca_cert, host, api_key.as_deref(), purpose).await
    } else {
        let config = load_host_config(hosts_file, host)?;
        let addr = config
            .bridge_addr
            .as_deref()
            .context("bridge_addr not configured in hosts.yaml")?;
        let token = config
            .bridge_token
            .as_deref()
            .context("bridge_token not configured in hosts.yaml")?;
        connect::connect_to_bridge_quic(addr, token, ca_cert).await
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::io::stderr)
        .init();
    let mut cli = Cli::parse();
    cli.hosts_file = expand_tilde(&cli.hosts_file);
    cli.ca_cert = expand_tilde(&cli.ca_cert);

    let server_addr = cli.server_addr.clone();
    let ca_cert = cli.ca_cert.clone();
    let api_key = cli.api_key.clone();
    let hosts_file = cli.hosts_file.clone();

    let result = match cli.command {
        Commands::Connect {
            host,
            session,
            pane,
            readonly,
            opencode_dir,
        } => {
            if let Some(server_addr) = &cli.server_addr {
                let pane = pane.unwrap_or_else(|| "%0".to_string());
                crate::tui::run_connect_with_ai(
                    None,
                    &cli.ca_cert,
                    &session,
                    &pane,
                    readonly,
                    &opencode_dir,
                    Some((server_addr.clone(), host.clone())),
                    cli.api_key.as_deref(),
                )
                .await
            } else {
                let config = load_host_config(&cli.hosts_file, &host)?;
                let pane = match pane {
                    Some(p) => p,
                    None => connect::find_lowest_pane(&config, &cli.ca_cert, &session).await?,
                };
                crate::tui::run_connect_with_ai(
                    Some(&config),
                    &cli.ca_cert,
                    &session,
                    &pane,
                    readonly,
                    &opencode_dir,
                    None,
                    None,
                )
                .await
            }
        }
        Commands::List { host } => {
            let conn = get_connection(&server_addr, &ca_cert, &api_key, &hosts_file, &host, "list")
                .await?;
            let (mut send, mut recv) = conn.open_bi().await?;
            send.write_all(&[0x01]).await?;
            let request = serde_json::json!({ "type": "list_sessions" });
            crate::protocol::send_json_frame(&mut send, &request).await?;
            let response = crate::protocol::recv_json_frame(&mut recv).await?;

            if response["ok"].as_bool().unwrap_or(false) {
                if let Some(sessions) = response["sessions"].as_array() {
                    if sessions.is_empty() {
                        println!("No active sessions on {host}");
                    } else {
                        println!("{:<30} HOST", "SESSION");
                        println!("{}", "-".repeat(50));
                        for s in sessions {
                            println!("{:<30} {}", s["session_name"].as_str().unwrap_or("-"), host);
                        }
                        println!("\n{} session(s) on {host}", sessions.len());
                    }
                }
                Ok(())
            } else {
                anyhow::bail!(
                    "list failed: {}",
                    response["error"].as_str().unwrap_or("unknown")
                )
            }
        }
        Commands::Replay { file, speed, idle } => {
            let expanded = expand_tilde(&file);
            let path = std::path::Path::new(&expanded);

            let local_path = if path.exists() {
                expanded
            } else if let Some(server) = &cli.server_addr {
                let url = format!("http://{server}/recordings/{expanded}");
                eprintln!("Fetching recording from {url} ...");
                let tmp_file = std::env::temp_dir().join("yunying-replay.cast");
                let tmp_path = tmp_file.to_string_lossy().to_string();
                let mut cmd = std::process::Command::new("curl");
                cmd.args(["-fsSL", "-o", &tmp_path]);
                if let Some(key) = &cli.api_key {
                    cmd.args(["-H", &format!("Authorization: Bearer {key}")]);
                }
                cmd.arg(&url);
                let resp = tokio::task::block_in_place(|| cmd.status())?;
                if !resp.success() {
                    anyhow::bail!("failed to download recording from {url}");
                }
                tmp_path
            } else {
                anyhow::bail!("file not found: {expanded} (use --server-addr for remote replay)");
            };

            replay::replay(
                std::path::Path::new(&local_path),
                &replay::ReplayOptions {
                    speed,
                    idle_limit: idle,
                },
            )
        }
        Commands::Upload {
            host,
            local_path,
            remote_path,
            exclude,
        } => {
            let conn = get_connection(
                &server_addr,
                &ca_cert,
                &api_key,
                &hosts_file,
                &host,
                "upload",
            )
            .await?;
            let result = transfer::upload(&conn, &local_path, &remote_path, &exclude).await;
            conn.close(0u32.into(), b"done");
            result
        }
        Commands::Download {
            host,
            remote_path,
            local_path,
        } => {
            let conn = get_connection(
                &server_addr,
                &ca_cert,
                &api_key,
                &hosts_file,
                &host,
                "download",
            )
            .await?;
            let result = transfer::download(&conn, &remote_path, &local_path).await;
            conn.close(0u32.into(), b"done");
            result
        }
        Commands::Tunnel {
            host,
            local,
            remote,
        } => {
            let (remote_host, remote_port) = remote
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("invalid --remote format, expected host:port"))?;
            let remote_port: u16 = remote_port.parse()?;

            let conn = get_connection(
                &server_addr,
                &ca_cert,
                &api_key,
                &hosts_file,
                &host,
                "tunnel",
            )
            .await?;
            let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{local}")).await?;
            println!(
                "tunnel: 127.0.0.1:{local} → {host}:{remote_host}:{remote_port} (Ctrl+C to stop)"
            );

            loop {
                let (mut tcp_stream, peer) = listener.accept().await?;
                let conn = conn.clone();
                let rh = remote_host.to_string();
                let rp = remote_port;
                tokio::spawn(async move {
                    let (mut send, mut recv) = match conn.open_bi().await {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("open stream failed: {e}");
                            return;
                        }
                    };
                    if send.write_all(&[0x05]).await.is_err() {
                        return;
                    }
                    if send
                        .write_all(&(rh.len() as u16).to_le_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if send.write_all(rh.as_bytes()).await.is_err() {
                        return;
                    }
                    if send.write_all(&rp.to_le_bytes()).await.is_err() {
                        return;
                    }

                    let (mut tcp_read, mut tcp_write) = tcp_stream.split();
                    let t2q = async {
                        use tokio::io::AsyncReadExt;
                        let mut buf = [0u8; 8192];
                        loop {
                            match tcp_read.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    if send.write_all(&buf[..n]).await.is_err() {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let _ = send.finish();
                    };
                    let q2t = async {
                        use tokio::io::AsyncWriteExt;
                        let mut buf = [0u8; 8192];
                        while let Ok(Some(n)) = recv.read(&mut buf).await {
                            if tcp_write.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    };
                    tokio::join!(t2q, q2t);
                    tracing::debug!("tunnel connection from {peer} closed");
                });
            }
        }
    };
    crate::ai::kill_serve().await;
    result
}
