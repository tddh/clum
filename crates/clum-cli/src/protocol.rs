use anyhow::Result;
use quinn::SendStream;

/// attach payload 尾字节 mode 协议（与 rmux-bridge `interactive.rs` 保持一致）：
/// 0x00 = legacy mux（spawn `rmux attach-session`），0x01 = raw pane 直通。
pub const ATTACH_MODE_MUX: u8 = 0x00;
pub const ATTACH_MODE_RAW: u8 = 0x01;

/// attach payload 构造（与 rmux-bridge `parse_attach_payload` 逐字段对应）。
/// payload_len 一律程序化取 `body.len()`——曾因手工求和漏计 mode 字节导致
/// bridge 读不满、mode 静默回落 legacy mux（2026-09-14 实测踩坑）。
fn build_attach_payload(
    client_id: &str,
    session_name: &str,
    pane_id: &str,
    cols: u16,
    rows: u16,
    mode: u8,
) -> Vec<u8> {
    let term = "xterm-256color";
    let mut body = Vec::new();
    body.extend_from_slice(&(client_id.len() as u16).to_le_bytes());
    body.extend_from_slice(client_id.as_bytes());
    body.extend_from_slice(&(session_name.len() as u16).to_le_bytes());
    body.extend_from_slice(session_name.as_bytes());
    body.push(pane_id.len() as u8);
    body.extend_from_slice(pane_id.as_bytes());
    body.extend_from_slice(&cols.to_le_bytes());
    body.extend_from_slice(&rows.to_le_bytes());
    body.push(term.len() as u8);
    body.extend_from_slice(term.as_bytes());
    body.push(mode);
    body
}

pub async fn write_attach_request(
    send: &mut SendStream,
    client_id: &str,
    session_name: &str,
    pane_id: &str,
    cols: u16,
    rows: u16,
    mode: u8,
) -> Result<()> {
    let body = build_attach_payload(client_id, session_name, pane_id, cols, rows, mode);
    send.write_all(&[0x01]).await?;
    send.write_all(&(body.len() as u16).to_le_bytes()).await?;
    send.write_all(&body).await?;
    Ok(())
}

pub async fn read_attached_response(recv: &mut quinn::RecvStream) -> Result<Vec<u8>> {
    let mut type_buf = [0u8; 1];
    recv.read_exact(&mut type_buf).await?;
    if type_buf[0] == 0x82 {
        let mut len_buf = [0u8; 2];
        recv.read_exact(&mut len_buf).await?;
        let payload_len = u16::from_le_bytes(len_buf) as usize;
        let mut payload = vec![0u8; payload_len];
        recv.read_exact(&mut payload).await?;
        let _code = payload[0];
        let msg_len = u16::from_le_bytes([payload[1], payload[2]]) as usize;
        let msg = String::from_utf8_lossy(&payload[3..3 + msg_len]);
        anyhow::bail!("bridge error: {}", msg);
    }
    if type_buf[0] != 0x81 {
        anyhow::bail!("unexpected response type: 0x{:02x}", type_buf[0]);
    }

    let mut len_buf = [0u8; 2];
    recv.read_exact(&mut len_buf).await?;
    let _payload_len = u16::from_le_bytes(len_buf) as usize;

    let mut scrollback_len_buf = [0u8; 4];
    recv.read_exact(&mut scrollback_len_buf).await?;
    let scrollback_len = u32::from_le_bytes(scrollback_len_buf) as usize;

    let mut scrollback = vec![0u8; scrollback_len];
    recv.read_exact(&mut scrollback).await?;
    Ok(scrollback)
}

pub async fn write_resize(send: &mut SendStream, cols: u16, rows: u16) -> Result<()> {
    send.write_all(&[0x02]).await?;
    send.write_all(&4u16.to_le_bytes()).await?;
    send.write_all(&cols.to_le_bytes()).await?;
    send.write_all(&rows.to_le_bytes()).await?;
    Ok(())
}

pub async fn write_detach(send: &mut SendStream) -> Result<()> {
    send.write_all(&[0x03]).await?;
    send.write_all(&0u16.to_le_bytes()).await?;
    Ok(())
}

pub async fn send_json_frame(send: &mut SendStream, value: &serde_json::Value) -> Result<()> {
    let json_str = serde_json::to_string(value)?;
    let len = json_str.len() as u32;
    send.write_all(&len.to_le_bytes()).await?;
    send.write_all(json_str.as_bytes()).await?;
    Ok(())
}

pub async fn recv_json_frame(recv: &mut quinn::RecvStream) -> Result<serde_json::Value> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;

    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    let value: serde_json::Value = serde_json::from_slice(&buf)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 镜像 rmux-bridge `parse_attach_payload` 的字段顺序遍历 body，
    /// 锁住两端 wire 一致性（字段顺序、长度前缀、mode 尾字节）。
    #[test]
    fn attach_payload_round_trip_matches_bridge_parsing() {
        let body = build_attach_payload("client-1", "clum", "%0", 120, 35, ATTACH_MODE_RAW);

        let mut off = 0usize;
        let id_len = u16::from_le_bytes([body[off], body[off + 1]]) as usize;
        off += 2;
        let id = std::str::from_utf8(&body[off..off + id_len]).unwrap();
        off += id_len;
        assert_eq!(id, "client-1");

        let sn_len = u16::from_le_bytes([body[off], body[off + 1]]) as usize;
        off += 2;
        let sn = std::str::from_utf8(&body[off..off + sn_len]).unwrap();
        off += sn_len;
        assert_eq!(sn, "clum");

        let pane_len = body[off] as usize;
        off += 1;
        let pane = std::str::from_utf8(&body[off..off + pane_len]).unwrap();
        off += pane_len;
        assert_eq!(pane, "%0");

        let cols = u16::from_le_bytes([body[off], body[off + 1]]);
        let rows = u16::from_le_bytes([body[off + 2], body[off + 3]]);
        off += 4;
        assert_eq!((cols, rows), (120, 35));

        let term_len = body[off] as usize;
        off += 1;
        let term = std::str::from_utf8(&body[off..off + term_len]).unwrap();
        off += term_len;
        assert_eq!(term, "xterm-256color");

        // 尾部恰好还剩 1 字节：mode —— payload_len=body.len() 时 bridge 必读满
        assert_eq!(off + 1, body.len());
        assert_eq!(body[off], ATTACH_MODE_RAW);
    }

    #[test]
    fn attach_payload_mux_mode_keeps_same_shape() {
        let body = build_attach_payload("c", "s", "%9", 80, 24, ATTACH_MODE_MUX);
        assert_eq!(body.last(), Some(&ATTACH_MODE_MUX));
    }
}
