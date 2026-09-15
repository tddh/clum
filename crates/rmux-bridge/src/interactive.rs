//! 交互式终端处理器：处理 QUIC 0x06（控制流）和 0x07（数据流）
//!
//! 设计参考：docs/terminal-state-design.md（终端状态感知与交互会话模型）

use anyhow::{Context, Result};
use quinn::{RecvStream, SendStream};
use rmux_sdk::{events::recovery::PaneRecoveryEvent, TerminalSizeSpec};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex, Notify};

use crate::bridge_audit::{self, BridgeAuditDb};
use crate::cast_recorder::{finalize_cast, CastRecorder};
use crate::protocol::ProtocolProxy;

/// attach payload 尾字节 mode：legacy mux 模式（spawn `rmux attach-session`）。
/// 缺省值——不带 mode 字节的旧 CLI 自动落入该模式。
pub const ATTACH_MODE_MUX: u8 = 0x00;
/// attach payload 尾字节 mode：raw pane 直通模式（recover_output / send_text，
/// 本地终端直接消费 pane 原始字节流，无 rmux UI 层）。
pub const ATTACH_MODE_RAW: u8 = 0x01;

/// raw 输出订阅看门狗：见 run_raw_pane_bridge 输出泵处的设计注释。
/// 档一（交互快速档）：敲键回显悬而未答超过该毫秒数 → 判死续订；
/// 档二（挂机兜底档）：无任何事件超过该毫秒数 → 兜底续订（watch/挂机场景）；
/// 两次续订的最小间隔：防 daemon 持续期坏导致的续订风暴。
/// （曾经的"渲染层心跳对照档"于 2026-09-15 移除：实测病例中零触发，
/// 每秒 snapshot 全屏文本比较的常驻成本不成立——检测方向本身存疑时
/// 不应保留高开销档位。）
const RAW_ECHO_TIMEOUT_MS: u64 = 500;
const RAW_IDLE_RESUBSCRIBE_MS: u64 = 10 * 60 * 1000;
const RAW_RESUB_MIN_INTERVAL_MS: u64 = 2_000;
/// daemon send_text IPC 的挂死死线：超时即杀连接强制客户端重连。
const SEND_TEXT_TIMEOUT: Duration = Duration::from_secs(3);

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 该模式下窗口应有的高度。
///
/// raw 直通没有 rmux UI，窗口用满整个终端；mux 下 rmux client 的可用区是
/// 「终端行数 − 状态栏行数」，窗口必须与可用区一致——否则窗口比可用区大 1 行，
/// client 只能显示窗口的前 N-1 行，shell 光标落到视口外，表现为"光标在倒数第二行"。
/// 状态栏开启时占 1 行。
fn window_rows_for(mode: u8, rows: u16) -> u16 {
    if mode == ATTACH_MODE_RAW {
        rows
    } else {
        rows.saturating_sub(1)
    }
}

/// 回显活性判定：敲键时间戳晚于最近一次输出（悬而未答）且已超过回显死线。
fn echo_deadline_exceeded(
    last_input_ms: u64,
    last_output_ms: u64,
    now_ms: u64,
    echo_timeout_ms: u64,
) -> bool {
    last_input_ms != 0
        && last_input_ms > last_output_ms
        && now_ms.saturating_sub(last_input_ms) >= echo_timeout_ms
}

/// Interactive session state shared between control (0x06) and data (0x07) streams.
pub struct InteractiveSession {
    pub session_name: String,
    pub pane_id: String,
    pub cols: u16,
    pub rows: u16,
    pub socket_path: String,
    pub mode: u8,
    /// raw 模式：pane 所在窗口索引（attach 时解析一次，resize 时复用，
    /// 避免每次 resize 都 spawn 一个 rmux 进程）。
    pub window_index: Option<u32>,
    pub master_fd: Option<OwnedFd>,
    pub child_pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub exit_notify: Arc<Notify>,
}

/// 会话状态 + 连接计数的组合。main（直连）与 register（注册）两处各自创建，
/// 避免两处重复声明相同的 Arc<Mutex<HashMap>> 类型。
pub struct SessionTracker {
    pub state: Arc<Mutex<std::collections::HashMap<String, InteractiveSession>>>,
    pub counts: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
}

impl SessionTracker {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(std::collections::HashMap::new())),
            counts: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }
}

