//! Tool → capability mapping.
//!
//! The table below is the single source of truth for the authorization layer.
//! It is guarded by tests against `schema::tools_definition()`: adding a tool
//! without a capability mapping fails the test suite (authorization-design.md
//! §10 invariant 6).
//!
//! Keep the capability verbs stable: policy rows in SQLite reference these
//! strings, so renaming one silently revokes every grant that used it.

use std::collections::HashMap;
use std::sync::LazyLock;

/// Independent verbs an authorization decision can grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    HostRead,
    HostAdmin,
    SessionCreate,
    SessionAttach,
    /// Read a session's metadata/history through the CLI relay. No MCP tool
    /// maps to it today, but the relay's `term` purpose requires it.
    SessionRead,
    SessionKill,
    SessionWrite,
    TerminalRead,
    TerminalInput,
    TerminalManage,
    CommandExec,
    /// 只读执行：仅允许被显式标记为只读的 `exec` / `batch_exec` 命令。
    CommandRead,
    FileRead,
    FileWrite,
    ForwardRead,
    ForwardCreate,
    ForwardDelete,
    RecordingRead,
    AuditRead,
    DeployBridge,
    ConfigReload,
    PolicyWrite,
    UsageRead,
}

impl Capability {
    pub const ALL: &'static [Capability] = &[
        Capability::HostRead,
        Capability::HostAdmin,
        Capability::SessionCreate,
        Capability::SessionAttach,
        Capability::SessionRead,
        Capability::SessionKill,
        Capability::SessionWrite,
        Capability::TerminalRead,
        Capability::TerminalInput,
        Capability::TerminalManage,
        Capability::CommandExec,
        Capability::CommandRead,
        Capability::FileRead,
        Capability::FileWrite,
        Capability::ForwardRead,
        Capability::ForwardCreate,
        Capability::ForwardDelete,
        Capability::RecordingRead,
        Capability::AuditRead,
        Capability::DeployBridge,
        Capability::ConfigReload,
        Capability::PolicyWrite,
        Capability::UsageRead,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Capability::HostRead => "host:read",
            Capability::HostAdmin => "host:admin",
            Capability::SessionCreate => "session:create",
            Capability::SessionAttach => "session:attach",
            Capability::SessionRead => "session:read",
            Capability::SessionKill => "session:kill",
            Capability::SessionWrite => "session:write",
            Capability::TerminalRead => "terminal:read",
            Capability::TerminalInput => "terminal:input",
            Capability::TerminalManage => "terminal:manage",
            Capability::CommandExec => "command:exec",
            Capability::CommandRead => "command:read",
            Capability::FileRead => "file:read",
            Capability::FileWrite => "file:write",
            Capability::ForwardRead => "forward:read",
            Capability::ForwardCreate => "forward:create",
            Capability::ForwardDelete => "forward:delete",
            Capability::RecordingRead => "recording:read",
            Capability::AuditRead => "audit:read",
            Capability::DeployBridge => "deploy:bridge",
            Capability::ConfigReload => "config:reload",
            Capability::PolicyWrite => "policy:write",
            Capability::UsageRead => "usage:read",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Capability::ALL.iter().copied().find(|c| c.as_str() == raw)
    }

    /// Capabilities not scoped to a concrete host. A group/label-scoped grant
    /// must not authorize these: there is no host to match the selector
    /// against, so only an explicit global grant is meaningful.
    pub fn is_global_only(self) -> bool {
        matches!(self, Capability::ConfigReload | Capability::PolicyWrite)
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Complete 68-tool mapping (authorization-design.md §5.3).
///
/// Every tool listed in `schema::tools_definition()` must appear exactly once.
pub const TOOL_CAPABILITIES: &[(&str, Capability)] = &[
    ("clum_usage_rules", Capability::UsageRead),
    ("host_list", Capability::HostRead),
    ("host_filter", Capability::HostRead),
    ("host_capabilities", Capability::HostRead),
    ("host_set_meta", Capability::HostAdmin),
    ("find_sessions", Capability::HostRead),
    ("find_panes", Capability::HostRead),
    ("session_list", Capability::HostRead),
    ("pane_info", Capability::HostRead),
    ("window_info", Capability::HostRead),
    ("pane_exists", Capability::HostRead),
    ("list_window_panes", Capability::HostRead),
    ("get_pane_title", Capability::HostRead),
    ("get_pane_by_title", Capability::HostRead),
    ("session_create", Capability::SessionCreate),
    ("split_window", Capability::SessionCreate),
    ("split_pane", Capability::SessionCreate),
    ("split_pane_with", Capability::SessionCreate),
    ("break_pane", Capability::SessionCreate),
    ("join_pane", Capability::SessionCreate),
    ("swap_pane", Capability::SessionCreate),
    ("session_attach", Capability::SessionAttach),
    ("stream_pane", Capability::SessionAttach),
    ("close_pane", Capability::SessionKill),
    ("close_window", Capability::SessionKill),
    ("kill_session", Capability::SessionKill),
    ("resize_pane", Capability::SessionWrite),
    ("resize_window", Capability::SessionWrite),
    ("rename_window", Capability::SessionWrite),
    ("select_window", Capability::SessionWrite),
    ("select_layout", Capability::SessionWrite),
    ("set_pane_title", Capability::SessionWrite),
    ("capture_pane", Capability::TerminalRead),
    ("capture_region", Capability::TerminalRead),
    ("wait_for_text", Capability::TerminalRead),
    ("wait_stable", Capability::TerminalRead),
    ("wait_exit", Capability::TerminalRead),
    ("wait_for_bytes", Capability::TerminalRead),
    ("find_pane_text", Capability::TerminalRead),
    ("find_text_all", Capability::TerminalRead),
    ("collect_until_exit", Capability::TerminalRead),
    ("send_keys", Capability::TerminalInput),
    ("send_text", Capability::TerminalInput),
    ("broadcast_keys", Capability::TerminalInput),
    ("batch_send_keys", Capability::TerminalInput),
    ("paste_buffer", Capability::TerminalInput),
    ("clear_history", Capability::TerminalManage),
    ("list_buffers", Capability::TerminalManage),
    ("delete_buffer", Capability::TerminalManage),
    ("exec", Capability::CommandExec),
    ("shell_command", Capability::CommandExec),
    ("batch_exec", Capability::CommandExec),
    ("cmd_escape", Capability::CommandExec),
    ("respawn_pane", Capability::CommandExec),
    ("file_download", Capability::FileRead),
    ("batch_download", Capability::FileRead),
    ("file_upload", Capability::FileWrite),
    ("batch_upload", Capability::FileWrite),
    ("forward_list", Capability::ForwardRead),
    ("forward_create", Capability::ForwardCreate),
    ("forward_close", Capability::ForwardDelete),
    ("list_recordings", Capability::RecordingRead),
    ("get_recording", Capability::RecordingRead),
    ("search_recordings", Capability::RecordingRead),
    ("audit_query", Capability::AuditRead),
    ("query_bridge_audit", Capability::AuditRead),
    ("deploy_bridge", Capability::DeployBridge),
    ("reload_config", Capability::ConfigReload),
];

static TOOL_CAPABILITY_MAP: LazyLock<HashMap<&'static str, Capability>> =
    LazyLock::new(|| TOOL_CAPABILITIES.iter().copied().collect());

/// `exec` / `batch_exec` accept either full execution or read-only execution.
/// Every other command tool requires `command:exec` only.
pub fn exec_tool_capabilities(tool: &str) -> Option<&'static [Capability]> {
    match tool {
        "exec" | "batch_exec" => Some(&[Capability::CommandExec, Capability::CommandRead]),
        _ => None,
    }
}

