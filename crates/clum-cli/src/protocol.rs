use anyhow::Result;
use quinn::SendStream;

pub async fn write_attach_request(
    send: &mut SendStream,
    session_name: &str,
    pane_id: &str,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let term = "xterm-256color";
    let payload_len = 2 + session_name.len() + 1 + pane_id.len() + 2 + 2 + 1 + term.len();

    send.write_all(&[0x01]).await?;
    send.write_all(&(payload_len as u16).to_le_bytes()).await?;
    send.write_all(&(session_name.len() as u16).to_le_bytes())
        .await?;
    send.write_all(session_name.as_bytes()).await?;
    send.write_all(&[pane_id.len() as u8]).await?;
    send.write_all(pane_id.as_bytes()).await?;
    send.write_all(&cols.to_le_bytes()).await?;
    send.write_all(&rows.to_le_bytes()).await?;
    send.write_all(&[term.len() as u8]).await?;
    send.write_all(term.as_bytes()).await?;
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
