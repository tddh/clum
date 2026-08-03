//! 交互式终端处理器：处理 QUIC 0x06（控制流）和 0x07（数据流）
//!
//! 参考：docs/connect-design.md

use anyhow::{Context, Result};
use quinn::{RecvStream, SendStream};
use rmux_sdk::TerminalSizeSpec;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex, Notify};

use crate::bridge_audit::{self, BridgeAuditDb};
use crate::cast_recorder::{finalize_cast, CastRecorder};
use crate::protocol::ProtocolProxy;

/// Interactive session state shared between control (0x06) and data (0x07) streams.
pub struct InteractiveSession {
    pub session_name: String,
    pub pane_id: String,
    pub cols: u16,
    pub rows: u16,
    pub socket_path: String,
    pub master_fd: Option<OwnedFd>,
    pub child_pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub exit_notify: Arc<Notify>,
    pub recording_file: Option<String>,
}

const SCROLLBACK_LINES: usize = 50;

async fn read_u8(recv: &mut RecvStream) -> Result<u8> {
    let mut buf = [0u8; 1];
    recv.read_exact(&mut buf).await?;
    Ok(buf[0])
}

async fn read_u16_le(recv: &mut RecvStream) -> Result<u16> {
    let mut buf = [0u8; 2];
    recv.read_exact(&mut buf).await?;
    Ok(u16::from_le_bytes(buf))
}