pub fn capability_for_tool(tool_name: &str) -> Option<Capability> {
    TOOL_CAPABILITY_MAP.get(tool_name).copied()
}

/// Capabilities granted to a legacy group-scoped key. Mirrors the pre-P0
/// `authorize`: every tool except `host_set_meta` / `reload_config`.
pub fn legacy_group_actions() -> Vec<Capability> {
    Capability::ALL
        .iter()
        .copied()
        .filter(|c| {
            !matches!(
                c,
                Capability::HostAdmin | Capability::ConfigReload | Capability::PolicyWrite
            )
        })
        .collect()
}

/// Legacy superadmin (key without a group). Keeps the old superadmin
/// restrictions: no policy management, because `policy:write` only exists on
/// the explicit `admin` role.
pub fn legacy_superadmin_actions() -> Vec<Capability> {
    Capability::ALL
        .iter()
        .copied()
        .filter(|c| *c != Capability::PolicyWrite)
        .collect()
}

/// Full administrator role: all business capabilities plus management.
/// Deliberately excludes `approval:decide` (not part of this enum): approvals
/// are decided by a human through elicitation or the server CLI.
pub fn admin_actions() -> Vec<Capability> {
    Capability::ALL.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_tool_has_exactly_one_capability() {
        let def = crate::schema::tools_definition();
        let tools: Vec<String> = def["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().expect("tool name").to_string())
            .collect();

        let mut mapped = HashSet::new();
        for (tool, _cap) in TOOL_CAPABILITIES {
            assert!(
                mapped.insert(*tool),
                "tool '{tool}' is mapped more than once in TOOL_CAPABILITIES"
            );
        }

        let schema_set: HashSet<&str> = tools.iter().map(String::as_str).collect();
        let missing: Vec<&str> = schema_set.difference(&mapped).copied().collect();
        let extra: Vec<&str> = mapped.difference(&schema_set).copied().collect();
        assert!(
            missing.is_empty(),
            "tools without a capability mapping: {missing:?}"
        );
        assert!(
            extra.is_empty(),
            "capability mappings for unknown tools: {extra:?}"
        );
        assert_eq!(tools.len(), 68, "tool surface is frozen at 68");
    }

    #[test]
    fn capability_names_are_unique_and_round_trip() {
        let mut names = HashSet::new();
        for cap in Capability::ALL {
            let name = cap.as_str();
            assert!(names.insert(name), "duplicate capability name: {name}");
            assert_eq!(Capability::parse(name), Some(*cap));
        }
        assert_eq!(Capability::parse("nope:nope"), None);
    }

    #[test]
    fn command_read_is_distinct_and_exec_only() {
        assert_eq!(
            Capability::parse("command:read"),
            Some(Capability::CommandRead)
        );
        assert!(Capability::ALL.contains(&Capability::CommandRead));
        assert_eq!(
            exec_tool_capabilities("exec"),
            Some(&[Capability::CommandExec, Capability::CommandRead][..])
        );
        assert_eq!(
            exec_tool_capabilities("batch_exec"),
            Some(&[Capability::CommandExec, Capability::CommandRead][..])
        );
        assert_eq!(exec_tool_capabilities("shell_command"), None);
        assert_eq!(exec_tool_capabilities("cmd_escape"), None);
        assert_eq!(exec_tool_capabilities("respawn_pane"), None);
    }

    #[test]
    fn legacy_roles_keep_old_restrictions() {
        let group = legacy_group_actions();
        assert!(!group.contains(&Capability::HostAdmin));
        assert!(!group.contains(&Capability::ConfigReload));
        assert!(!group.contains(&Capability::PolicyWrite));
        assert!(group.contains(&Capability::DeployBridge));
        assert!(group.contains(&Capability::CommandExec));

        let superadmin = legacy_superadmin_actions();
        assert!(superadmin.contains(&Capability::HostAdmin));
        assert!(superadmin.contains(&Capability::ConfigReload));
        assert!(!superadmin.contains(&Capability::PolicyWrite));
        assert!(admin_actions().contains(&Capability::PolicyWrite));
    }
}