/// 会话级活跃连接计数（跨 connection 共享，main/register 创建）。
/// 用于判断某连接断开时该会话是否还有其它活跃 client：
/// 若还有则跳过 layout restore，避免 even-vertical 重排误伤其它连接的显示。
pub struct SessionCounter {
    counts: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    session: String,
}

impl SessionCounter {
    pub fn register(
        counts: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
        session: String,
    ) -> Self {
        if let Ok(mut m) = counts.lock() {
            *m.entry(session.clone()).or_insert(0) += 1;
        }
        Self { counts, session }
    }

    /// 该会话中除自己外的活跃连接数。
    pub fn active_others(&self) -> usize {
        if let Ok(m) = self.counts.lock() {
            m.get(&self.session).copied().unwrap_or(0).saturating_sub(1)
        } else {
            0
        }
    }
}

impl Drop for SessionCounter {
    fn drop(&mut self) {
        if let Ok(mut m) = self.counts.lock() {
            if let Some(c) = m.get_mut(&self.session) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    m.remove(&self.session);
                }
            }
        }
    }
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
    session_state: Arc<Mutex<std::collections::HashMap<String, InteractiveSession>>>,
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

    let (client_id, session_name, pane_id, cols, rows, _term, mode) =
        parse_attach_payload(&payload)?;

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

    if mode == ATTACH_MODE_RAW && !rp.supports_raw_recovery().await {
        write_error(
            &mut send,
            0x02,
            "raw pane mode requires daemon capability 'sdk.pane.raw_recovery'; \
             reconnect with --mux or upgrade the rmux daemon",
        )
        .await?;
        return Ok(());
    }

    // raw 模式的屏幕重建由数据流首个 Rebase keyframe 承担（保真度高于文本拼接，
    // P0 实证 hist 全覆盖）；legacy 模式沿用 50 行文本现场恢复。
    let scrollback: Vec<u8> = if mode == ATTACH_MODE_RAW {
        Vec::new()
    } else {
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
        scrollback.into_bytes()
    };

    let window_index = rp.window_index_of_pane(&pane_id).await;
    if let Some(i) = window_index {
        if let Err(e) = rp
            .resize_window_sized(&session_name, i, cols, window_rows_for(mode, rows))
            .await
        {
            tracing::warn!("resize owning window failed: {e}");
        }
    }
    pane.resize(TerminalSizeSpec::new(cols, window_rows_for(mode, rows)))
        .await?;

    let exit_notify = Arc::new(Notify::new());
    {
        let mut state = session_state.lock().await;
        state.insert(
            client_id.clone(),
            InteractiveSession {
                session_name: session_name.clone(),
                pane_id: pane_id.clone(),
                cols,
                rows,
                socket_path: rp.socket_path().to_string(),
                mode,
                window_index,
                master_fd: None,
                child_pid: None,
                exit_code: None,
                exit_notify: exit_notify.clone(),
            },
        );
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
                    .get(&client_id)
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

                let (mode_opt, window_index) = {
                    let state = session_state.lock().await;
                    if let Some(master_fd) =
                        state.get(&client_id).and_then(|s| s.master_fd.as_ref())
                    {
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
                    match state.get(&client_id) {
                        Some(s) => (Some(s.mode), s.window_index),
                        None => (None, None),
                    }
                };
                if let (Some(m), Some(idx)) = (mode_opt, window_index) {
                    let rp = proxy.read().await;
                    if let Err(e) = rp
                        .resize_window_sized(
                            &session_name,
                            idx,
                            new_cols,
                            window_rows_for(m, new_rows),
                        )
                        .await
                    {
                        tracing::warn!("resize owning window failed: {e}");
                    }
                }

                pane.resize(TerminalSizeSpec::new(
                    new_cols,
                    window_rows_for(mode_opt.unwrap_or(ATTACH_MODE_RAW), new_rows),
                ))
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
                if let Some(pid) = state.get(&client_id).and_then(|s| s.child_pid) {
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

    // 清理本客户端的 interactive 状态（enrolled 模式多个客户端共享 map，必须按 client_id 移除）
    session_state.lock().await.remove(&client_id);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_interactive_data(
    conn: quinn::Connection,
    mut send: SendStream,
    mut recv: RecvStream,
    proxy: Arc<tokio::sync::RwLock<ProtocolProxy>>,
    session_state: Arc<Mutex<std::collections::HashMap<String, InteractiveSession>>>,
    session_counts: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    client_id: String,
    recording_enabled: bool,
    recording_dir: PathBuf,
    fsync_interval_secs: u64,
    audit_db: Arc<BridgeAuditDb>,
    recording_pubkey: Arc<tokio::sync::RwLock<Option<(String, String)>>>,
) -> Result<()> {
    let (session_name, socket_path, mode) = {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        loop {
            if let Some(info) = session_state.lock().await.get(&client_id) {
                break (
                    info.session_name.clone(),
                    info.socket_path.clone(),
                    info.mode,
                );
            }
            if start.elapsed() > timeout {
                anyhow::bail!("timeout waiting for control stream (0x06) to attach");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    // 登记本连接为该会话的活跃 client；断开时 Drop 自动 -1。
    let counter = SessionCounter::register(session_counts, session_name.clone());

    let (cols, rows, pane_id) = {
        let state = session_state.lock().await;
        let info = state.get(&client_id).context("session state missing")?;
        (info.cols, info.rows, info.pane_id.clone())
    };

    if mode == ATTACH_MODE_RAW {
        return run_raw_pane_bridge(
            conn,
            send,
            recv,
            proxy,
            session_state,
            counter,
            client_id,
            session_name,
            pane_id,
            cols,
            rows,
            recording_enabled,
            recording_dir,
            fsync_interval_secs,
            audit_db,
            recording_pubkey,
        )
        .await;
    }

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
        if let Some(s) = state.get_mut(&client_id) {
            s.master_fd = Some(master_fd.try_clone()?);
        }
    }

    let slave_fd = unsafe { OwnedFd::from_raw_fd(slave) };
    let slave_tty_name = unsafe {
        let p = libc::ttyname(slave_fd.as_raw_fd());
        if p.is_null() {
            None
        } else {
            Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
        }
    };
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
        if let Some(s) = state.get_mut(&client_id) {
            s.child_pid = child_pid;
        }
    }

    // ─── 看门狗：rmux client 被 detach 但子进程不退时主动 kill ───
    // rmux 存在"server 已 detach client（list-clients 移除），但 attach-session
    // 客户端进程不退出"的情况，导致 child.wait() 永久阻塞、0x83 发不出、cli 卡死。
    // 周期查询 list-clients，若自己的 slave tty 已不在 client 列表 → SIGKILL 子进程。
    let watchdog = {
        let socket = socket_path.clone();
        let session = session_name.clone();
        let slave_tty = slave_tty_name.clone();
        tokio::spawn(async move {
            let Some(tty) = slave_tty else { return };
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await; // 跳过立即 tick，避免 attach 尚未注册时误杀
            loop {
                interval.tick().await;
                let out = tokio::process::Command::new("rmux")
                    .args(["-S", &socket, "list-clients", "-t", &session])
                    .output()
                    .await;
                let Ok(out) = out else { continue };
                let text = String::from_utf8_lossy(&out.stdout);
                if !text.contains(&tty) {
                    tracing::info!(
                        tty = %tty,
                        pid = child_pid.unwrap_or(0),
                        "rmux client detached (process lingering), killing child"
                    );
                    if let Some(pid) = child_pid {
                        unsafe {
                            libc::kill(pid as i32, libc::SIGKILL);
                        }
                    }
                    break;
                }
            }
        })
    };

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

    let recorder: Option<CastRecorder> = make_recorder(
        recording_enabled,
        &recording_dir,
        &session_name,
        &pane_id,
        cols,
        rows,
        fsync_interval_secs,
        &recording_pubkey,
    )
    .await;

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
        if let Some(s) = state.get_mut(&client_id) {
            s.exit_code = Some(code);
            s.master_fd = None;
            s.child_pid = None;
            // 每个客户端（client_id）有自己的 exit_notify，只唤醒本客户端的 control handler。
            s.exit_notify.notify_waiters();
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
    // 若该会话还有其它活跃 client（多人同时 term），跳过恢复——
    // even-vertical 重排会误伤其它连接的显示。
    if copy_result.is_err() && counter.active_others() == 0 {
        let state = session_state.lock().await;
        let sn = state.get(&client_id).map(|s| s.session_name.clone());
        let pid = state.get(&client_id).map(|s| s.pane_id.clone());
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

    // 子进程已退出/已 kill，停止看门狗，避免 task 泄漏。
    watchdog.abort();
    copy_result?;
    Ok(())
}

fn parse_attach_payload(data: &[u8]) -> Result<(String, String, String, u16, u16, String, u8)> {
    let mut offset = 0;

    if data.len() < 2 {
        anyhow::bail!("attach payload too short for client_id_len");
    }
    let client_id_len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
    offset += 2;
    if data.len() < offset + client_id_len {
        anyhow::bail!("attach payload truncated in client_id");
    }
    let client_id = String::from_utf8(data[offset..offset + client_id_len].to_vec())?;
    offset += client_id_len;

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

    // 可选尾字节 mode：旧 CLI 不携带 → ATTACH_MODE_MUX（零成本向后兼容）
    let mode = match data.get(offset + term_len) {
        None => ATTACH_MODE_MUX,
        Some(&ATTACH_MODE_MUX) => ATTACH_MODE_MUX,
        Some(&ATTACH_MODE_RAW) => ATTACH_MODE_RAW,
        Some(&other) => {
            tracing::warn!(other, "unknown attach mode byte, falling back to mux");
            ATTACH_MODE_MUX
        }
    };

    Ok((client_id, session_name, pane_id, cols, rows, term, mode))
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

// ─── 录制初始化（legacy mux 与 raw pane 两模式共用） ───

#[allow(clippy::too_many_arguments)]
async fn make_recorder(
    recording_enabled: bool,
    recording_dir: &std::path::Path,
    session_name: &str,
    pane_id: &str,
    cols: u16,
    rows: u16,
    fsync_interval_secs: u64,
    recording_pubkey: &tokio::sync::RwLock<Option<(String, String)>>,
) -> Option<CastRecorder> {
    if !recording_enabled {
        return None;
    }
    let now = chrono::Utc::now();
    let date_dir = recording_dir.join(now.format("%Y-%m-%d").to_string());
    if let Err(e) = tokio::fs::create_dir_all(&date_dir).await {
        tracing::warn!("failed to create recording dir {:?}: {}", date_dir, e);
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            tokio::fs::set_permissions(&date_dir, std::fs::Permissions::from_mode(0o700)).await
        {
            tracing::warn!(
                "failed to set recording dir permissions {:?}: {}",
                date_dir,
                e
            );
        }
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

    let encryptor = {
        let guard = recording_pubkey.read().await;
        match guard.as_ref() {
            Some((pk, kid)) => match clum_core::crypto::RecordingEncryptor::new(pk, kid, &filename)
            {
                Ok(e) => Some(e),
                Err(e) => {
                    tracing::error!("failed to build recording encryptor, storing plaintext: {e}");
                    None
                }
            },
            None => {
                tracing::warn!(
                    "recording public key unavailable, storing plaintext (legacy server?)"
                );
                None
            }
        }
    };
    match CastRecorder::start(
        cast_path.clone(),
        cols,
        rows,
        fsync_interval_secs,
        encryptor,
    )
    .await
    {
        Ok(rec) => {
            tracing::info!(path = %cast_path.display(), "started cast recording");
            Some(rec)
        }
        Err(e) => {
            tracing::warn!("failed to start cast recording: {}", e);
            None
        }
    }
}

/// 收尾录制文件（两种模式共用的尾部逻辑）。
async fn finish_recording(recorder: Option<CastRecorder>, exit_code: i32) {
    let Some(rec) = recorder else { return };
    let cast_path = rec.path().to_path_buf();
    if let Some(meta) = rec.finish(exit_code).await {
        if let Err(e) = finalize_cast(&cast_path, &meta).await {
            tracing::warn!("failed to finalize cast {:?}: {}", cast_path, e);
            return;
        }
        tracing::info!(
            path = %cast_path.display(),
            sha256 = %meta.sha256,
            size_bytes = meta.size_bytes,
            duration_secs = meta.duration_secs,
            "cast recording finalized"
        );
    } else {
        tracing::warn!("cast recording returned no metadata: {:?}", cast_path);
    }
}

// ─── raw pane 直通模式 ───

/// 输入侧 UTF-8 重组器：QUIC chunk 边界可能把多字节 UTF-8 序列切半，
/// 尾部半个序列保留到下一块；确认非法的字节丢弃并计数。
struct Utf8Assembler {
    pending: Vec<u8>,
}

impl Utf8Assembler {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// 返回 (可发送的完整 UTF-8 文本, 本块确认丢弃的字节数)。
    fn push(&mut self, chunk: &[u8]) -> (String, usize) {
        self.pending.extend_from_slice(chunk);
        let mut out = String::new();
        let mut dropped = 0usize;
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    break;
                }
                Err(e) => {
                    let valid_up_to = e.valid_up_to();
                    if valid_up_to > 0 {
                        out.push_str(
                            // SAFETY: valid_up_to 之前必然是合法 UTF-8
                            std::str::from_utf8(&self.pending[..valid_up_to]).unwrap_or_default(),
                        );
                    }
                    match e.error_len() {
                        Some(bad) => {
                            dropped += bad;
                            self.pending.drain(..valid_up_to + bad);
                        }
                        None => {
                            // 尾部多字节序列不完整，保留等待下一块
                            self.pending.drain(..valid_up_to);
                            break;
                        }
                    }
                }
            }
        }
        (out, dropped)
    }
}

/// raw 模式数据面：pane PTY 原始字节直通（recover_output / send_text）。
/// 不 spawn `rmux attach-session`——本地终端直接消费 pane 字节流，
/// 首连/重连屏幕重建由首个 Rebase(Initial) keyframe 承担（P0 实证保真）。
///
/// 输入通道防挂死（2026-09-14 实战病例：daemon 坏状态下 send_text IPC 可静默
/// 永挂 → input_pump 停读 → server relay 背压 → CLI 流控冻死，用户敲键全丢）：
/// send_text 超过 SEND_TEXT_TIMEOUT_MS 未返回 → close 整条 QUIC 连接，
/// CLI 立即感知 Lost 并自动重连（新连接即新订阅，daemon 侧出生即健康）。
#[allow(clippy::too_many_arguments)]
async fn run_raw_pane_bridge(
    conn: quinn::Connection,
    mut send: SendStream,
    mut recv: RecvStream,
    proxy: Arc<tokio::sync::RwLock<ProtocolProxy>>,
    session_state: Arc<Mutex<std::collections::HashMap<String, InteractiveSession>>>,
    counter: SessionCounter,
    client_id: String,
    session_name: String,
    pane_id: String,
    cols: u16,
    rows: u16,
    recording_enabled: bool,
    recording_dir: PathBuf,
    fsync_interval_secs: u64,
    audit_db: Arc<BridgeAuditDb>,
    recording_pubkey: Arc<tokio::sync::RwLock<Option<(String, String)>>>,
) -> Result<()> {
    tracing::info!(
        session = %session_name,
        pane = %pane_id,
        size = %format!("{cols}x{rows}"),
        "raw pane bridge started"
    );

    let recorder = make_recorder(
        recording_enabled,
        &recording_dir,
        &session_name,
        &pane_id,
        cols,
        rows,
        fsync_interval_secs,
        &recording_pubkey,
    )
    .await;

    // attach 审计由 control (0x06) handler 统一记录，此处不重复；
    // attach_time 仅用于 exit 事件的 duration_secs。
    let attach_time = std::time::Instant::now();

    let pane = {
        let rp = proxy.read().await;
        rp.get_pane(&session_name, &pane_id)
            .await
            .context("raw mode: pane lookup failed")?
    };

    // 输入泵：QUIC 0x07 → pane.send_text。
    // 敲键时间戳（ms）与输出泵共享——回显活性检测（R-A-W）用：
    // 正常链路敲键回显几毫秒即回流（本机 IPC），敲键后长时间无输出 = 订阅已死。
    // 返回值：true = daemon 侧 send_text IPC 挂死（背压链冻结的源头，
    // 见函数头注释），由收尾路径杀连接强制客户端重连。
    let last_input_ms = Arc::new(AtomicU64::new(0));
    let last_input_for_pump = Arc::clone(&last_input_ms);
    let pane_in = pane.clone();
    let mut utf8_in = Utf8Assembler::new();
    let input_pump = async {
        let mut buf = [0u8; 4096];
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(0)) | Ok(None) | Err(_) => break false,
                Ok(Some(n)) => {
                    last_input_for_pump.store(unix_ms_now(), Ordering::Relaxed);
                    if let Some(ref rec) = recorder {
                        rec.record_input(&buf[..n]);
                    }
                    let (text, dropped) = utf8_in.push(&buf[..n]);
                    if !text.is_empty() {
                        match tokio::time::timeout(SEND_TEXT_TIMEOUT, pane_in.send_text(&text))
                            .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(_)) => break false,
                            Err(_elapsed) => {
                                tracing::error!(
                                    pane = %pane_id,
                                    timeout_ms = SEND_TEXT_TIMEOUT.as_millis() as u64,
                                    "raw mode: daemon send_text stalled — input channel dead"
                                );
                                break true;
                            }
                        }
                    }
                    if dropped > 0 {
                        tracing::warn!(
                            dropped,
                            "raw mode: dropped non-UTF-8 input bytes; \
                             use --mux if this terminal sends non-UTF-8 keys"
                        );
                    }
                }
            }
        }
    };

    // 输出泵：recover_output → QUIC + 录制。
    //
    // daemon 侧 recover_output 订阅存在静默停推的实战病例（2026-09-14
    // dns-backup 双订阅对照实证：同一时刻同一 pane——探针新订阅实时收到注入
    // 字节，bridge 既有订阅零推送且永不恢复；渲染层 capture 始终正常），
    // 且死亡是静默的（无错误/结束事件）。对策（两档检测）：
    //  * R-A-W 档：敲键悬而未答超过 RAW_ECHO_TIMEOUT → 判死续订（交互中）；
    //  * 挂机兜底档：无任何事件超过 RAW_IDLE_RESUBSCRIBE_MS 才续订（watch 场景）。
    //  * Rebase 一律不写 cast（恢复机制而非 pane 真实字节）——续订无论多少
    //    次录制零膨胀；续订节流防风暴。
    // 续订 = drop 旧流（SDK 自动 unsubscribe）→ 重新 recover_output，新流首个
    // keyframe 权威重刷屏幕；输入通路不动，无键丢失。
    let output_pane = pane.clone();
    let output_pump = async {
        let mut wire_err = false;
        let mut stream = match output_pane.recover_output().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(pane = %pane_id, "raw mode: initial subscribe failed {e}");
                return true;
            }
        };
        let mut last_output_ms = unix_ms_now();
        let mut last_resub_ms = 0u64;
        loop {
            let now = unix_ms_now();
            let li = last_input_ms.load(Ordering::Relaxed);
            let echo_stale = echo_deadline_exceeded(li, last_output_ms, now, RAW_ECHO_TIMEOUT_MS);
            let idle_stale = now.saturating_sub(last_output_ms) >= RAW_IDLE_RESUBSCRIBE_MS;
            if (echo_stale || idle_stale)
                && now.saturating_sub(last_resub_ms) >= RAW_RESUB_MIN_INTERVAL_MS
            {
                let cause = if echo_stale {
                    "echo-stale"
                } else {
                    "idle-fallback"
                };
                tracing::info!(pane = %pane_id, cause, "raw mode: resubscribing (daemon-stream heal)");
                match output_pane.recover_output().await {
                    Ok(new_stream) => {
                        stream = new_stream;
                        last_resub_ms = now;
                        last_output_ms = now;
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(pane = %pane_id, "raw mode: resubscribe failed {e}, ending stream");
                        wire_err = true;
                        break;
                    }
                }
            }
            // select 超时：悬而未答的敲键等到回显死线；否则睡到心跳/兜底三者中最近的检查点
            let echo_wait = if li > last_output_ms {
                RAW_ECHO_TIMEOUT_MS.saturating_sub(now.saturating_sub(li))
            } else {
                RAW_IDLE_RESUBSCRIBE_MS.saturating_sub(now.saturating_sub(last_output_ms))
            };
            let wait = Duration::from_millis(echo_wait.max(1));
            tokio::select! {
                r = tokio::time::timeout(wait, stream.next()) => match r {
                    Err(_elapsed) => {} // 超时返回循环头：判死/节流/续订逻辑统一在头部
                    Ok(Ok(Some(PaneRecoveryEvent::Rebase(r)))) => {
                        tracing::debug!(
                            reason = ?r.reason,
                            epoch = r.epoch,
                            kf_len = r.keyframe.len(),
                            "raw mode: rebase forwarded (not recorded: recovery, not pane bytes)"
                        );
                        // 屏幕重建转发给客户端，但不写入 cast（见块首注释）
                        if send.write_all(&r.keyframe).await.is_err() {
                            wire_err = true;
                            break;
                        }
                        last_output_ms = unix_ms_now();
                    }
                    Ok(Ok(Some(PaneRecoveryEvent::Bytes { bytes, .. }))) => {
                        if let Some(ref rec) = recorder {
                            rec.record_output(&bytes);
                        }
                        if send.write_all(&bytes).await.is_err() {
                            wire_err = true;
                            break;
                        }
                        last_output_ms = unix_ms_now();
                    }
                    Ok(Ok(Some(PaneRecoveryEvent::Lifecycle(l)))) => {
                        tracing::info!(pane = %pane_id, "raw mode: lifecycle {l:?}");
                        break;
                    }
                    Ok(Ok(Some(PaneRecoveryEvent::End(reason)))) => {
                        tracing::info!(pane = %pane_id, "raw mode: stream end {reason:?}");
                        break;
                    }
                    // PaneRecoveryEvent 非 exhaustive：未知前向兼容事件视作活性信号
                    Ok(Ok(Some(_))) => {
                        last_output_ms = unix_ms_now();
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(e)) => {
                        tracing::warn!(pane = %pane_id, "raw mode: stream error {e}");
                        break;
                    }
                },
            }
        }
        wire_err
    };

    let (wire_err, input_stalled) = tokio::select! {
        w = output_pump => (w, false),
        s = input_pump => (false, s),
    };

    // daemon send_text 挂死：杀整条连接强制客户端 Lost 重连（新订阅出生即健康）。
    // 跳过 0x83/exit_notify——这不是 pane 正常退出，CLI 不应把它当 detach。
    if input_stalled {
        tracing::error!(
            pane = %pane_id,
            "raw mode: closing QUIC connection to force client reconnect after daemon stall"
        );
        audit_db
            .log(bridge_audit::BridgeEvent {
                event_type: "exit".to_string(),
                client_addr: String::new(),
                client_id: None,
                session_name: Some(session_name.clone()),
                pane_id: Some(pane_id.clone()),
                cols: None,
                rows: None,
                detail: Some(serde_json::json!("raw-send-stalled")),
                duration_secs: Some(attach_time.elapsed().as_secs_f64()),
                exit_code: None,
            })
            .await;
        finish_recording(recorder, -1).await;
        conn.close(quinn::VarInt::from_u32(0xE001), b"daemon send_text stalled");
        return Ok(());
    }

    // pane 子进程退出（或客户端断开）：查 exit code 并唤醒 control 流发 0x83
    let exit_code = pane.info().await.ok().and_then(|i| {
        i.panes
            .first()
            .and_then(|p| p.exit_state.as_ref().and_then(|s| s.code))
    });
    {
        let mut st = session_state.lock().await;
        if let Some(s) = st.get_mut(&client_id) {
            s.exit_code = exit_code;
            s.exit_notify.notify_waiters();
        }
    }

    audit_db
        .log(bridge_audit::BridgeEvent {
            event_type: "exit".to_string(),
            client_addr: String::new(),
            client_id: None,
            session_name: Some(session_name.clone()),
            pane_id: Some(pane_id.clone()),
            cols: None,
            rows: None,
            detail: Some(if wire_err {
                serde_json::json!("raw-wire-err")
            } else {
                serde_json::json!("raw")
            }),
            duration_secs: Some(attach_time.elapsed().as_secs_f64()),
            exit_code,
        })
        .await;

    finish_recording(recorder, exit_code.unwrap_or(-1)).await;

    // 与 legacy 相同：连接异常中断时 pane 可能残留客户端终端尺寸，
    // 若该会话无其它活跃 client，恢复 even-vertical 布局
    if wire_err && counter.active_others() == 0 {
        let result = proxy
            .read()
            .await
            .handle_select_layout(&session_name, 0, "even-vertical")
            .await;
        let ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if ok {
            tracing::info!(
                session = %session_name,
                pane = %pane_id,
                "layout restored after abnormal disconnect (raw)"
            );
        } else {
            tracing::warn!(session = %session_name, result = %result, "failed to restore layout");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_rows_leave_room_for_status_bar_in_mux() {
        assert_eq!(window_rows_for(ATTACH_MODE_RAW, 33), 33);
        assert_eq!(window_rows_for(ATTACH_MODE_RAW, 1), 1);
        assert_eq!(window_rows_for(ATTACH_MODE_MUX, 33), 32);
        assert_eq!(window_rows_for(ATTACH_MODE_MUX, 1), 0);
    }

    /// 构造 attach payload：与 CLI `write_attach_request` 的 wire 格式一致。
    fn attach_payload(
        client_id: &str,
        session: &str,
        pane: &str,
        cols: u16,
        rows: u16,
        mode: Option<u8>,
    ) -> Vec<u8> {
        let term = "xterm-256color";
        let mut v = Vec::new();
        v.extend_from_slice(&(client_id.len() as u16).to_le_bytes());
        v.extend_from_slice(client_id.as_bytes());
        v.extend_from_slice(&(session.len() as u16).to_le_bytes());
        v.extend_from_slice(session.as_bytes());
        v.push(pane.len() as u8);
        v.extend_from_slice(pane.as_bytes());
        v.extend_from_slice(&cols.to_le_bytes());
        v.extend_from_slice(&rows.to_le_bytes());
        v.push(term.len() as u8);
        v.extend_from_slice(term.as_bytes());
        if let Some(m) = mode {
            v.push(m);
        }
        v
    }

    #[test]
    fn parse_attach_payload_without_mode_byte_defaults_to_mux() {
        let p = attach_payload("c1", "clum", "%0", 80, 24, None);
        let (client_id, session, pane, cols, rows, _term, mode) =
            parse_attach_payload(&p).expect("parse");
        assert_eq!(client_id, "c1");
        assert_eq!(session, "clum");
        assert_eq!(pane, "%0");
        assert_eq!((cols, rows), (80, 24));
        assert_eq!(mode, ATTACH_MODE_MUX);
    }

    #[test]
    fn parse_attach_payload_with_raw_mode_byte() {
        let p = attach_payload("c1", "clum", "%2", 100, 30, Some(ATTACH_MODE_RAW));
        let (_, _, _, _, _, _, mode) = parse_attach_payload(&p).expect("parse");
        assert_eq!(mode, ATTACH_MODE_RAW);
    }

    #[test]
    fn parse_attach_payload_unknown_mode_byte_falls_back_to_mux() {
        let p = attach_payload("c1", "clum", "%0", 80, 24, Some(0x7f));
        let (_, _, _, _, _, _, mode) = parse_attach_payload(&p).expect("parse");
        assert_eq!(mode, ATTACH_MODE_MUX);
    }

    #[test]
    fn utf8_assembler_passes_ascii_through() {
        let mut a = Utf8Assembler::new();
        let (text, dropped) = a.push(b"ls -la\r");
        assert_eq!(text, "ls -la\r");
        assert_eq!(dropped, 0);
    }

    #[test]
    fn utf8_assembler_reassembles_split_multibyte() {
        let mut a = Utf8Assembler::new();
        // "你" = E4 BD A0，块边界切成两半
        let (text, dropped) = a.push(&[0xE4, 0xBD]);
        assert_eq!(text, "");
        assert_eq!(dropped, 0);
        let (text, dropped) = a.push(&[0xA0]);
        assert_eq!(text, "你");
        assert_eq!(dropped, 0);
    }

    #[test]
    fn utf8_assembler_drops_illegal_bytes_and_keeps_going() {
        let mut a = Utf8Assembler::new();
        let (text, dropped) = a.push(b"abc\xFF\xe4\xbd\xa0");
        assert_eq!(text, "abc你");
        assert_eq!(dropped, 1);
    }

    #[test]
    fn utf8_assembler_multiple_illegal_bytes() {
        let mut a = Utf8Assembler::new();
        let (text, dropped) = a.push(b"\xFE\xFFok");
        assert_eq!(text, "ok");
        assert_eq!(dropped, 2);
    }

    #[test]
    fn echo_deadline_no_input_never_dead() {
        // 从未敲键：永不判死（挂机/无输入场景不受快速档波及）
        assert!(!echo_deadline_exceeded(
            0,
            1_000,
            100_000,
            RAW_ECHO_TIMEOUT_MS
        ));
    }

    #[test]
    fn echo_deadline_answered_input_alive() {
        // 最近输出晚于敲键（回显已回）：不算悬而未答
        let now = 200_000;
        assert!(!echo_deadline_exceeded(
            100_000,
            100_050,
            now,
            RAW_ECHO_TIMEOUT_MS
        ));
    }

    #[test]
    fn echo_deadline_pending_within_window_alive() {
        // 敲键悬而未答但仍在回显窗内：不判死
        let now = 100_300;
        assert!(!echo_deadline_exceeded(
            100_000,
            99_000,
            now,
            RAW_ECHO_TIMEOUT_MS
        ));
    }

    #[test]
    fn echo_deadline_pending_past_window_dead() {
        // 敲键悬而未答且超过回显窗：判死
        let now = 100_600;
        assert!(echo_deadline_exceeded(
            100_000,
            99_000,
            now,
            RAW_ECHO_TIMEOUT_MS
        ));
    }

    #[test]
    fn echo_deadline_zero_timestamps_not_dead() {
        // 输出时间戳为 0（attach 起始边界）且从未敲键：不判死
        assert!(!echo_deadline_exceeded(0, 0, 100_000, RAW_ECHO_TIMEOUT_MS));
    }
}
