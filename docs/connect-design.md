# `yunying-cli connect` 交互式终端连接设计方案

> **版本**: v1.2 | **日期**: 2026-07-19 | **状态**: 已实现
>
> PTY 透传（0x06/0x07 协议）已完整实现。同时新增 AI 对话面板（ratatui 交替屏 + SSE 流式输出），通过 opencode SDK `session.prompt()` 纯管道模式驱动 Sisyphus agent，agent 通过 yunying MCP 工具操作远程终端。已支持 SGR 鼠标协议（滚轮/触摸板翻页）。

---

## 一、需求背景

### 1.1 问题

当前 yunying 的所有终端操作都是 **请求-响应模式**（MCP 工具调用）：

```
AI Agent ──MCP exec──► yunying-mcp ──QUIC──► bridge ──► rmux
              │
              └── 每次调用独立，执行完返回结果
```

这适合 AI Agent 自动化操作，但无法满足以下场景：

| 场景                     | 为什么 MCP 工具不够              |
| ---------------------- | ------------------------- |
| 需要运行 vim/htop 等交互式程序   | MCP 的 `exec` 是原子操作，无法持续交互 |
| 复杂故障需要人工深入排查           | AI 判断需要人工介入时，没有便捷的接入方式    |
| 需要 Tab 补全、Ctrl+C 等终端特性 | `send_keys` 无法提供实时终端体验    |
| 多人协作排查同一问题             | 没有共享终端会话的能力               |

### 1.2 目标

新增 `yunying-cli connect` 命令，让用户从命令行 **交互式连接** 到远程 rmux 会话：

```bash
# 连接到远程主机的 rmux 会话
$ yunying-cli connect prod-web-01 --session yunying --pane %0

# 效果等同于 SSH + tmux attach，但通过 QUIC 加密通道
# 支持：实时输入输出、vim/top/htop、Ctrl+C、Tab 补全、窗口 resize
```

### 1.3 设计原则

1. **复用现有基础设施**：同一 bridge、同一认证、同一 rmux daemon
2. **接入而非创建**：connect 接入已有的 rmux pane，不创建新的执行环境
3. **人机协作**：AI Agent 和人类可以操作同一个 pane、同一个 shell
4. **会话持久化**：断开不销毁，rmux session 继续存活

---

## 二、核心概念

### 2.1 connect 的语义

`connect` 不是新建终端，而是 **接入已有的 rmux pane**：

```
你的键盘 → CLI 终端 → QUIC 加密通道 → bridge → rmux daemon → pane 里的 shell
                                                                    ↓
你的屏幕 ← CLI 终端 ← QUIC 加密通道 ← bridge ← rmux daemon ← pane 的输出
```

**和 AI Agent 用 `exec` 工具操作的是同一个 pane、同一个 shell、同一个进程空间。**

### 2.2 会话生命周期

| 事件 | 会话状态 |
|------|---------|
| `session_create` | rmux daemon 创建 session，pane 里启动 shell |
| `connect` 接入 | 本地终端代理到 pane，**不创建新 shell** |
| `connect` 断开 | pane 里的 shell **继续运行**，只是没人看了 |
| `kill_session` | rmux daemon 销毁 session，shell 被终止 |

**断开 ≠ 销毁**。和 `tmux detach` 一样，断开只是"不看了"，后台进程继续跑。

### 2.3 与 SSH 的区别

| 维度 | SSH | yunying-cli connect |
|------|-----|-------------------|
| **执行环境** | 新建一个 shell 进程 | 接入已有的 rmux pane |
| **断开后** | shell 终止，进程丢失 | shell 继续，进程存活 |
| **多人接入** | 各自独立的 shell | 可以接入同一个 pane（共享屏幕） |
| **AI 协作** | AI 无法接入你的 SSH session | AI 和人可以操作同一个 pane |
| **传输层** | TCP + SSH 协议 | QUIC + Bridge Token |
| **凭证管理** | 本地持有 SSH 密钥 | AI 端不持有服务器凭证 |

### 2.4 人机协作场景

```
时间线：
─────────────────────────────────────────────────────

AI Agent 通过 MCP exec 操作 pane %0：
  exec("ls -la") → 输出到 pane %0
  exec("cat config.yaml") → 输出到 pane %0

         ↓ 此时用户通过 connect 接入同一个 pane %0

用户通过 connect 交互式操作 pane %0：
  看到 AI 之前执行的命令和输出（scrollback 回放）
  继续在这个 shell 里操作
  环境变量、工作目录、历史记录都保留

         ↓ 用户 Ctrl+\ 断开

AI Agent 继续通过 MCP exec 操作同一个 pane %0：
  看到用户执行过的命令（在历史记录里）
  可以继续操作
```

---

## 三、社区参考实现

### 3.1 最相关的项目

