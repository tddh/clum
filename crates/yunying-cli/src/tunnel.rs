use std::time::Duration;

use anyhow::{bail, Context, Result};

/// 解析 "30s" / "10m" / "2h" / 纯秒数 "7200"；"0" 表示永不（Duration::ZERO）。
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num
        .parse()
        .with_context(|| format!("invalid duration: {s}"))?;
    let secs = match unit {
        "s" => Some(n),
        "m" => n.checked_mul(60),
        "h" => n.checked_mul(3600),
        _ => bail!("invalid duration unit in: {s} (expected s/m/h)"),
    }
    .context(format!("duration too large: {s}"))?;
    Ok(Duration::from_secs(secs))
}

const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[allow(clippy::too_many_arguments)] // mirrors get_connection's conn params + tunnel args
pub async fn run(
    server_addr: &Option<String>,
    ca_cert: &str,
    api_key: &Option<String>,
    hosts_file: &str,
    host: &str,
    local: u16,
    remote_host: &str,
    remote_port: u16,
    give_up_after: Duration, // Duration::ZERO = 永不放弃
) -> Result<()> {
    let mut conn =
        crate::get_connection(server_addr, ca_cert, api_key, hosts_file, host, "tunnel").await?;
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{local}")).await?;
    println!(
        "tunnel: 127.0.0.1:{local} → {host}:{remote_host}:{remote_port} (Ctrl+C to stop{})",
        if give_up_after == Duration::ZERO {
            String::new()
        } else {
            format!(", gives up after {}s offline", give_up_after.as_secs())
        }
    );

    let mut down_since: Option<std::time::Instant> = None;
    let mut backoff = Duration::from_secs(1);

    loop {
        if let Some(since) = down_since {
            // 重连阶段
            if give_up_after != Duration::ZERO && since.elapsed() >= give_up_after {
                bail!(
                    "tunnel giving up: offline for over {}s without successful reconnect",
                    give_up_after.as_secs()
                );
            }
            let wait = backoff;
            let wait = if give_up_after != Duration::ZERO {
                wait.min(give_up_after.saturating_sub(since.elapsed()))
            } else {
                wait
            };
            tokio::time::sleep(wait).await;
            match crate::get_connection(server_addr, ca_cert, api_key, hosts_file, host, "tunnel")
                .await
            {
                Ok(c) => {
                    conn = c;
                    down_since = None;
                    backoff = Duration::from_secs(1);
                    eprintln!("tunnel: reconnected");
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if msg.contains("rejected") || msg.contains("auth") {
                        eprintln!("tunnel: reconnect failed (check credentials): {msg}");
                    } else {
                        eprintln!("tunnel: reconnect failed: {msg}");
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
            continue;
        }

        // 正常服务阶段：竞争 accept 与连接死亡
        tokio::select! {
            accept_res = listener.accept() => {
                let (tcp_stream, peer) = accept_res?;
                let conn = conn.clone();
                let rh = remote_host.to_string();
                let rp = remote_port;
                tokio::spawn(relay_one(conn, tcp_stream, rh, rp, peer));
            }
            reason = conn.closed() => {
                down_since = Some(std::time::Instant::now());
                eprintln!("tunnel: connection lost ({reason}), reconnecting...");
            }
        }
    }
}

/// 单条 TCP 会话的转发：向 bridge 打开 QUIC 双向流，写 0x05 隧道协议头，然后双向搬运字节。
async fn relay_one(
    conn: quinn::Connection,
    mut tcp_stream: tokio::net::TcpStream,
    rh: String,
    rp: u16,
    peer: std::net::SocketAddr,
) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("-5m").is_err());
        assert!(parse_duration("99999999999999999999h").is_err());
        assert!(parse_duration("9999999999999999h").is_err());
    }
}
