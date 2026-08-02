//! RMUX protocol layer: connects to a local RMUX daemon over a Unix socket
//! and translates JSON requests into RMUX SDK calls. Handles all 55 tool
//! message types plus pane output streaming.

use anyhow::Result;
use regex::Regex;
use rmux_sdk::PaneId;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;

static ANSI_RE: OnceLock<Regex> = OnceLock::new();
static PANE_ID_RE: OnceLock<Regex> = OnceLock::new();
static BUFFER_PARSE_RE: OnceLock<Regex> = OnceLock::new();

pub struct PaneOutputStream {
    pub rx: mpsc::Receiver<String>,
}

/// Wraps an RMUX SDK connection and exposes JSON-handling methods for each
/// protocol message type used by the MCP server.
pub struct ProtocolProxy {
    rmux: rmux_sdk::Rmux,
    /// Facade with no SDK-level timeout, for long-wait operations
    /// (collect_until_exit, wait_for_bytes) where the caller's timeout_ms
    /// governs the deadline via bridge-side tokio::time::timeout.
    rmux_long: rmux_sdk::Rmux,
    socket_path: String,
    generation: u64,
}

impl ProtocolProxy {
    /// Connects to the RMUX daemon at the given Unix socket path.
    pub async fn connect(socket_path: &str) -> Result<Self> {
        let rmux = rmux_sdk::Rmux::builder()
            .unix_socket(socket_path)
            .default_timeout(Duration::from_secs(30))
            .connect()
            .await?;
        let rmux_long = rmux_sdk::Rmux::builder()
            .unix_socket(socket_path)
            .default_timeout(Duration::MAX)
            .connect()
            .await?;
        Ok(Self {
            rmux,
            rmux_long,
            socket_path: socket_path.to_string(),
            generation: 0,
        })
    }

    /// Returns the Unix socket path used to connect to the RMUX daemon.
    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Reconnect both rmux connections after a transport failure.
    pub async fn reconnect(&mut self) -> Result<()> {
        self.rmux = rmux_sdk::Rmux::builder()
            .unix_socket(&self.socket_path)
            .default_timeout(Duration::from_secs(30))
            .connect()
            .await?;
        self.rmux_long = rmux_sdk::Rmux::builder()
            .unix_socket(&self.socket_path)
            .default_timeout(Duration::MAX)
            .connect()
            .await?;
        self.generation += 1;
        tracing::info!(generation = self.generation, "rmux SDK reconnected");
        Ok(())
    }

    fn parse_pane_id(raw: &str) -> Option<PaneId> {
        raw.strip_prefix('%')
            .and_then(|n| n.parse::<u32>().ok())
            .map(PaneId::new)
    }

    fn clean_text(raw: &str, command: Option<&str>) -> String {
        let ansi_re = ANSI_RE.get_or_init(|| {
            Regex::new(r"\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])").expect("invalid ANSI regex")
        });
        let no_ansi = ansi_re.replace_all(raw, "");

        let mut lines: Vec<&str> = no_ansi.lines().collect();

        if let Some(cmd) = command {
            while let Some(first) = lines.first() {
                let t = first.trim();
                if t.is_empty() || t.contains(cmd) || t == cmd {
                    lines.remove(0);
                } else {
                    break;
                }
            }
        }

        lines
            .into_iter()
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

mod buffer;
mod exec;
mod output;
mod pane;
mod session;
mod window;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pane_id_valid() {
        assert!(ProtocolProxy::parse_pane_id("%0").is_some());
        assert!(ProtocolProxy::parse_pane_id("%42").is_some());
        assert!(ProtocolProxy::parse_pane_id("%999").is_some());
    }

    #[test]
    fn test_parse_pane_id_invalid() {
        assert!(ProtocolProxy::parse_pane_id("0").is_none()); // no % prefix
        assert!(ProtocolProxy::parse_pane_id("%abc").is_none()); // non-numeric
        assert!(ProtocolProxy::parse_pane_id("").is_none()); // empty
        assert!(ProtocolProxy::parse_pane_id("%-1").is_none()); // negative
    }

    #[test]
    fn test_clean_text_strips_ansi() {
        let input = "\x1B[31mred text\x1B[0m";
        let cleaned = ProtocolProxy::clean_text(input, None);
        assert!(!cleaned.contains('\x1B'));
        assert!(cleaned.contains("red text"));
    }

    #[test]
    fn test_clean_text_keeps_prompt_lines() {
        let input = "root@host:~# ls\nfile1\nfile2";
        let cleaned = ProtocolProxy::clean_text(input, None);
        // prompt lines are no longer stripped — exec returns full terminal context
        assert!(cleaned.contains("root@"));
        assert!(cleaned.contains("file1"));
        assert!(cleaned.contains("file2"));
    }

    #[test]
    fn test_clean_text_strips_command_echo() {
        let input = "ls\nfile1\nfile2";
        let cleaned = ProtocolProxy::clean_text(input, Some("ls"));
        assert!(!cleaned.contains("ls"));
        assert!(cleaned.contains("file1"));
    }

    #[test]
    fn test_clean_text_strips_empty_lines() {
        let input = "\n\nhello\n\n";
        let cleaned = ProtocolProxy::clean_text(input, None);
        assert_eq!(cleaned, "hello");
    }
}
