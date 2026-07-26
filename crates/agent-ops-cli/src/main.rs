mod ai;
mod connect;
mod protocol;
mod replay;
mod tui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agent-ops-cli", about = "AI Agent 远程运维 CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(long, default_value = "~/.agent-ops/hosts.yaml")]
    hosts_file: String,

    #[arg(long, default_value = "~/.agent-ops/ca.crt")]
    ca_cert: String,
}

#[derive(Subcommand)]
enum Commands {
    Connect {
        host: String,

        #[arg(long, default_value = "agent-ops")]
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
}

fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return home + &path[1..];
        }
    }
    path.to_string()
}

fn load_host_config(
    hosts_file: &str,
    host_name: &str,
) -> anyhow::Result<agent_ops_core::HostConfig> {
    let contents = std::fs::read_to_string(hosts_file)?;
    let registry: agent_ops_core::HostRegistry = serde_yml::from_str(&contents)?;
    registry
        .hosts
        .into_iter()
        .find(|h| h.name == host_name)
        .ok_or_else(|| anyhow::anyhow!("host '{}' not found in {}", host_name, hosts_file))
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

    let result = match cli.command {
        Commands::Connect {
            host,
            session,
            pane,
            readonly,
            opencode_dir,
        } => {
            let config = load_host_config(&cli.hosts_file, &host)?;
            let pane = match pane {
                Some(p) => p,
                None => connect::find_lowest_pane(&config, &cli.ca_cert, &session).await?,
            };
            crate::tui::run_connect_with_ai(
                &config,
                &cli.ca_cert,
                &session,
                &pane,
                readonly,
                &opencode_dir,
            )
            .await
        }
        Commands::List { host } => {
            let config = load_host_config(&cli.hosts_file, &host)?;
            connect::list_sessions(&config, &cli.ca_cert).await
        }
        Commands::Replay { file, speed, idle } => {
            let expanded = expand_tilde(&file);
            let path = std::path::Path::new(&expanded);
            replay::replay(
                path,
                &replay::ReplayOptions {
                    speed,
                    idle_limit: idle,
                },
            )
        }
    };
    crate::ai::kill_serve().await;
    result
}
