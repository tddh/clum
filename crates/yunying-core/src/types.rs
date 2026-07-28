use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 目标主机配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    /// 主机标识名，agent 通过此名称引用主机
    pub name: String,
    /// bridge 监听地址 host:port
    pub bridge_addr: String,
    /// bridge 认证 token
    pub bridge_token: String,
    /// 显式分组（生产/测试/开发）
    #[serde(default)]
    pub group: String,
    /// 主机标签，用于分组过滤
    #[serde(default)]
    pub tags: Vec<String>,
    /// 键值对标签，更灵活的过滤（如 dc: shanghai, rack: a3）
    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,
    /// 可选：允许的隧道目标列表（glob 模式，如 "10.0.1.*:*"）。
    /// None = 全部允许（向后兼容，不配置则不限制）。
    #[serde(default)]
    pub allowed_tunnel_targets: Option<Vec<String>>,
}

/// 主机注册表
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostRegistry {
    pub hosts: Vec<HostConfig>,
}

/// Metadata for an interactive terminal session, including session identity,
/// host origin, attachment status, and constituent panes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub host_name: String,
    pub session_name: String,
    pub created_at: DateTime<Utc>,
    pub attached: bool,
    pub windows: usize,
    pub panes: Vec<PaneInfo>,
}

/// Metadata for a single terminal pane within a session window,
/// including its ID, position, optional title, and running state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub window_index: usize,
    pub pane_index: usize,
    pub title: Option<String>,
    pub running: bool,
}

/// 审计事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub event_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub agent_name: String,
    pub host_name: String,
    pub session_name: String,
    pub pane_id: Option<String>,
    pub action: AuditAction,
    pub detail: String,
    pub output_summary: Option<String>,
    pub success: bool,
    pub duration_ms: u64,
    pub error_message: Option<String>,
}

/// 审计动作类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuditAction {
    SessionCreate,
    SessionAttach,
    SessionDetach,
    SendKeys,
    CapturePane,
    WaitForText,
    SplitWindow,
    FileUpload,
    FileDownload,
    SessionList,
    HostList,
    HostFilter,
    Exec,
    ClosePane,
    CloseWindow,
    KillSession,
    PaneInfo,
    WindowInfo,
    PaneExists,
    ResizePane,
    SendText,
    SetPaneTitle,
    FindPaneText,
    RenameWindow,
    ListWindowPanes,
    ResizeWindow,
    SelectWindow,
    SelectLayout,
    WaitExit,
    SpawnCommand,
    ShellCommand,
    RespawnPane,
    BroadcastKeys,
    CmdEscape,
    StreamSubscribe,
    BatchExec,
    BatchUpload,
    BatchDownload,
    TunnelCreate,
    TunnelList,
    TunnelClose,
    FindPanes,
    FindSessions,
    GetPaneTitle,
    FindTextAll,
    ClearHistory,
    ListBuffers,
    PasteBuffer,
    DeleteBuffer,
    SplitPaneWith,
    GetPaneByTitle,
    CollectUntilExit,
    BreakPane,
    JoinPane,
    SwapPane,
    HostCapabilities,
    CaptureRegion,
    WaitForBytes,
    WaitStable,
    DeployBridge,
    AuditQuery,
    AuditStats,
    AuditCleanup,
    ConfigReload,
    BridgeAuditQuery,
}