| 项目                                                           | 语言           | 传输                    | 关键特性                                  | 参考价值     |
| ------------------------------------------------------------ | ------------ | --------------------- | ------------------------------------- | -------- |
| **[neosh](https://github.com/plucury/neosh)**                | Rust         | QUIC + SSH bootstrap  | session resume、token 认证、replay buffer | 架构最接近    |
| **[mish](https://github.com/amedeedaboville/mish)**          | Rust         | QUIC datagrams        | Sans-IO SSP 核心、预测回显、多客户端              | 协议设计最完整  |
| **[gmux ADR-0004](https://github.com/gmuxapp/gmux)**         | Go           | WebSocket             | 极简协议（raw bytes + JSON resize）         | 最简协议参考   |
| **[Quil](https://github.com/artyomsv/quil)**                 | Go           | Unix socket IPC       | Ghost Buffer 回放、版本握手、MCP 集成           | 回放机制参考   |
| **[VS Code RemotePty](https://github.com/microsoft/vscode)** | TS           | SSH tunnel            | start/detach/input/resize/ack 五原语     | API 语义参考 |
| **[sshx](https://github.com/ekzhang/sshx)**                  | Rust         | WebSocket + E2EE      | broadcast 多客户端、滚动缓冲区                  | 多客户端参考   |
| **[VROOM-Terminal](https://github.com/deftai/vroom)**        | Python       | WebSocket/WebRTC/QUIC | TLV + CBOR 协议、能力协商                    | 形式化规范参考  |
| **[ttyd](https://github.com/tsl0922/ttyd)**                  | C + xterm.js | WebSocket             | PTY → WebSocket 桥接，广泛使用               | 生产级参考    |

### 3.2 关键设计决策（从社区提炼）

| 决策点 | 社区主流选择 | 我们的选择 | 理由 |
|--------|------------|-----------|------|
| **控制面 vs 数据面** | 分离（VROOM、gmux、mish） | 分离（两个 QUIC 流） | 控制命令不应被数据阻塞 |
| **数据编码** | 原始字节（gmux、neosh、sshx） | 原始字节 | 终端 I/O 就是字节流，不需要额外编码 |
| **Resize 协议** | JSON text（gmux）或 binary prefix（VROOM） | 控制流上的二进制帧 | 紧凑且类型安全 |
| **历史回放** | Ghost Buffer（Quil、VS Code、sshx） | bridge 端环形缓冲区 | 重连时立即看到上下文 |
| **认证** | Token + TLS（neosh、yunying 现有） | 复用现有 bridge token | 无需新认证机制 |
| **会话持久化** | 由 tmux/rmux daemon 管理 | 复用 rmux session | rmux 天然支持 detach/attach |

---

## 四、架构设计

### 4.1 整体架构

```
┌─────────────────────────────────────────────────────────────────┐
│                      yunying-cli connect                          │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌──────────────┐    QUIC/TLS     ┌──────────────┐             │
│  │  yunying   │ ═══════════════ │ rmux-bridge  │             │
│  │  CLI (新增)   │                 │ (改动)        │             │
│  │              │                 │              │             │
│  │ ┌──────────┐ │  控制流 (0x06)  │ ┌──────────┐ │             │
│  │ │ crossterm│ │ ◄────────────── │ │ control  │ │             │
│  │ │ raw mode │ │  resize/signal  │ │ handler  │ │             │
│  │ └────┬─────┘ │                 └────┬─────┘ │             │
│  │      │       │                      │       │             │
│  │ ┌────┴─────┐ │  数据流 (0x07)  ┌────┴─────┐ │  ┌────────┐│
│  │ │ stdin/out│ │ ◄═══════════════│ PTY relay│ │◄─┤ rmux   ││
│  │ │ forward  │ │  raw PTY bytes  │ (双向)    │ │  │ daemon ││
│  │ └──────────┘ │                 └──────────┘ │  └────────┘│
│  └──────────────┘                 └──────────────┘             │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### 4.2 与现有架构的关系

```
现有 MCP 工具（请求-响应）：
  AI Agent ──MCP──► yunying-mcp ──QUIC 0x01──► bridge ──► rmux
  特点：每次调用独立，exec/capture_pane 是原子操作

新增 connect 命令（交互式长连接）：
  用户 ──CLI──► yunying CLI ──QUIC 0x06/0x07──► bridge ──► rmux
  特点：持续双向流，实时 PTY 转发

两者共享：
  • 同一 bridge 进程（端口 9778）
  • 同一认证机制（bridge token）
  • 同一 TLS 证书体系
  • 同一 rmux daemon
```

### 4.3 与 AI Agent MCP 工具的能力矩阵

```
┌─────────────────────────────────────────────────────────┐
│                    yunying 完整能力矩阵                  │
├─────────────────────────────────────────────────────────┤
│                                                         │
│  AI Agent 使用（MCP 工具）        人类使用（CLI 命令）     │
│  ┌─────────────────────┐    ┌─────────────────────┐    │
│  │ • exec              │    │ • yunying-cli connect  │    │
│  │ • capture_pane      │    │ • yunying upload   │    │
│  │ • send_keys         │    │ • yunying download │    │
│  │ • file_upload       │    │ • yunying tunnel   │    │
│  │ • batch_exec        │    │                      │    │
│  │ • ...（67 个工具）   │    │                      │    │
│  └─────────────────────┘    └─────────────────────┘    │
│                                                         │
│  共享基础设施：                                           │
│  • 同一 bridge（端口 9778）                               │
│  • 同一认证（bridge token）                               │
│  • 同一 TLS 证书                                         │
│  • 同一审计系统                                           │
│  • 同一 rmux daemon                                      │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

---

## 五、协议设计

### 5.1 QUIC 流类型分配

| Magic | 类型 | 方向 | 说明 | 状态 |
|-------|------|------|------|------|
| `0x01` | JSON 协议帧 | 双向 | 请求-响应（MCP 工具） | 已用 |
| `0x02` | 文件上传 | 双向 | 客户端 → 服务端 | 已用 |
| `0x03` | 文件下载 | 单向 | 服务端 → 客户端 | 已用 |
| `0x05` | 端口隧道 | 双向 | TCP ↔ QUIC relay | 已用 |
| **`0x06`** | **交互控制** | **双向** | **resize、signal、协商** | **新增** |
| **`0x07`** | **交互数据** | **双向** | **原始 PTY 字节流** | **新增** |

### 5.2 为什么分为两个流

| 方案 | 优点 | 缺点 |
|------|------|------|
| 单流（控制 + 数据混合） | 简单 | 控制消息可能被大数据阻塞 |
| **双流（0x06 控制 + 0x07 数据）** | 控制消息始终低延迟 | 需要管理两个流 |

**选择双流**，参考 VROOM-Terminal 的 control/pty 分离设计。resize 等控制消息必须即时送达，不能被 PTY 输出数据阻塞。

### 5.3 控制流协议（0x06）

控制流使用 TLV（Type-Length-Value）帧格式：

```
┌──────────┬──────────┬──────────────────┐
│ type: u8 │ len: u16 │ payload: [u8]    │
│          │ (LE)     │                  │
└──────────┴──────────┴──────────────────┘
```

#### 客户端 → 服务端消息

```rust
enum ClientControl {
    /// 连接建立（第一条消息）
    /// type = 0x01
    Attach {
        session_name: String,   // LE u16 长度前缀 + UTF-8
        pane_id: String,        // LE u8 长度前缀 + UTF-8
        initial_cols: u16,
        initial_rows: u16,
        term: String,           // 终端类型，如 "xterm-256color"
    },
    
    /// 终端尺寸变化
    /// type = 0x02
    Resize {
        cols: u16,
        rows: u16,
    },
    
    /// 断开连接（优雅关闭）
    /// type = 0x03, len = 0
    Detach,
}
```

#### 服务端 → 客户端消息

```rust
enum ServerControl {
    /// 连接确认
    /// type = 0x81
    Attached {
        scrollback_len: u32,    // Ghost Buffer 长度
        scrollback: Vec<u8>,    // 最近 N 行历史输出
    },
    
    /// 错误
    /// type = 0x82
    Error {
        code: u8,               // 0x01=session not found
                                // 0x02=pane not found
                                // 0x03=protocol error (unexpected message type, etc.)
                                // 0x04=auth failed
                                // 0x05=already attached (readonly)
        message: String,
    },
    
    /// 进程退出
    /// type = 0x83
    ProcessExited {
        exit_code: i32,
    },
}
```

#### 消息类型编码表

| Type 值 | 方向 | 消息 | 说明 |
|---------|------|------|------|
| `0x01` | C→S | Attach | 请求接入 pane |
| `0x02` | C→S | Resize | 终端尺寸变化 |
| `0x03` | C→S | Detach | 优雅断开 |
| `0x81` | S→C | Attached | 连接确认 + scrollback |
| `0x82` | S→C | Error | 错误响应 |
| `0x83` | S→C | ProcessExited | 进程退出通知 |

### 5.4 数据流协议（0x07）

数据流是 **纯原始字节**，无帧头，无长度前缀：

```
客户端 → 服务端：键盘输入的原始字节（ANSI 转义序列等）
服务端 → 客户端：PTY 输出的原始字节（包含 ANSI 颜色、光标移动等）
```

**为什么不用帧**：终端 I/O 是连续的字节流，加帧会增加延迟和复杂度。QUIC 本身保证了可靠有序传输。参考 gmux、neosh、sshx 的设计。

### 5.5 连接建立流程

```
客户端                              Bridge                     rmux daemon
  │                                   │                           │
  │── QUIC connect (TLS 1.3) ────────►│                           │
  │                                   │                           │
  │── accept_bi() → 认证流 ──────────►│                           │
  │── AUTH frame (token) ────────────►│                           │
  │◄── "OK\n" ───────────────────────│                           │
  │                                   │── Unix socket connect ───►│
  │                                   │◄── connected ─────────────│
  │                                   │                           │
  │── open_bi() → 控制流 ────────────►│                           │
  │── [0x06] (stream type) ──────────►│                           │
  │── Attach{session,pane,cols,rows}─►│                           │
  │                                   │── get_pane(session,pane)─►│
  │                                   │── output_stream() ───────►│
  │                                   │── snapshot() ────────────►│
  │◄── Attached{scrollback} ─────────│                           │
  │                                   │                           │
  │── open_bi() → 数据流 ────────────►│                           │
  │── [0x07] (stream type) ──────────►│                           │
  │                                   │                           │
  │════════ 双向 PTY 字节流转发 ════════════════════════════════│
  │                                   │                           │
  │── Resize{cols,rows} ─────────────►│  (控制流，随时发送)       │
  │                                   │── pane.resize() ────────►│
  │                                   │                           │
  │── Detach ────────────────────────►│                           │
  │                                   │── 清理资源               │
  │                                   │                           │
```

**关键流程说明**：

1. **QUIC 连接建立**：客户端与 bridge 进行 TLS 1.3 握手
2. **认证**：客户端打开第一个 bidi stream，发送 AUTH frame（`AUTH` + token_len + token），bridge 验证后返回 `OK\n`
3. **连接 rmux**：bridge 认证通过后，通过 Unix socket 连接到本地 rmux daemon（无需认证）
4. **打开控制流**：客户端打开第二个 bidi stream，写入 `0x06` 作为 stream type，然后发送 Attach 消息
5. **打开数据流**：客户端打开第三个 bidi stream，写入 `0x07` 作为 stream type
6. **双向转发**：控制流处理 resize/detach 等控制消息，数据流转发 PTY 输入输出

### 5.6 字节级协议示例

以 `yunying-cli connect prod-web-01 --session yunying --pane %0` 为例：

```
=== QUIC 连接建立 ===
Client → Bridge: TLS 1.3 握手
Client → Bridge: AUTH frame (bridge token)
Bridge → Client: "OK\n"

=== 控制流 (stream type 0x06) ===
Client → Bridge:
  [0x06]                              # stream type: interactive control
  [0x01]                              # message type: Attach
  [0x23 0x00]                         # payload length: 35 bytes (9+2+2+1+2+2+16+1)
  [0x09 0x00]                         # session_name length: 9
  "yunying"                         # session_name (UTF-8)
  [0x02]                              # pane_id length: 2
  "%0"                                # pane_id (UTF-8)
  [0x50 0x00]                         # initial_cols: 80
  [0x18 0x00]                         # initial_rows: 24
  [0x10]                              # term length: 16
  "xterm-256color"                    # term (UTF-8)

Bridge → Client:
  [0x81]                              # message type: Attached
  [0x04 0x01]                         # payload length: 260 bytes (scrollback_len u32 + 256B data, u16 LE)
  [0x00 0x01 0x00 0x00]              # scrollback_len: 256
  [scrollback data: 256 bytes]        # Ghost Buffer

=== 数据流 (stream type 0x07) ===
Client → Bridge:
  [0x07]                              # stream type: interactive data
  [6C 73 20 2D 6C 61 0D]            # "ls -la\r" (键盘输入)

Bridge → Client:
  [total 1234 bytes]                  # PTY 输出（ls 的结果）

=== 终端 resize ===
Client → Bridge (控制流):
  [0x02]                              # message type: Resize
  [0x04 0x00]                         # payload length: 4
  [0x80 0x00]                         # cols: 128
  [0x30 0x00]                         # rows: 48

=== 断开 ===
Client → Bridge (控制流):
  [0x03]                              # message type: Detach
  [0x00 0x00]                         # payload length: 0

=== 进程退出 ===
Bridge → Client (控制流):
  [0x83]                              # message type: ProcessExited
  [0x04 0x00]                         # payload length: 4
  [0x00 0x00 0x00 0x00]              # exit_code: 0 (i32 LE)
```

---

## 六、实现方案

### 6.1 代码改动清单

| 组件 | 文件 | 改动类型 | 代码量 | 说明 |
|------|------|---------|--------|------|
| **Bridge** | `crates/rmux-bridge/src/files.rs` | 修改 | ~10 行 | `handle_quic_stream()` 新增 `0x06`/`0x07` 分支 |
| **Bridge** | `crates/rmux-bridge/src/interactive.rs` | 新增 | ~200 行 | 交互式终端处理器 |
| **CLI** | `crates/yunying-cli/Cargo.toml` | 新增 | ~20 行 | 新 crate 配置 |
| **CLI** | `crates/yunying-cli/src/main.rs` | 新增 | ~80 行 | CLI 入口（clap） |
| **CLI** | `crates/yunying-cli/src/connect.rs` | 新增 | ~250 行 | 交互式连接核心逻辑 |
| **CLI** | `crates/yunying-cli/src/terminal.rs` | 新增 | ~80 行 | 本地终端管理 |
| **CLI** | `crates/yunying-cli/src/protocol.rs` | 新增 | ~100 行 | 控制流/数据流协议编解码 |
| **总计** | | | **~740 行** | |

### 6.2 Bridge 端实现

#### 6.2.1 流类型分发（修改 `files.rs`）

```rust
// crates/rmux-bridge/src/files.rs
// 在 handle_quic_stream() 的 match 中新增：
//
// 注意：0x06 和 0x07 流共享同一个 InteractiveSession（Arc<Mutex<Option<>>>）。
// 0x06 在 Attach 完成后写入 session_name/pane_id，0x07 等待写入完成后读取。
// 调用方（main.rs）需要创建共享状态并传入 handle_quic_stream。

pub async fn handle_quic_stream(
    send: SendStream,
    mut recv: RecvStream,
    proxy: std::sync::Arc<ProtocolProxy>,
    session_state: std::sync::Arc<tokio::sync::Mutex<Option<InteractiveSession>>>,
) {
    let mut type_buf = [0u8; 1];
    if recv.read_exact(&mut type_buf).await.is_err() {
        return;
    }
    match type_buf[0] {
        0x01 => { /* 已有：JSON 协议帧 */ }
        0x02 => handle_upload_quic(send, recv).await,
        0x03 => handle_download_quic(send, recv).await,
        0x05 => handle_tunnel_quic(send, recv).await,
         // 0x06 和 0x07 共享同一个 InteractiveSession 状态（通过 Arc<Mutex<>> 传递）
        // 0x06 先于 0x07 到达，在 handle_interactive_control 中完成 Attach 后将
        // session_name/pane_id 写入共享状态，0x07 的 handle_interactive_data 从中读取。
        // 必须在 handle_quic_stream 分发层保证：0x07 等待 0x06 的 Attach 完成后再开始转发。
        0x06 => handle_interactive_control(send, recv, proxy.clone(), session_state.clone()).await,
        0x07 => handle_interactive_data(send, recv, proxy, session_state).await,
        t => {
            tracing::warn!("unknown QUIC stream type: 0x{:02x}", t);
        }
    }
}
```

#### 6.2.2 交互式终端处理器（`interactive.rs`）

> **注**：实际实现与以下设计有重大差异。设计阶段使用 rmux SDK 的 `pane.output_stream()` + `pane.send_key()` 通过 Unix socket 直接转发，但实际实现改用 `libc::openpty()` 创建本地 PTY + spawn `rmux attach-session` 子进程方案。以下为设计参考，实际实现见 `crates/rmux-bridge/src/interactive.rs`。

```rust
// crates/rmux-bridge/src/interactive.rs
//
// 参考：
// - files.rs::handle_tunnel_quic() — QUIC 双向 relay 模式
// - protocol.rs::subscribe_pane_output() — rmux output_stream API

use anyhow::{Context, Result};
use quinn::{RecvStream, SendStream};
use rmux_sdk::TerminalSizeSpec;
use tokio::sync::{Mutex, Notify};
use std::sync::Arc;

use crate::protocol::ProtocolProxy;

// 注意：以下 API 需要在 ProtocolProxy 中新增公开方法：
// - proxy.get_session(name) → 检查 session 是否存在并返回 session 对象
// - proxy.get_pane(session_name, pane_id) → 获取 pane 对象（已部分存在）
// 以下辅助函数需要在 interactive.rs 中实现（或从 protocol.rs 复用）：
// - read_u8 / read_u16_le / read_bytes — QUIC stream 基本读取
// - write_attached / write_error — 协议帧编码

/// 交互式会话状态（在控制流和数据流之间共享）
pub struct InteractiveSession {
    // 注：实际实现与设计有差异。设计阶段使用 ProtocolProxy，实际实现使用 openpty +
    // rmux attach-session 子进程方案（见 interactive.rs），以下字段为实际代码中的结构：
    pub socket_path: String,          // rmux socket 路径
    pub session_name: String,
    pub pane_id: String,
    pub cols: u16,
    pub rows: u16,
    pub master_fd: Option<OwnedFd>,   // PTY master fd（用于数据转发）
    pub child_pid: Option<u32>,       // rmux attach-session 子进程 PID
    pub exit_code: Option<i32>,
    pub exit_notify: std::sync::Arc<Notify>,
}

/// Ghost Buffer 大小（字节）
/// 注意：受 TLV 帧 u16 负载长度限制（最大 65535），减去 scrollback_len(u32=4字节)，
/// scrollback 保留最近 50 行终端输出。
const SCROLLBACK_LINES: usize = 50;

/// 进程退出通知器（在创建 session_state 时构造，被 InteractiveSession 引用）
fn create_exit_notify() -> std::sync::Arc<Notify> {
    std::sync::Arc::new(Notify::new())
}

/// 处理交互式控制流 (0x06)
pub async fn handle_interactive_control(
    mut send: SendStream,
    mut recv: RecvStream,
    proxy: std::sync::Arc<ProtocolProxy>,
    session_state: std::sync::Arc<tokio::sync::Mutex<Option<InteractiveSession>>>,
) -> Result<()> {
    // 1. 读取 Attach 消息
    let msg_type = read_u8(&mut recv).await?;
    if msg_type != 0x01 {
        write_error(&mut send, 0x03, "expected Attach message first").await?;
        return Ok(());
    }
    let payload_len = read_u16_le(&mut recv).await? as usize;
    let payload = read_bytes(&mut recv, payload_len).await?;
    
    let (session_name, pane_id, cols, rows, _term) = parse_attach_payload(&payload)?;
    
    // 2. 先验证 session 是否存在
    let session = match proxy.get_session(&session_name).await {
        Ok(s) => s,
        Err(_) => {
            write_error(&mut send, 0x01, &format!("session not found: {}", session_name)).await?;
            return Ok(());
        }
    };
    
    // 3. 验证 pane 存在
    let pane = match session.get_pane(&pane_id).await {
        Ok(p) => p,
        Err(e) => {
            write_error(&mut send, 0x02, &format!("pane not found: {}", e)).await?;
            return Ok(());
        }
    };
    
    // 4. 获取快照作为 Ghost Buffer
    let snapshot = pane.snapshot().await?;
    let scrollback = snapshot.visible_text().into_bytes();
    let scrollback_len = scrollback.len().min(SCROLLBACK_LINES);
    let scrollback = &scrollback[scrollback.len() - scrollback_len..];
    
    // 5. 设置初始终端尺寸
    pane.resize(TerminalSizeSpec::new(cols, rows)).await?;
    
    // 6. 写入共享状态，唤醒等待中的数据流 (0x07)
    {
        let exit_notify = create_exit_notify();
        let mut state = session_state.lock().await;
        *state = Some(InteractiveSession {
            proxy: proxy.clone(),  // Arc clone（引用计数递增）
            session_name: session_name.clone(),
            pane_id: pane_id.clone(),
            cols,
            rows,
            exit_code: None,
            exit_notify: exit_notify.clone(),
        });
    }
    
    // 7. 发送 Attached 确认
    write_attached(&mut send, scrollback).await?;
    
    // 8. 持续处理控制消息（同时监控进程退出）
    // 提取 exit_notify 引用，避免每次迭代都 lock session_state
    let exit_notify = {
        session_state.lock().await.as_ref()
            .map(|s| s.exit_notify.clone())
    };
    
    loop {
        // 同时等待：控制消息 或 进程退出通知
        let msg_result = if let Some(ref notify) = exit_notify {
            tokio::select! {
                r = read_u8(&mut recv) => Some(r),
                _ = notify.notified() => None,  // exit_code 已写入，跳出循环
            }
        } else {
            Some(read_u8(&mut recv).await)
        };
        
        let msg_type = match msg_result {
            Some(Ok(t)) => t,
            Some(Err(_)) | None => {
                // 流关闭或进程退出：检查是否有 exit_code 需要发送
                if let Some(exit_code) = session_state.lock().await.as_ref().and_then(|s| s.exit_code) {
                    write_process_exited(&mut send, exit_code).await?;
                    tracing::info!("process exited in {}/{}: code={}", session_name, pane_id, exit_code);
                }
                break;
            }
        };
        let payload_len = read_u16_le(&mut recv).await? as usize;
        let payload = read_bytes(&mut recv, payload_len).await?;
        
        match msg_type {
            0x02 => {
                // Resize
                let new_cols = u16::from_le_bytes([payload[0], payload[1]]);
                let new_rows = u16::from_le_bytes([payload[2], payload[3]]);
                pane.resize(TerminalSizeSpec::new(new_cols, new_rows)).await?;
                tracing::debug!("resize: {}x{}", new_cols, new_rows);
            }
            0x03 => {
                // Detach
                tracing::info!("client detached from {}/{}", session_name, pane_id);
                break;
            }
            _ => {
                tracing::warn!("unknown control message type: 0x{:02x}", msg_type);
            }
        }
    }
    
    Ok(())
}

/// 处理交互式数据流 (0x07)
/// 
/// 从共享状态 session_state 中读取 session_name/pane_id（由 0x06 控制流写入）。
/// 如果 0x06 尚未完成 Attach，则等待。
/// 参考 files.rs::handle_tunnel_quic() 的 tokio::try_join! 双向 relay 模式
pub async fn handle_interactive_data(
    mut send: SendStream,
    mut recv: RecvStream,
    proxy: std::sync::Arc<ProtocolProxy>,
    session_state: std::sync::Arc<tokio::sync::Mutex<Option<InteractiveSession>>>,
) -> Result<()> {
    // 等待 0x06 控制流完成 Attach 并写入共享状态（最多等待 30 秒）
    let session_info = {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        loop {
            if let Some(info) = session_state.lock().await.as_ref() {
                break (info.session_name.clone(), info.pane_id.clone());
            }
            if start.elapsed() > timeout {
                anyhow::bail!("timeout waiting for control stream (0x06) to attach");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    let (session_name, pane_id) = session_info;
    
    let pane = proxy.get_pane(&session_name, &pane_id).await?;
    let mut output_stream = pane.output_stream().await?;
    
    // 双向 relay（参考 handle_tunnel_quic 的 tokio::try_join! 模式）
    let input_to_pty = async {
        let mut buf = [0u8; 4096];
        loop {
            match recv.read(&mut buf).await? {
                Some(n) => {
                    let keys = String::from_utf8_lossy(&buf[..n]);
                    pane.send_key(&keys).await?;
                }
                None => break, // 客户端断开
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    
    let pty_to_output = async {
        use futures::StreamExt;
        while let Some(chunk) = output_stream.next().await {
            match chunk {
                PaneOutputChunk::Bytes { bytes, .. } => {
                    send.write_all(&bytes).await?;
                }
                PaneOutputChunk::Lag(_) => continue,
                _ => continue,
            }
        }
        // 进程退出：写入退出码并唤醒控制流 (0x06)
        // 注：exit_code 的具体值需从 rmux pane 状态获取（如 pane.wait_exit()），
        // 此处简化为退出码 0（Phase 2 完善）
        {
            let mut state = session_state.lock().await;
            if let Some(ref mut s) = *state {
                s.exit_code = Some(0);
                s.exit_notify.notify_one();  // 唤醒控制流
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    
    tokio::try_join!(input_to_pty, pty_to_output)?;
    Ok(())
}

// === 协议编解码辅助函数 ===

fn parse_attach_payload(data: &[u8]) -> Result<(String, String, u16, u16, String)> {
    let mut offset = 0;
    
    let session_name_len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
    offset += 2;
    let session_name = String::from_utf8(data[offset..offset + session_name_len].to_vec())?;
    offset += session_name_len;
    
    let pane_id_len = data[offset] as usize;
    offset += 1;
    let pane_id = String::from_utf8(data[offset..offset + pane_id_len].to_vec())?;
    offset += pane_id_len;
    
    let cols = u16::from_le_bytes([data[offset], data[offset + 1]]);
    offset += 2;
    let rows = u16::from_le_bytes([data[offset], data[offset + 1]]);
    offset += 2;
    
    let term_len = data[offset] as usize;
    offset += 1;
    let term = String::from_utf8(data[offset..offset + term_len].to_vec())?;
    
    Ok((session_name, pane_id, cols, rows, term))
}

async fn write_attached(send: &mut SendStream, scrollback: &[u8]) -> Result<()> {
    send.write_all(&[0x81]).await?; // message type: Attached
    let payload_len = 4 + scrollback.len();
    send.write_all(&(payload_len as u16).to_le_bytes()).await?;
    send.write_all(&(scrollback.len() as u32).to_le_bytes()).await?;
    send.write_all(scrollback).await?;
    Ok(())
}

async fn write_error(send: &mut SendStream, code: u8, message: &str) -> Result<()> {
    send.write_all(&[0x82]).await?; // message type: Error
    let payload_len = 1 + 2 + message.len();
    send.write_all(&(payload_len as u16).to_le_bytes()).await?;
    send.write_all(&[code]).await?;
    send.write_all(&(message.len() as u16).to_le_bytes()).await?;
    send.write_all(message.as_bytes()).await?;
    Ok(())
}

async fn write_process_exited(send: &mut SendStream, exit_code: i32) -> Result<()> {
    send.write_all(&[0x83]).await?; // message type: ProcessExited
    send.write_all(&4u16.to_le_bytes()).await?; // payload length: 4 (i32)
    send.write_all(&exit_code.to_le_bytes()).await?;
    Ok(())
}
```

### 6.3 CLI 客户端实现

#### 6.3.1 新增 crate 结构

```
crates/yunying-cli/
├── Cargo.toml
└── src/
    ├── main.rs          # CLI 入口（clap）
    ├── connect.rs       # 交互式连接核心逻辑
    ├── terminal.rs      # 本地终端管理（crossterm raw mode）
    └── protocol.rs        # 控制流/数据流协议编解码
```

#### 6.3.2 Cargo.toml

```toml
[package]
name = "yunying-cli"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "yunying-cli"
path = "src/main.rs"

[dependencies]
yunying-core = { path = "../yunying-core" }
anyhow = "1"
clap = { version = "4", features = ["derive"] }
crossterm = { version = "0.29", features = ["event-stream"] }
quinn = "0.11"
rustls = "0.23"
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = "0.3"
futures = "0.3"
```

#### 6.3.3 CLI 入口（`main.rs`）

```rust
// crates/yunying-cli/src/main.rs

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "yunying-cli", about = "AI Agent 远程运维 CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    
    /// 主机配置文件路径
    #[arg(long, default_value = "~/.yunying/hosts.yaml")]
    hosts_file: String,
    
    /// CA 证书路径（用于 TLS 验证）
    #[arg(long, default_value = "~/.yunying/ca.crt")]
    ca_cert: String,
}

#[derive(Subcommand)]
enum Commands {
    /// 交互式连接到远程 rmux 会话
    Connect {
        /// 目标主机名（在 hosts.yaml 中定义）
        host: String,
        
        /// rmux session 名称
        #[arg(long, default_value = "yunying")]
        session: String,
        
        /// pane ID
        #[arg(long)]  // 可选，不指定时自动选择最小编号 pane
        pane: String,
        
        /// 只读模式（不发送输入）
        #[arg(long)]
        readonly: bool,
    },
    
    /// 列出可连接的会话和 pane
    List {
        /// 目标主机名
        host: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Connect { host, session, pane, readonly } => {
            let config = load_host_config(&cli.hosts_file, &host)?;
            crate::connect::connect(&config, &cli.ca_cert, &session, &pane, readonly).await?;
        }
        Commands::List { host } => {
            let config = load_host_config(&cli.hosts_file, &host)?;
            crate::connect::list_sessions(&config, &cli.ca_cert).await?;
        }
    }
    
    Ok(())
}
```

#### 6.3.4 交互式连接核心（`connect.rs`）

```rust
// crates/yunying-cli/src/connect.rs
//
// 参考：
// - crossterm raw mode 模式：直接转发字节，不画 UI
// - zellij-client/src/os_input_output.rs — 类似的 raw 终端转发
// - neosh 的 session resume 机制

use yunying_core::HostConfig;
use anyhow::{Context, Result};
use crossterm::terminal::{enable_raw_mode, disable_raw_mode};
use serde_json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// 注意：以下函数在 protocol.rs 中定义，此处仅引用
// write_attach_request, read_attached_response, write_resize, write_detach
// send_json_frame, recv_json_frame

/// 建立到 bridge 的 QUIC 连接（TLS 1.3 + AUTH frame 认证）
///
/// 流程：QUIC connect → TLS 握手 → open_bi() → 发送 AUTH frame → 等待 "OK\n"
/// ca_cert_path 来自 CLI --ca-cert 参数，非 HostConfig 字段。
async fn connect_to_bridge(
    bridge_addr: &str,
    bridge_token: &str,
    ca_cert_path: &str,
) -> anyhow::Result<quinn::Connection> {
    // TODO: 实现 QUIC 连接 + AUTH 认证
    // 参考：现有 MCP server 中的 transport 模块
    todo!("implement QUIC connection to bridge")
}

/// 交互式连接到远程 rmux 会话
pub async fn connect(
    config: &HostConfig,
    ca_cert_path: &str,
    session_name: &str,
    pane_id: &str,
    readonly: bool,
) -> Result<()> {
    // 1. 建立 QUIC 连接（复用 transport 逻辑）
    let conn = connect_to_bridge(&config.bridge_addr, &config.bridge_token, ca_cert_path)
        .await
        .context("failed to connect to bridge")?;
    
    // 2. 获取本地终端尺寸
    let (cols, rows) = crossterm::terminal::size()
        .context("failed to get terminal size")?;
    
    // 3. 打开控制流（0x06）
    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await?;
    ctrl_send.write_all(&[0x06]).await?; // stream type
    write_attach_request(&mut ctrl_send, session_name, pane_id, cols, rows).await?;
    
    // 4. 等待 Attached 确认 + scrollback
    let scrollback = read_attached_response(&mut ctrl_recv).await?;
    
    // 5. 打开数据流（0x07）
    let (mut data_send, mut data_recv) = conn.open_bi().await?;
    data_send.write_all(&[0x07]).await?; // stream type
    
    // 6. 设置本地终端为 raw 模式
    enable_raw_mode()?;
    
    // 7. 先输出 scrollback（Ghost Buffer 回放）
    if !scrollback.is_empty() {
        let mut stdout = tokio::io::stdout();
        stdout.write_all(&scrollback).await?;
        stdout.flush().await?;
    }
    
    // 8. 三路并发
    let ctrl_send = std::sync::Arc::new(tokio::sync::Mutex::new(ctrl_send));
    
    let result = if readonly {
        // 只读模式：不发送 stdin
        tokio::select! {
            r = quic_to_stdout(&mut data_recv) => r,
            r = resize_watcher(ctrl_send.clone()) => r,
        }
    } else {
        tokio::select! {
            r = stdin_to_quic(&mut data_send) => r,
            r = quic_to_stdout(&mut data_recv) => r,
            r = resize_watcher(ctrl_send.clone()) => r,
        }
    };
    
    // 9. 恢复终端
    disable_raw_mode()?;
    
    // 10. 发送 Detach 消息
    let mut ctrl_send = ctrl_send.lock().await;
    write_detach(&mut ctrl_send).await.ok();
    
    result
}

/// stdin → QUIC 数据流转发
async fn stdin_to_quic(send: &mut quinn::SendStream) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 4096];
    loop {
        let n = stdin.read(&mut buf).await?;
        if n == 0 { break; }
        send.write_all(&buf[..n]).await?;
    }
    Ok(())
}

/// QUIC 数据流 → stdout 转发
async fn quic_to_stdout(recv: &mut quinn::RecvStream) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 4096];
    loop {
        match recv.read(&mut buf).await? {
            Some(n) => {
                stdout.write_all(&buf[..n]).await?;
                stdout.flush().await?;
            }
            None => break,
        }
    }
    Ok(())
}

/// 监听本地终端 resize 事件 → 发送控制消息
async fn resize_watcher(
    ctrl_send: std::sync::Arc<tokio::sync::Mutex<quinn::SendStream>>,
) -> Result<()> {
    use crossterm::event::{Event, EventStream};
    use futures::StreamExt;
    
    let mut reader = EventStream::new();
    while let Some(event) = reader.next().await {
        if let Ok(Event::Resize(cols, rows)) = event {
            let mut send = ctrl_send.lock().await;
            write_resize(&mut send, cols, rows).await?;
        }
    }
    Ok(())
}

/// 列出远程主机上的会话和 pane
pub async fn list_sessions(config: &HostConfig, ca_cert_path: &str) -> Result<()> {
    let conn = connect_to_bridge(&config.bridge_addr, &config.bridge_token, ca_cert_path).await?;
    
    // 通过 JSON 协议流查询 session 列表
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[0x01]).await?; // stream type: JSON
    
    let request = serde_json::json!({
        "type": "list_sessions"
    });
    send_json_frame(&mut send, &request).await?;
    
    let response = recv_json_frame(&mut recv).await?;
    
    // 格式化输出（实际实现简化为 SESSION + HOST 两列）
    if let Some(sessions) = response.get("sessions").and_then(|s| s.as_array()) {
        println!("{:<20} {}", "SESSION", "HOST");
        println!("{}", "-".repeat(40));
        for session in sessions {
            println!(
                "{:<20} {}",
                session["name"].as_str().unwrap_or("-"),
                session["host"].as_str().unwrap_or("-"),
            );
        }
    }
    
    Ok(())
}
```

#### 6.3.5 本地终端管理（`terminal.rs`）

```rust
// crates/yunying-cli/src/terminal.rs
//
// 参考：
// - ratatui 的 raw mode 模式
// - zellij-client 的终端转发

use crossterm::terminal::{enable_raw_mode, disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use std::io::stdout;

/// 终端守卫：确保退出时恢复终端状态
pub struct TerminalGuard {
    alternate_screen: bool,
}

impl TerminalGuard {
    /// 进入 raw 模式
    pub fn enter_raw_mode() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        Ok(TerminalGuard {
            alternate_screen: false,
        })
    }
    
    /// 进入全屏模式（可选）
    pub fn enter_alternate_screen(&mut self) -> anyhow::Result<()> {
        execute!(stdout(), EnterAlternateScreen)?;
        self.alternate_screen = true;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        if self.alternate_screen {
            let _ = execute!(stdout(), LeaveAlternateScreen);
        }
    }
}
```

---

## 七、关键技术决策

### 7.1 为什么用两个 QUIC 流而不是一个

**选择双流**（0x06 控制 + 0x07 数据），参考 VROOM-Terminal 的 control/pty 分离设计。

resize 等控制消息必须即时送达，不能被 PTY 输出数据阻塞。例如当用户快速滚动 vim 产生大量输出时，resize 消息如果和数据混在同一个流里，可能会被排队等待。

### 7.2 为什么数据流不加帧头

**选择纯原始字节**，参考 gmux、neosh、sshx 的设计。

终端 I/O 是连续字节流，QUIC 已保证可靠有序传输，不需要额外帧。加帧会增加延迟（需要等完整帧才能处理）和复杂度（需要处理分片、重组）。

### 7.3 `send_key` vs 直接写 PTY stdin

**选择 `send_key`**（通过 rmux daemon 的 Unix socket）。

| 方案 | 优点 | 缺点 |
|------|------|------|
| `pane.send_key()` | 已有 API，经过测试，保持架构一致 | Unix socket 往返延迟（< 0.1ms） |
| 直接写 PTY fd | 零延迟 | 需要绕过 rmux daemon，破坏架构 |

rmux daemon 的 Unix socket 是本地 IPC，延迟 < 0.1ms，对人类交互来说完全不可感知。当前实现已使用 batch 模式：每次从 QUIC stream 读取的 4096 字节 buffer 直接整体传入 `send_key`，避免逐键调用的 Unix socket 往返开销。如果后续发现延迟问题，可进一步优化为异步 batch（延迟 1-2ms 聚合按键）。

### 7.4 Ghost Buffer 大小

**选择 50 行**，参考 Quil 的 500 行 + sshx 的 1MB，实际实现中采用较保守的 50 行以降低内存占用。大约足够覆盖大多数排查场景。

> **Note**：设计阶段曾考虑 ~64KB 方案（受 TLV 帧 u16 负载长度限制），但实际实现简化为按行数限制。

> **Phase 2 TODO**：将 `SCROLLBACK_LINES` 从硬编码常量改为 bridge 配置项。

---

## 八、安全考量

### 8.1 认证

复用现有 bridge token 认证机制，无需新增认证：

```
CLI 客户端 → QUIC TLS 握手 → AUTH frame (bridge token) → OK
```

### 8.2 审计

#### 8.2.1 问题：CLI 直连时如何审计？

CLI 直连模式下没有 MCP server 参与，传统的 MCP 端审计无法覆盖。但 **bridge 是所有连接的必经之路**，无论来自 MCP server 还是 CLI 客户端，所有 PTY 输入输出都必须经过 bridge 转发。因此 bridge 是天然的审计点。

```
CLI 终端 ←──QUIC──→ bridge ←──Unix Socket──→ rmux daemon
                        │
                   所有数据都经过这里
                   可以旁路录制
```

#### 8.2.2 审计的两个层次

| 层次 | MCP 模式 | CLI 直连模式 |
|------|---------|-------------|
| **连接级**：谁、何时、连了哪台机器、哪个 session、持续多久 | ✅ MCP server 记录 | **bridge 端记录** |
| **操作级**：用户执行了什么命令、看到什么输出 | ✅ 每次 exec 记录 detail + output_summary | **bridge 端旁路录制 PTY I/O** |

#### 8.2.3 连接级审计

bridge 在 `handle_interactive_control` 中记录连接事件：

```rust
// bridge 端：interactive.rs（Phase 3 实现，设计示意）
//
// 注意：client_addr 需从 QUIC 连接对象获取（由调用方 handle_quic_stream 传入），
// hash_token 需在 bridge 初始化时注入。以下为伪代码示意。
pub async fn handle_interactive_control(...) -> Result<()> {
    // 连接建立时记录（Phase 3）
    // CONN: quinn::Connection 需由调用方作为额外参数传入
    let audit_event = InteractiveAuditEvent {
        timestamp: Utc::now(),
        // client_addr: conn.remote_address(),  // Phase 3: 从 QUIC 连接获取
        // token_id: audit_ctx.hash_token(&auth_token),  // Phase 3: 由 bridge 注入
        // host_name: proxy.host_name(),  // Phase 3: ProtocolProxy 新增 host_name() 方法
        host_name: "unknown",  // Phase 3: 从 bridge 配置或 ProtocolProxy 获取
        session_name: session_name.clone(),
        pane_id: pane_id.clone(),
        event_type: "connect",
    };
    // audit_log.write(audit_event).await?;  // Phase 3
    
    // ... 处理连接 ...
    
    // 断开时记录
    audit_event.event_type = "disconnect";
    audit_event.duration_ms = start.elapsed().as_millis();
    audit_log.write(audit_event).await?;
}
```

连接事件存储为 SQLite（与 MCP 审计同格式）：

```json
{
  "event_id": "...",
  "timestamp": "2026-07-07T10:30:00Z",
  "agent_name": "human-user",
  "host_name": "prod-web-01",
  "session_name": "yunying",
  "pane_id": "%0",
  "action": "interactive_connect",
  "detail": "cols=120,rows=40,term=xterm-256color",
  "success": true,
  "duration_ms": 300000
}
```

#### 8.2.4 操作级审计：PTY I/O 旁路录制

bridge 在转发数据流时，**同时写入录制文件**：

```rust
// bridge 端：interactive.rs（审计增强版，Phase 3 实现）
//
// 注意：以下为设计示意，client_addr 需从 QUIC 连接层传入，
// audit_log / SessionRecorder / hash_token 需在 bridge 初始化时注入。
// Phase 3 实现注意：recorder 的 record_input/record_output 需要 &mut self，
// 在 try_join! 两个并发 async 块中需使用 Arc<Mutex<SessionRecorder>> 包装。
pub async fn handle_interactive_data(
    mut send: SendStream,
    mut recv: RecvStream,
    proxy: ProtocolProxy,
    session_state: std::sync::Arc<tokio::sync::Mutex<Option<InteractiveSession>>>,
    // Phase 3: 审计上下文（由调用方注入）
    recorder: Option<std::sync::Arc<tokio::sync::Mutex<SessionRecorder>>>,
) -> Result<()> {
    // 等待 0x06 完成 Attach（同上）
    let session_info = {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        loop {
            if let Some(info) = session_state.lock().await.as_ref() {
                break (info.session_name.clone(), info.pane_id.clone());
            }
            if start.elapsed() > timeout {
                anyhow::bail!("timeout waiting for control stream (0x06) to attach");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    let (session_name, pane_id) = session_info;
    
    let pane = proxy.get_pane(&session_name, &pane_id).await?;
    let mut output_stream = pane.output_stream().await?;
    
    // 创建录制文件（Phase 3）
    // 注意：recorder 由调用方通过 Arc<Mutex<>> 注入，确保并发安全
    let recorder = recorder;  // 直接使用注入的 recorder
    
    // 双向 relay + 旁路录制
    let input_to_pty = async {
        let mut buf = [0u8; 4096];
        loop {
            match recv.read(&mut buf).await? {
                Some(n) => {
                    if let Some(ref r) = recorder { r.lock().await.record_input(&buf[..n]).await; }
                    let keys = String::from_utf8_lossy(&buf[..n]);
                    pane.send_key(&keys).await?;
                }
                None => break,
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    
    let pty_to_output = async {
        while let Some(chunk) = output_stream.next().await {
            match chunk {
                PaneOutputChunk::Bytes { bytes, .. } => {
                    if let Some(ref r) = recorder { r.lock().await.record_output(&bytes).await; }
                    send.write_all(&bytes).await?;
                }
                PaneOutputChunk::Lag(_) => continue,
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    
    tokio::try_join!(input_to_pty, pty_to_output)?;
    if let Some(ref r) = recorder { r.finish().await?; }
    Ok(())
}
```

#### 8.2.5 录制文件格式

采用 **asciinema v2 格式**（业界标准终端录制格式），可直接用 `asciinema play` 回放：

```jsonl
// ~/.yunying/recordings/2026-07-07/prod-web-01_yunying_%0_1720321800.cast
{"version": 2, "width": 120, "height": 40, "timestamp": 1720321800, "env": {"SHELL": "/bin/bash", "TERM": "xterm-256color"}}
{"ts": 0.0, "type": "i", "data": "ls -la\n"}
{"ts": 0.05, "type": "o", "data": "total 32\r\ndrwxr-xr-x  5 root root 4096 Jul  7 10:00 .\r\n..."}
{"ts": 1.2, "type": "i", "data": "cat config.yaml\n"}
{"ts": 1.3, "type": "o", "data": "server:\n  port: 8080\n..."}
```

字段说明：
- `type: "i"` — 用户输入（stdin）
- `type: "o"` — PTY 输出（stdout）
- `ts` — 相对于连接开始的秒数（浮点）
- `data` — 原始字节（JSON 转义）

#### 8.2.6 审计架构总览

```
┌─────────────────────────────────────────────────────────────────┐
│                         审计体系                                  │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  MCP 模式（AI Agent）              CLI 直连模式（人类）           │
│  ┌───────────────────┐            ┌───────────────────┐        │
│  │  yunying-mcp    │            │  yunying-cli    │        │
│  │                   │            │                   │        │
│  │  SQLite 审计日志   │            │  无本地审计        │        │
│  │  (每次工具调用)    │            │  (防篡改无意义)    │        │
│  └────────┬──────────┘            └────────┬──────────┘        │
│           │                                │                    │
│           └──────────┬─────────────────────┘                    │
│                      │                                          │
│                      ▼                                          │
│           ┌──────────────────┐                                  │
│           │   rmux-bridge    │                                  │
│           │                  │                                  │
│           │  统一审计层：      │                                  │
│           │  ① 连接事件日志   │  ← 两种模式都记录                │
│           │  ② PTY I/O 录制  │  ← 两种模式都录制                │
│           │  ③ 审计日志存储   │                                  │
│           └──────────────────┘                                  │
│                      │                                          │
│                      ▼                                          │
│           ┌──────────────────┐                                  │
│           │  审计存储          │                                  │
│           │  bridge 端:       │                                  │
│           │  ~/.yunying/    │                                  │
│           │  ├── audit.db     │  ← 连接事件（SQLite）            │
│           │  └── recordings/  │  ← PTY 录制（asciinema 格式）    │
│           └──────────────────┘                                  │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

#### 8.2.7 MCP 审计 vs CLI 审计对比

| 维度 | MCP 审计 | CLI 审计 |
|------|---------|---------|
| **记录位置** | MCP server 端（用户本地） | bridge 端（目标主机） |
| **粒度** | 每次工具调用一条记录 | 整个连接一条记录 + 连续 I/O 录制 |
| **防篡改** | SQLite 在用户本地，可被修改 | 录制在 bridge 端（目标主机），用户无法修改 |
| **回放** | 无（只有文本摘要） | asciinema 格式，可完整回放 |
| **存储** | `~/.yunying/audit.db` | bridge 端 `~/.yunying/recordings/` |

**CLI 审计实际上比 MCP 审计更安全**——录制文件在目标主机上，操作用户无法删除或篡改。

#### 8.2.8 查询与回放

```bash
# 查询连接记录
$ yunying audit query --type interactive --host prod-web-01

TIME                 USER          HOST          SESSION     DURATION
2026-07-07 10:30:00  human-user    prod-web-01   yunying   5m 23s
2026-07-07 09:15:00  claude-sonnet prod-web-01   yunying   0m 3s    (MCP exec)

# 回放录制
$ yunying replay ~/.yunying/recordings/prod-web-01_yunying_%0_1720321800.cast

# 列出所有录制
$ yunying recordings list --host prod-web-01

FILE                                                      SIZE    DATE
prod-web-01_yunying_%0_1720321800.cast                  12KB    2026-07-07 10:30
prod-web-01_yunying_%0_1720318200.cast                  45KB    2026-07-07 09:15
```

#### 8.2.9 录制存储策略

```yaml
# bridge 端配置（bridge.yaml）
audit:
  recording:
    enabled: true
    dir: ~/.yunying/recordings
    max_age_days: 30          # 保留 30 天
    max_size_mb: 500          # 最大 500MB
    compress: true            # gzip 压缩旧录制
    exclude_patterns:          # 排除敏感 session
      - "secret-*"
```

### 8.3 权限控制（未来扩展）

可以通过 bridge token 的 scope 限制 connect 权限：

```yaml
# hosts.yaml
hosts:
  - name: prod-web-01
    bridge_addr: 10.0.1.10:9778
    bridge_token: "token-with-connect-scope"
    allowed_actions: [exec, connect]  # 限制可执行的操作
    readonly_sessions: ["production"]  # 这些 session 只能只读连接
```

---

## 九、PTY 输入透传与鼠标支持

### 9.1 设计：真透传，不解析

PTY 模式下 stdin **不经过 crossterm 事件解析**，原始字节直接转发给远端 PTY，只拦截三个本地控制字节：

| 字节 | 按键 | 行为 |
|------|------|------|
| `0x07` | Ctrl+G | 打开 AI 面板 |
| `0x1c` | Ctrl+\ | detach 断开 |
| `0x0c` | Ctrl+L | 清空 AI 历史 |

终端 resize 通过 SIGWINCH 信号（`tokio::signal::unix::SignalKind::window_change`）检测，经控制流（0x06）发送 Resize 消息。

### 9.2 为什么不用 crossterm 解析

早期实现用 crossterm `EventStream` 把 stdin 解析成事件，再用 `translate_key_to_bytes`/`translate_mouse_event` 重新编码转发。这个"解析-重编码"循环有两个致命问题：

1. **吞掉终端应答序列**：远端应用（tmux/vim 等）会向终端发查询（如 `\x1b[?997n`），终端的应答（如 `\x1b[?997;2n`）经 stdin 返回。crossterm 不认识这些序列、解析不出事件就直接丢弃，远端等不到应答而卡住。
2. **解析器在 Ghostty 上停摆**：Ghostty 会发送 iTerm2 不发的特有序列，crossterm 解析器遇到后停止产出事件，表现为"输出正常、键盘卡死"。

改为原始字节透传后，stdin 字节（键盘、鼠标 SGR 序列、终端应答）原样到达远端，与直连 `rmux attach` 行为一致。

### 9.3 鼠标

鼠标模式完全由**远端应用拥有**：远端通过透传输出自行启用 SGR 协议（`\x1b[?1000h\x1b[?1002h\x1b[?1006h` 等），终端上报的 SGR 序列（`\x1b[<{button};{col};{row}{M/m}`）作为普通 stdin 字节直接转发。**CLI 不写任何鼠标模式序列，也不翻译鼠标事件。**

> **历史教训**：CLI 曾自行向终端写鼠标模式序列（crossterm `EnableMouseCapture`），与透传输出中远端写入的序列形成"双重驱动"；其默认启用的 1003（any-motion）还会让 Ghostty 上报海量触摸板微动事件。这些均已移除。

---

## 十、OpenCode Serve 生命周期

### 10.1 设计

AI 面板依赖 `opencode serve --port 14096` 作为后端：

| 事件 | 行为 |
|------|------|
| 首次 AI 提问 | `ensure_serve()` 检测端口 14096 是否监听，未监听则 spawn `opencode serve` |
| 后续提问 | 端口已监听 → 直接复用已有 serve 进程 |
| 关闭 AI 面板（Esc/Ctrl+G） | **不杀** serve，保持运行以便下次快速复用 |
| CLI 进程退出 | `main()` 末尾调用 `kill_serve()` 显式清理 |

### 10.2 工作目录

通过 `--opencode-dir <path>` 参数控制 serve 的工作目录（默认当前目录 `.`）。底层使用 `tokio::process::Command::current_dir()` 设置。

> **注意**：`kill_on_drop(true)` 对存储在 `static` 中的 `Child` handle 不生效（程序退出时 static drop 顺序不保证），因此必须在 `main()` 末尾显式 kill。

---

## 十一、分阶段实施计划

### Phase 1：基础连接（3-5 天）

| 任务 | 说明 | 验收标准 |
|------|------|---------|
| Bridge 端 `handle_interactive_control/data` | 新增 0x06/0x07 流处理 | 能接受连接并转发数据 |
| CLI crate 骨架 | `yunying-cli connect` 命令 | `just build` 通过 |
| 基础 PTY 转发 | stdin → send_key, output_stream → stdout | 能执行简单命令并看到输出 |
| Raw mode 终端 | crossterm enable_raw_mode | 终端输入输出正常 |

### Phase 2：体验优化（2-3 天）

| 任务 | 说明 | 验收标准 |
|------|------|---------|
| Resize 同步 | 本地窗口变化 → 远程 pane resize | vim/htop 正确 reflow |
| Ghost Buffer 回放 | 连接时输出最近 ~64KB 历史 | 重连时能看到之前的输出 |
| 优雅断开 | Ctrl+\ 或 `exit` 时发送 Detach 消息 | 断开后 session 继续存活 |
| 错误处理 | session/pane 不存在时的友好提示 | 清晰的错误信息 |
| `yunying list` | 列出可连接的 session/pane | 表格输出 |

### Phase 3：高级特性（3-5 天，可选）

| 任务 | 说明 | 验收标准 |
|------|------|---------|
| 断线重连 | QUIC 连接迁移 + rmux session reattach | 网络切换后自动恢复 |
| 只读模式 | `yunying-cli connect --readonly` | 只能看不能输入 |
| 审计集成 | ✅ 已实现：bridge 端 PTY 录制 + 事件日志 + MCP 同步拉取 | `query_bridge_audit` / `list_recordings` |
| 多人共享 | 多个客户端 attach 同一个 pane | 所有人看到相同输出 |

---

## 十、风险与缓解

| 风险 | 影响 | 概率 | 缓解措施 |
|------|------|------|---------|
| `send_key` 延迟导致交互卡顿 | 中 | 低 | Unix socket < 0.1ms；可后续 batch 优化 |
| `output_stream` 返回 UTF-8 文本而非原始字节 | 中 | 中 | 测试二进制程序（如 `cat /dev/urandom`）；必要时改用 rmux 底层 API |
| rmux daemon 不支持并发 send_key + output_stream | 高 | 低 | 测试验证；必要时用两个独立 daemon 连接 |
| CLI 客户端跨平台兼容性 | 中 | 低 | crossterm 支持 macOS/Linux/Windows |
| 大量输出时 QUIC 流控阻塞 resize | 低 | 低 | 双流设计已缓解；控制流优先级高于数据流 |
| 多人共享时 rmux output_stream 多订阅不兼容 | 高 | 中 | Phase 3 实现前需验证 rmux SDK 是否支持多个 output_stream 订阅同一 pane |

---

## 十一、CLI 使用示例

```bash
# 基础连接
$ yunying-cli connect prod-web-01
Connected to prod-web-01:yunying/%0 (120x40)
Last login: 2026-07-07 10:00:00
$ ls -la
...

# 指定 session 和 pane
$ yunying-cli connect prod-web-01 --session debug --pane %1

# 只读模式（观察 AI Agent 操作）
$ yunying-cli connect prod-web-01 --readonly

# 列出可连接的会话（注意：list 是独立子命令，非 connect --list）
$ yunying-cli list prod-web-01
SESSION              HOST
yunying            prod-web-01
debug                prod-web-01

# 使用自定义 hosts 文件
$ yunying-cli --hosts-file ./my-hosts.yaml connect staging-01
```

---

## 十二、TODO：MCP 方案（本次不实现）

### 12.1 方案概述

除了 CLI 方案，还可以在 MCP server 中直接支持交互式终端连接，让 AI agent 通过工具调用来操作远程终端。

```
AI Agent ──MCP tool call──► yunying-mcp ──QUIC 长连接──► bridge ──► rmux
                                │
                          内部维持连接（类似 tunnel.rs 的模式）
                          StreamManager 管理多个 interactive session
```

### 12.2 工具设计

```
interactive_create(host, session, pane) → session_id
interactive_send(session_id, input)     → 发送按键/命令
interactive_read(session_id)            → 读取当前输出
interactive_resize(session_id, cols, rows)
interactive_close(session_id)
```

MCP server 内部维持 QUIC 长连接，AI agent 通过多次工具调用来"交互"。这和 tunnel 的模式一样——`TunnelManager` 在 MCP 端管理长连接，工具调用只是操作这个连接。

### 12.3 与 CLI 方案的区别

| 维度 | MCP 方案 | CLI 方案 |
|------|---------|---------|
| **使用者** | AI Agent | 人类 |
| **交互方式** | 工具调用（文本） | 实时终端（ANSI 渲染） |
| **体验** | 发一条命令，读一次输出 | 持续的双向流，实时响应 |
| **适合场景** | 多步操作、保留上下文、人机协作 | vim/htop 等 TUI 程序、复杂排查 |
| **开发量** | 小（复用 bridge 协议，加几个工具） | 中（需要 crossterm raw mode 等） |

### 12.4 适用场景

**场景 1：AI 需要在同一个 shell 中执行多步操作（保留环境变量、工作目录）**
```
interactive_create → send("cd /app") → send("export DEBUG=1") → send("./run.sh")
```

**场景 2：AI 和人类协作同一个 session**
```
人类通过 CLI connect 进去排查
AI 通过 MCP interactive_send 补充执行命令
双方操作同一个 pane
```

**场景 3：AI 需要运行交互式程序并分析输出**
```
send("python3") → send("import pandas") → send("df.head()") → read() → 分析结果
```

### 12.5 根本限制

MCP 是请求-响应模式，不是实时流：

```
MCP 模式：
  AI: interactive_send("ls -la\n")
  MCP: OK
  AI: interactive_read()           ← 这里有个时间差
  MCP: "total 32\r\ndrwxr-x..."   ← 返回的是文本快照

CLI 模式：
  用户按键 → 实时看到输出 → 再按键 → 实时看到输出
  （持续的双向流，人类眼睛实时渲染）
```

AI agent 不需要"看到"终端，它需要的是发送命令和读取输出。MCP 方案能满足这个需求，但无法提供人类的实时终端体验。

### 12.6 为什么这次不做

1. **CLI 方案优先级更高**：人类运维场景更紧急
2. **Bridge 端协议已设计好**：0x06/0x07 流类型两种方案通用
3. **MCP 方案可以后续快速实现**：只需在 MCP server 中加几个工具 + StreamManager
4. **两种方案不冲突**：可以并存，MCP server 和 CLI 都是 bridge 的客户端

### 12.7 实现预估

| 组件 | 文件 | 改动类型 | 代码量 |
|------|------|---------|--------|
| MCP Server | `tools/exec.rs` | 新增工具 | ~150 行 |
| MCP Server | `interactive.rs` | 新增模块 | ~200 行 |
| MCP Server | `stream_manager.rs` | 新增模块 | ~150 行 |
| **总计** | | | **~500 行** |

预计开发时间：2-3 天（在 CLI 方案完成后）。

---

## 十三、参考文档

| 文档 | 说明 |
|------|------|
| [neosh 协议规范](https://github.com/plucury/neosh/blob/main/neosh_protocol_v0.1.0.md) | QUIC 终端协议设计 |
| [gmux ADR-0004](https://github.com/gmuxapp/gmux/blob/main/docs/adr/0004-integrated-pty-transport.md) | 极简 PTY + WebSocket 传输 |
| [VROOM-Terminal 协议](https://github.com/deftai/vroom/blob/main/VROOM-Terminal.md) | 形式化远程终端协议规范 |
| [mish 设计文档](https://github.com/amedeedaboville/mish) | Sans-IO SSP + QUIC 终端 |
| [Quil 架构 ADR](https://github.com/artyomsv/quil/blob/master/docs/architecture.md) | Ghost Buffer + 版本握手 |
| [VS Code RemotePty](https://github.com/microsoft/vscode/blob/main/src/vs/workbench/contrib/terminal/browser/remotePty.ts) | start/detach/input/resize/ack 五原语 |
| [docs/DEPLOY.md](./DEPLOY.md) | 本项目部署架构 |