async fn read_bytes(recv: &mut RecvStream, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn handle_interactive_control(
    mut send: SendStream,
    mut recv: RecvStream,
    proxy: Arc<tokio::sync::RwLock<ProtocolProxy>>,
    session_state: Arc<Mutex<Option<InteractiveSession>>>,
    audit_db: Arc<BridgeAuditDb>,
    idle_timeout_secs: u64,
) -> Result<()> {
    let msg_type = read_u8(&mut recv).await?;
    if msg_type != 0x01 {
        write_error(&mut send, 0x03, "expected Attach message first").await?;
        return Ok(());
    }
    let payload_len = read_u16_le(&mut recv).await? as usize;
    let payload = read_bytes(&mut recv, payload_len).await?;

    let (session_name, pane_id, cols, rows, _term) = parse_attach_payload(&payload)?;

    let rp = proxy.read().await;
    let _session = match rp.get_session(&session_name).await {
        Ok(s) => s,
        Err(_) => {
            write_error(
                &mut send,
                0x01,
                &format!("session not found: {}", session_name),
            )
            .await?;
            return Ok(());
        }
    };

    let pane = match rp.get_pane(&session_name, &pane_id).await {
        Ok(p) => p,
        Err(e) => {
            write_error(&mut send, 0x02, &format!("pane not found: {}", e)).await?;
            return Ok(());
        }
    };

    let snapshot = pane.snapshot().await?;
    let raw_text = snapshot.visible_text();
    let lines: Vec<&str> = raw_text.lines().collect();
    let recent_lines = if lines.len() > SCROLLBACK_LINES {
        &lines[lines.len() - SCROLLBACK_LINES..]
    } else {
        &lines
    };
    let mut scrollback = String::new();
    scrollback.push_str("\r\n\x1b[2m--- scrollback (last 50 lines) ---\x1b[0m\r\n");
    for line in recent_lines {
        scrollback.push_str(line);
        scrollback.push_str("\r\n");
    }
    scrollback.push_str("\x1b[2m--- end of scrollback ---\x1b[0m\r\n");
    let scrollback = scrollback.into_bytes();

    pane.resize(TerminalSizeSpec::new(cols, rows)).await?;

    let exit_notify = Arc::new(Notify::new());
    {
        let mut state = session_state.lock().await;
        *state = Some(InteractiveSession {
            session_name: session_name.clone(),
            pane_id: pane_id.clone(),
            cols,
            rows,
            socket_path: rp.socket_path().to_string(),
            master_fd: None,
            child_pid: None,
            exit_code: None,
            exit_notify: exit_notify.clone(),
            recording_file: None,
        });
    }
    drop(rp);

    write_attached(&mut send, &scrollback).await?;

    let attach_time = std::time::Instant::now();
    audit_db
        .log(bridge_audit::BridgeEvent {
            event_type: "attach".to_string(),
            client_addr: String::new(),
            client_id: None,
            session_name: Some(session_name.clone()),
            pane_id: Some(pane_id.clone()),
            cols: Some(cols),
            rows: Some(rows),
            detail: None,
            duration_secs: None,
            exit_code: None,
        })
        .await;

    loop {
        let msg_result = if idle_timeout_secs > 0 {
            tokio::select! {
                r = read_u8(&mut recv) => Some(r),
                _ = exit_notify.notified() => None,
                _ = tokio::time::sleep(Duration::from_secs(idle_timeout_secs)) => {
                    tracing::warn!(
                        session = %session_name,
                        pane = %pane_id,
                        timeout_secs = idle_timeout_secs,
                        "idle timeout — disconnecting client"
                    );
                    None
                }
            }
        } else {
            tokio::select! {
                r = read_u8(&mut recv) => Some(r),
                _ = exit_notify.notified() => None,
            }
        };

        let msg_type = match msg_result {
            Some(Ok(t)) => t,
            Some(Err(_)) | None => {
                if let Some(exit_code) = session_state
                    .lock()
                    .await
                    .as_ref()
                    .and_then(|s| s.exit_code)
                {
                    write_process_exited(&mut send, exit_code).await?;
                    tracing::info!(
                        "process exited in {}/{}: code={}",
                        session_name,
                        pane_id,
                        exit_code
                    );
                }
                break;
            }
        };

        let payload_len = read_u16_le(&mut recv).await? as usize;
        let payload = read_bytes(&mut recv, payload_len).await?;

        match msg_type {
            0x02 => {
                if payload.len() < 4 {
                    tracing::warn!("resize payload too short: {} bytes", payload.len());
                    continue;
                }
                let new_cols = u16::from_le_bytes([payload[0], payload[1]]);
                let new_rows = u16::from_le_bytes([payload[2], payload[3]]);

                let state = session_state.lock().await;
                if let Some(master_fd) = state.as_ref().and_then(|s| s.master_fd.as_ref()) {
                    let winsize = libc::winsize {
                        ws_row: new_rows,
                        ws_col: new_cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    unsafe {
                        libc::ioctl(master_fd.as_raw_fd(), libc::TIOCSWINSZ, &winsize);
                    }
                    tracing::debug!("resize PTY: {}x{}", new_cols, new_rows);
                }

                pane.resize(TerminalSizeSpec::new(new_cols, new_rows))
                    .await?;
            }
            0x03 => {
                tracing::info!("client detached from {}/{}", session_name, pane_id);
                audit_db
                    .log(bridge_audit::BridgeEvent {
                        event_type: "detach".to_string(),
                        client_addr: String::new(),
                        client_id: None,
                        session_name: Some(session_name.clone()),
                        pane_id: Some(pane_id.clone()),
                        cols: None,
                        rows: None,
                        detail: None,
                        duration_secs: Some(attach_time.elapsed().as_secs_f64()),
                        exit_code: None,
                    })
                    .await;
                let state = session_state.lock().await;
                if let Some(pid) = state.as_ref().and_then(|s| s.child_pid) {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                    tracing::info!("sent SIGTERM to child pid {}", pid);
                }
                break;
            }
            _ => {
                tracing::warn!("unknown control message type: 0x{:02x}", msg_type);
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_interactive_data(
    mut send: SendStream,
    mut recv: RecvStream,
    proxy: Arc<tokio::sync::RwLock<ProtocolProxy>>,
    session_state: Arc<Mutex<Option<InteractiveSession>>>,
    recording_enabled: bool,
    recording_dir: PathBuf,
    fsync_interval_secs: u64,
    audit_db: Arc<BridgeAuditDb>,
) -> Result<()> {
    let (session_name, socket_path) = {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        loop {
            if let Some(info) = session_state.lock().await.as_ref() {
                break (info.session_name.clone(), info.socket_path.clone());
            }
            if start.elapsed() > timeout {
                anyhow::bail!("timeout waiting for control stream (0x06) to attach");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };

    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret != 0 {
        anyhow::bail!("openpty failed: {}", std::io::Error::last_os_error());
    }

    let (cols, rows, pane_id) = {
        let state = session_state.lock().await;
        let info = state.as_ref().context("session state missing")?;
        (info.cols, info.rows, info.pane_id.clone())
    };

    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(master, libc::TIOCSWINSZ, &winsize);
    }

    let master_fd = unsafe { OwnedFd::from_raw_fd(master) };

    {
        let mut state = session_state.lock().await;
        if let Some(ref mut s) = *state {
            s.master_fd = Some(master_fd.try_clone()?);
        }
    }

    let slave_fd = unsafe { OwnedFd::from_raw_fd(slave) };
    let slave_stdin = slave_fd.try_clone()?;
    let slave_stdout = slave_fd.try_clone()?;
    let slave_stderr = slave_fd;

    let mut child = tokio::process::Command::new("rmux")
        .args(["-S", &socket_path, "attach-session", "-t", &session_name])
        .env("TERM", "xterm-256color")
        .env("COLUMNS", cols.to_string())
        .env("LINES", rows.to_string())
        .stdin(std::process::Stdio::from(slave_stdin))
        .stdout(std::process::Stdio::from(slave_stdout))
        .stderr(std::process::Stdio::from(slave_stderr))
        .spawn()
        .context("failed to spawn rmux attach-session")?;

    let child_pid = child.id();
    tracing::info!(
        session = %session_name,
        socket = %socket_path,
        size = %format!("{}x{}", cols, rows),
        pid = child_pid.unwrap_or(0),
        "spawned rmux attach-session via PTY"
    );

    {
        let mut state = session_state.lock().await;
        if let Some(ref mut s) = *state {
            s.child_pid = child_pid;
        }
    }

    let flags = unsafe { libc::fcntl(master_fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        anyhow::bail!("fcntl F_GETFL failed: {}", std::io::Error::last_os_error());
    }
    let ret = unsafe {
        libc::fcntl(
            master_fd.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        )
    };
    if ret == -1 {
        anyhow::bail!("fcntl F_SETFL failed: {}", std::io::Error::last_os_error());
    }

    let async_fd = AsyncFd::new(master_fd).context("failed to create AsyncFd for PTY")?;

    // ─── Start cast recording if enabled ───
    let recorder: Option<CastRecorder> = if recording_enabled {
        let now = chrono::Utc::now();
        let date_dir = recording_dir.join(now.format("%Y-%m-%d").to_string());
        if let Err(e) = tokio::fs::create_dir_all(&date_dir).await {
            tracing::warn!("failed to create recording dir {:?}: {}", date_dir, e);
            None
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    tokio::fs::set_permissions(&date_dir, std::fs::Permissions::from_mode(0o700))
                        .await;
            }
            let epoch = now.timestamp();
            // Generate a 4-hex client id from SystemTime hash (no rand crate).
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::time::SystemTime::now().hash(&mut hasher);
            let client_id = format!("{:04x}", hasher.finish() & 0xFFFF);

            let safe_session = session_name
                .replace(['/', '\\', '\0'], "_")
                .replace("..", "_");
            let safe_pane = pane_id.replace(['/', '\\', '\0', '%'], "_");
            let filename = format!("{safe_session}_{safe_pane}_{epoch}_{client_id}.cast");
            let cast_path = date_dir.join(&filename);

            match CastRecorder::start(cast_path.clone(), cols, rows, fsync_interval_secs).await {
                Ok(rec) => {
                    tracing::info!(path = %cast_path.display(), "started cast recording");
                    // Store recording path in session state.
                    {
                        let mut state = session_state.lock().await;
                        if let Some(ref mut s) = *state {
                            s.recording_file = Some(cast_path.to_string_lossy().to_string());
                        }
                    }
                    Some(rec)
                }
                Err(e) => {
                    tracing::warn!("failed to start cast recording: {}", e);
                    None
                }
            }
        }
    } else {
        None
    };

    let quic_to_pty = async {
        let mut buf = [0u8; 4096];
        loop {
            let n = recv.read(&mut buf).await?.unwrap_or(0);
            if n == 0 {
                break;
            }

            if let Some(ref rec) = recorder {
                rec.record_input(&buf[..n]);
            }

            let mut written = 0;
            while written < n {
                let mut guard = async_fd.writable().await?;
                match guard.try_io(|inner| {
                    let fd = inner.get_ref().as_raw_fd();
                    let ret = unsafe {
                        libc::write(
                            fd,
                            buf[written..].as_ptr() as *const libc::c_void,
                            (n - written) as libc::size_t,
                        )
                    };
                    if ret < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(ret as usize)
                    }
                }) {
                    Ok(Ok(w)) => written += w,
                    Ok(Err(e)) => return Err(e.into()),
                    Err(_would_block) => continue,
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    let pty_to_quic = async {
        let mut buf = [0u8; 4096];
        loop {
            let mut guard = async_fd.readable().await?;
            let result = guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                let ret = unsafe {
                    libc::read(
                        fd,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len() as libc::size_t,
                    )
                };
                if ret < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(ret as usize)
                }
            });

            match result {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    if let Some(ref rec) = recorder {
                        rec.record_output(&buf[..n]);
                    }
                    send.write_all(&buf[..n]).await?;
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_would_block) => continue,
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    let copy_result = tokio::select! {
        r = quic_to_pty => {
            tracing::info!("QUIC→PTY finished: {:?}", r);
            r
        }
        r = pty_to_quic => {
            tracing::info!("PTY→QUIC finished: {:?}", r);
            r
        }
    };

    let status = child.wait().await?;
    let code = status.code().unwrap_or(-1);
    tracing::info!(exit_code = code, "rmux attach-session exited");

    audit_db
        .log(bridge_audit::BridgeEvent {
            event_type: "exit".to_string(),
            client_addr: String::new(),
            client_id: None,
            session_name: Some(session_name.clone()),
            pane_id: Some(pane_id.clone()),
            cols: None,
            rows: None,
            detail: None,
            duration_secs: None,
            exit_code: Some(code),
        })
        .await;

    {
        let mut state = session_state.lock().await;
        if let Some(ref mut s) = *state {
            s.exit_code = Some(code);
            s.master_fd = None;
            s.child_pid = None;
            s.exit_notify.notify_one();
        }
    }

    // ─── Finalize cast recording ───
    if let Some(rec) = recorder {
        let cast_path = rec.path().to_path_buf();
        if let Some(meta) = rec.finish(code).await {
            match finalize_cast(&cast_path, &meta).await {
                Ok(()) => {
                    tracing::info!(
                        path = %cast_path.display(),
                        sha256 = %meta.sha256,
                        size_bytes = meta.size_bytes,
                        duration_secs = meta.duration_secs,
                        "cast recording finalized"
                    );
                }
                Err(e) => {
                    tracing::warn!("failed to finalize cast {:?}: {}", cast_path, e);
                }
            }
        } else {
            tracing::warn!("cast recording returned no metadata: {:?}", cast_path);
        }
    }

    // ─── Restore pane layout on abnormal disconnect ───
    // When the client disconnects abnormally (network drop, process killed),
    // the pane may have been left at the client's terminal size. Restore with
    // even-vertical layout so other panes are not permanently squashed.
    if copy_result.is_err() {
        let state = session_state.lock().await;
        let sn = state.as_ref().map(|s| s.session_name.clone());
        let pid = state.as_ref().map(|s| s.pane_id.clone());
        drop(state);
        if let Some(ref sn) = sn {
            let result = proxy
                .read()
                .await
                .handle_select_layout(sn, 0, "even-vertical")
                .await;
            let ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            if ok {
                tracing::info!(
                    session = %sn,
                    pane = %pid.as_deref().unwrap_or("?"),
                    "layout restored after abnormal disconnect"
                );
            } else {
                tracing::warn!(
                    session = %sn,
                    result = %result,
                    "failed to restore layout after abnormal disconnect"
                );
            }
        }
    }

    copy_result?;
    Ok(())
}

fn parse_attach_payload(data: &[u8]) -> Result<(String, String, u16, u16, String)> {
    let mut offset = 0;

    if data.len() < 2 {
        anyhow::bail!("attach payload too short for session_name_len");
    }
    let session_name_len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
    offset += 2;
    if data.len() < offset + session_name_len {
        anyhow::bail!("attach payload truncated in session_name");
    }
    let session_name = String::from_utf8(data[offset..offset + session_name_len].to_vec())?;
    offset += session_name_len;

    if data.len() < offset + 1 {
        anyhow::bail!("attach payload too short for pane_id_len");
    }
    let pane_id_len = data[offset] as usize;
    offset += 1;
    if data.len() < offset + pane_id_len {
        anyhow::bail!("attach payload truncated in pane_id");
    }
    let pane_id = String::from_utf8(data[offset..offset + pane_id_len].to_vec())?;
    offset += pane_id_len;

    if data.len() < offset + 4 {
        anyhow::bail!("attach payload too short for cols/rows");
    }
    let cols = u16::from_le_bytes([data[offset], data[offset + 1]]);
    offset += 2;
    let rows = u16::from_le_bytes([data[offset], data[offset + 1]]);
    offset += 2;

    if data.len() < offset + 1 {
        anyhow::bail!("attach payload too short for term_len");
    }
    let term_len = data[offset] as usize;
    offset += 1;
    if data.len() < offset + term_len {
        anyhow::bail!("attach payload truncated in term");
    }
    let term = String::from_utf8(data[offset..offset + term_len].to_vec())?;

    Ok((session_name, pane_id, cols, rows, term))
}

async fn write_attached(send: &mut SendStream, scrollback: &[u8]) -> Result<()> {
    send.write_all(&[0x81]).await?;
    let payload_len = 4 + scrollback.len();
    send.write_all(&(payload_len as u16).to_le_bytes()).await?;
    send.write_all(&(scrollback.len() as u32).to_le_bytes())
        .await?;
    send.write_all(scrollback).await?;
    Ok(())
}

async fn write_error(send: &mut SendStream, code: u8, message: &str) -> Result<()> {
    send.write_all(&[0x82]).await?;
    let payload_len = 1 + 2 + message.len();
    send.write_all(&(payload_len as u16).to_le_bytes()).await?;
    send.write_all(&[code]).await?;
    send.write_all(&(message.len() as u16).to_le_bytes())
        .await?;
    send.write_all(message.as_bytes()).await?;
    Ok(())
}

async fn write_process_exited(send: &mut SendStream, exit_code: i32) -> Result<()> {
    send.write_all(&[0x83]).await?;
    send.write_all(&4u16.to_le_bytes()).await?;
    send.write_all(&exit_code.to_le_bytes()).await?;
    Ok(())
}
