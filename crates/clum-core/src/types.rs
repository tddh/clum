use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 目标主机配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    /// 主机标识名，agent 通过此名称引用主机
    pub name: String,
    /// bridge 监听地址 host:port（纯 enrolled 模式下可选，不需要直连）
    #[serde(default)]
    pub bridge_addr: Option<String>,
    /// bridge 认证 token（纯 enrolled 模式下可选，不需要直连）
    #[serde(default)]
    pub bridge_token: Option<String>,
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
    pub allowed_forward_targets: Option<Vec<String>>,
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
    HostSetMeta,
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
    ForwardCreate,
    ForwardList,
    ForwardClose,
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
    AgentRelay,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_config_serde_roundtrip_full() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("dc".to_string(), "shanghai".to_string());
        labels.insert("rack".to_string(), "a3".to_string());

        let config = HostConfig {
            name: "prod-web-01".to_string(),
            bridge_addr: Some("10.0.1.10:9778".to_string()),
            bridge_token: Some("secret-token-abc".to_string()),
            group: "production".to_string(),
            tags: vec!["web".to_string(), "nginx".to_string()],
            labels,
            allowed_forward_targets: Some(vec![
                "10.0.1.*:22".to_string(),
                "10.0.2.*:443".to_string(),
            ]),
        };

        let json = serde_json::to_string(&config).unwrap();
        let restored: HostConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.name, config.name);
        assert_eq!(restored.bridge_addr, config.bridge_addr);
        assert_eq!(restored.bridge_token, config.bridge_token);
        assert_eq!(restored.group, config.group);
        assert_eq!(restored.tags, config.tags);
        assert_eq!(restored.labels, config.labels);
        assert_eq!(
            restored.allowed_forward_targets,
            config.allowed_forward_targets
        );
    }

    #[test]
    fn host_config_deserialize_minimal_fields() {
        let json = r#"{"name":"minimal-host"}"#;
        let config: HostConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "minimal-host");
        assert!(config.bridge_addr.is_none());
        assert!(config.bridge_token.is_none());
        assert_eq!(config.group, "");
        assert!(config.tags.is_empty());
        assert!(config.labels.is_empty());
        assert!(config.allowed_forward_targets.is_none());
    }

    #[test]
    fn host_config_deserialize_optional_nulls() {
        let json = r#"{"name":"partial-host","bridge_addr":null,"bridge_token":null,"group":"","tags":[],"labels":{},"allowed_forward_targets":null}"#;
        let config: HostConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "partial-host");
        assert!(config.bridge_addr.is_none());
        assert!(config.bridge_token.is_none());
        assert!(config.allowed_forward_targets.is_none());
    }

    #[test]
    fn audit_event_serde_roundtrip() {
        let event = AuditEvent {
            event_id: uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            timestamp: chrono::DateTime::parse_from_rfc3339("2024-01-15T10:30:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            agent_name: "test-agent".to_string(),
            host_name: "tf01".to_string(),
            session_name: "clum".to_string(),
            pane_id: Some("%0".to_string()),
            action: AuditAction::Exec,
            detail: "ls -la /tmp".to_string(),
            output_summary: Some("3 files listed".to_string()),
            success: true,
            duration_ms: 42,
            error_message: None,
        };

        let json = serde_json::to_string(&event).unwrap();
        let restored: AuditEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.event_id, event.event_id);
        assert_eq!(restored.timestamp, event.timestamp);
        assert_eq!(restored.agent_name, event.agent_name);
        assert_eq!(restored.host_name, event.host_name);
        assert_eq!(restored.session_name, event.session_name);
        assert_eq!(restored.pane_id, event.pane_id);
        assert_eq!(restored.detail, event.detail);
        assert_eq!(restored.output_summary, event.output_summary);
        assert_eq!(restored.success, event.success);
        assert_eq!(restored.duration_ms, event.duration_ms);
        assert_eq!(restored.error_message, event.error_message);
        // AuditAction 枚举的 serde 比较
        assert!(matches!(restored.action, AuditAction::Exec));
    }

    #[test]
    fn audit_event_deserialize_failed_action() {
        let json = r#"{
            "event_id": "660e8400-e29b-41d4-a716-446655440001",
            "timestamp": "2024-06-01T15:45:30Z",
            "agent_name": "ops-bot",
            "host_name": "db01",
            "session_name": "clum",
            "pane_id": null,
            "action": "SessionCreate",
            "detail": "created session",
            "output_summary": null,
            "success": false,
            "duration_ms": 1500,
            "error_message": "connection refused"
        }"#;
        let event: AuditEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            event.event_id.to_string(),
            "660e8400-e29b-41d4-a716-446655440001"
        );
        assert_eq!(event.host_name, "db01");
        assert!(matches!(event.action, AuditAction::SessionCreate));
        assert!(!event.success);
        assert_eq!(event.duration_ms, 1500);
        assert_eq!(event.error_message, Some("connection refused".to_string()));
        assert!(event.pane_id.is_none());
    }

    #[test]
    fn audit_action_serde_all_variants() {
        let actions = vec![
            (AuditAction::Exec, r#""Exec""#),
            (AuditAction::SessionCreate, r#""SessionCreate""#),
            (AuditAction::SendKeys, r#""SendKeys""#),
            (AuditAction::FileUpload, r#""FileUpload""#),
            (AuditAction::FileDownload, r#""FileDownload""#),
            (AuditAction::DeployBridge, r#""DeployBridge""#),
            (AuditAction::BatchExec, r#""BatchExec""#),
            (AuditAction::ForwardCreate, r#""ForwardCreate""#),
        ];

        for (action, expected_json) in actions {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(json, expected_json);
            let restored: AuditAction = serde_json::from_str(&json).unwrap();
            // Compare debug representation since enum variants without data
            assert_eq!(format!("{:?}", restored), format!("{:?}", action));
        }
    }

    #[test]
    fn host_registry_serde_roundtrip() {
        let registry = HostRegistry {
            hosts: vec![
                HostConfig {
                    name: "host-a".to_string(),
                    bridge_addr: Some("10.0.0.1:9778".to_string()),
                    bridge_token: Some("token-a".to_string()),
                    group: "production".to_string(),
                    tags: vec![],
                    labels: std::collections::HashMap::new(),
                    allowed_forward_targets: None,
                },
                HostConfig {
                    name: "host-b".to_string(),
                    bridge_addr: None,
                    bridge_token: None,
                    group: "staging".to_string(),
                    tags: vec!["test".to_string()],
                    labels: std::collections::HashMap::new(),
                    allowed_forward_targets: None,
                },
            ],
        };

        let json = serde_json::to_string(&registry).unwrap();
        let restored: HostRegistry = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.hosts.len(), 2);
        assert_eq!(restored.hosts[0].name, "host-a");
        assert_eq!(restored.hosts[1].name, "host-b");
        assert_eq!(restored.hosts[1].group, "staging");
        assert!(restored.hosts[1].bridge_addr.is_none());
    }

    #[test]
    fn session_info_serde_roundtrip() {
        let session = SessionInfo {
            session_id: "sess-001".to_string(),
            host_name: "tf01".to_string(),
            session_name: "clum".to_string(),
            created_at: chrono::Utc::now(),
            attached: true,
            windows: 2,
            panes: vec![
                PaneInfo {
                    pane_id: "%0".to_string(),
                    window_index: 0,
                    pane_index: 0,
                    title: Some("main".to_string()),
                    running: true,
                },
                PaneInfo {
                    pane_id: "%1".to_string(),
                    window_index: 1,
                    pane_index: 0,
                    title: None,
                    running: false,
                },
            ],
        };

        let json = serde_json::to_string(&session).unwrap();
        let restored: SessionInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.session_id, session.session_id);
        assert_eq!(restored.host_name, session.host_name);
        assert_eq!(restored.session_name, session.session_name);
        assert_eq!(restored.attached, session.attached);
        assert_eq!(restored.windows, session.windows);
        assert_eq!(restored.panes.len(), 2);
        assert_eq!(restored.panes[0].pane_id, "%0");
        assert_eq!(restored.panes[1].running, false);
        assert_eq!(restored.panes[1].title, None);
    }

    #[test]
    fn pane_info_serde_roundtrip() {
        let pane = PaneInfo {
            pane_id: "%3".to_string(),
            window_index: 2,
            pane_index: 1,
            title: Some("editor".to_string()),
            running: true,
        };

        let json = serde_json::to_string(&pane).unwrap();
        let restored: PaneInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.pane_id, "%3");
        assert_eq!(restored.window_index, 2);
        assert_eq!(restored.pane_index, 1);
        assert_eq!(restored.title, Some("editor".to_string()));
        assert!(restored.running);
    }
}
