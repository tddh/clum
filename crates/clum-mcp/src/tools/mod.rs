use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use crate::audit;
use crate::forward::ForwardManager;
use crate::recording_sync;
use crate::router::HostRouter;
use crate::stream::StreamManager;

mod batch;
mod bridge_audit;
mod buffer;
mod common;
mod deploy;
mod discovery;
mod exec;
mod file;
mod forward;
mod output;
mod pane;
mod rules;
mod search;
mod session;
mod window;

// Re-export audit for all sub-modules
pub(crate) use deploy::{audit, audit_flagged};

pub struct ToolContext {
    pub router: Arc<HostRouter>,
    pub ca_cert_path: Option<String>,
    pub audit_db: Arc<audit::AuditDb>,
    pub agent_name: Arc<std::sync::Mutex<String>>,
    pub caller_group: Arc<std::sync::Mutex<Option<String>>>,
    pub current_op: Arc<std::sync::Mutex<Option<String>>>,
    pub forward_manager: Arc<ForwardManager>,
    pub stream_manager: Arc<StreamManager>,
    pub recordings_dir: PathBuf,
    pub recording_keyring: Arc<crate::recording_keyring::RecordingKeyring>,
    pub bridge_registry: Arc<crate::registry::BridgeRegistry>,
    pub bridge_store: Arc<crate::bridge_store::BridgeStore>,
    pub file_transfer: crate::server_config::FileTransferConfig,
    pub command_kb: Arc<std::sync::RwLock<clum_command_kb::KnowledgeBase>>,
    /// 命令知识库的运行时覆盖文件路径。内嵌数据只是「开箱默认值」，
    /// 任何修正/新增都改这个文件（reload_config 生效），**不需要重新编译**。
    pub command_kb_path: PathBuf,
    pub rules: Arc<std::sync::RwLock<clum_command_kb::RuleSet>>,
    /// 规则文件路径（启动时解析；reload_config 时按此路径重载）
    pub rules_path: PathBuf,
    /// 命令裁决策略（permissive / strict）。`reload_config` 热更新。
    pub command_policy: Arc<std::sync::RwLock<crate::server_config::CommandPolicy>>,
    /// Role-based policy snapshot (authorization-design.md P0).
    pub policy: Arc<crate::policy::PolicyStore>,
    /// Authenticated API-key principal. `None` means "no API-key auth"
    /// (stdio mode or bootstrap loopback), which keeps the pre-P0 superadmin
    /// behaviour.
    pub principal_name: Arc<std::sync::Mutex<Option<String>>>,
}

impl Clone for ToolContext {
    fn clone(&self) -> Self {
        Self {
            router: Arc::clone(&self.router),
            ca_cert_path: self.ca_cert_path.clone(),
            audit_db: Arc::clone(&self.audit_db),
            agent_name: Arc::new(std::sync::Mutex::new(
                self.agent_name
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            )),
            caller_group: Arc::new(std::sync::Mutex::new(
                self.caller_group
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            )),
            current_op: Arc::new(std::sync::Mutex::new(
                self.current_op
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            )),
            forward_manager: Arc::clone(&self.forward_manager),
            recording_keyring: Arc::clone(&self.recording_keyring),
            stream_manager: Arc::clone(&self.stream_manager),
            recordings_dir: self.recordings_dir.clone(),
            bridge_registry: Arc::clone(&self.bridge_registry),
            bridge_store: Arc::clone(&self.bridge_store),
            file_transfer: self.file_transfer.clone(),
            command_kb: Arc::clone(&self.command_kb),
            command_kb_path: self.command_kb_path.clone(),
            rules: Arc::clone(&self.rules),
            rules_path: self.rules_path.clone(),
            command_policy: Arc::clone(&self.command_policy),
            policy: Arc::clone(&self.policy),
            principal_name: Arc::new(std::sync::Mutex::new(
                self.principal_name
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            )),
        }
    }
}

async fn resolve_host_group(ctx: &ToolContext, host: &str) -> Option<String> {
    let meta_list = ctx.bridge_store.get_all_host_meta().await;
    if let Some(meta) = meta_list.iter().find(|m| m.hostname == host) {
        if !meta.group.is_empty() {
            return Some(meta.group.clone());
        }
    }
    ctx.router
        .get(host)
        .map(|h| h.group)
        .filter(|g| !g.is_empty())
}

/// 解析一次调用涉及的**所有**主机组（去重）。
///
/// 单主机工具用 `host`；`batch_exec` / `batch_send_keys` 等用 `hosts` 数组。只读 `host`
/// 会让 batch 类工具的 host_group 恒为 None，使所有按主机组限定的规则被跳过
/// （实测：同一条 `host_group=["prod"] action="deny"`，exec 拦住、batch_exec 放行）。
/// 无主机信息时返回 `[None]`，表示「无主机组上下文」。
async fn resolve_call_host_groups(ctx: &ToolContext, args: &Value) -> Vec<Option<String>> {
    let mut hosts: Vec<&str> = Vec::new();
    if let Some(h) = args.get("host").and_then(|v| v.as_str()) {
        hosts.push(h);
    }
    if let Some(arr) = args.get("hosts").and_then(|v| v.as_array()) {
        hosts.extend(arr.iter().filter_map(|v| v.as_str()));
    }
    if hosts.is_empty() {
        return vec![None];
    }
    let mut groups: Vec<Option<String>> = Vec::new();
    for h in hosts {
        let g = resolve_host_group(ctx, h).await;
        if !groups.contains(&g) {
            groups.push(g);
        }
    }
    groups
}

/// 审批绑定的主机标识：单主机用其名；batch 类工具用**排序去重后**的 hosts 集合，
/// 使「一次批准 = 该次调用的整个主机集合」（单命令 + 单主机 + 一次性消费）。
fn approval_scope_host(args: &Value) -> String {
    if let Some(h) = args.get("host").and_then(|v| v.as_str()) {
        return h.to_string();
    }
    if let Some(arr) = args.get("hosts").and_then(|v| v.as_array()) {
        let mut hs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        hs.sort_unstable();
        hs.dedup();
        return hs.join(",");
    }
    String::new()
}

/// 把「执行侧会解码」的参数在入口处解码一次，使**裁决与执行看到同一个字符串**。
///
/// 历史缺陷：`send_keys` 族在执行时调用 `unescape_keys`（`\x0a` → 换行），
/// 而规则引擎评估的是**未解码**的原始 args。于是 `ls \x0arm -rf /` 被判 `Allow`
/// （归一化后只有一段、program=ls），实际执行却是两条命令。这里统一解码并写回
/// args，执行侧不再二次解码（`unescape_keys` 不幂等，重复调用会把 `\\x0a` 解成换行）。
///
/// `send_text` 不在此列：它的语义是「不解释转义」，评估与执行本来就一致。
fn normalize_payload_args(tool_name: &str, args: &mut Value) {
    let field = match tool_name {
        "send_keys" | "batch_send_keys" | "broadcast_keys" => "keys",
        _ => return,
    };
    let Some(raw) = args.get(field).and_then(|v| v.as_str()) else {
        return;
    };
    let decoded = crate::tools::exec::unescape_keys(raw);
    if decoded != raw {
        args[field] = Value::String(decoded);
    }
}

/// 回传给 agent 的命令文本：命令类工具用原文（agent 自己发出的内容，不构成泄露），
/// 无命令载荷的工具用占位描述。审计与审批表用的是**脱敏摘要 / 指纹**，不是这里。
fn agent_visible_command(eval: &rules::CallEvaluation) -> String {
    eval.command.clone().unwrap_or_else(|| eval.display.clone())
}

/// 审计 detail 的裁决详情后缀（无 decision 时为空串）。
fn decision_suffix(decision: &Option<clum_command_kb::Decision>) -> String {
    match decision {
        Some(d) => format!(" segments={:?} hazards={:?}", d.commands, d.hazards),
        None => String::new(),
    }
}

/// How a principal may execute commands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ExecMode {
    /// `command:exec`: normal command policy applies.
    #[default]
    Full,
    /// `command:read`: only commands explicitly classified as read-only.
    ReadOnly,
}

/// Result of the authorization step (P0 capability model).
#[derive(Debug, Default, Clone)]
pub(crate) struct Authorization {
    /// A matching grant demands human approval even when the command rules
    /// would allow the call.
    pub require_approval: bool,
    pub approval_reason: Option<String>,
    pub exec_mode: ExecMode,
}

/// Every host referenced by one call, with the metadata selectors need.
/// `batch_*` tools use `hosts`; single-host tools use `host`.
async fn resolve_call_host_resources(
    ctx: &ToolContext,
    args: &Value,
) -> Vec<crate::policy::HostResource> {
    let mut names: Vec<&str> = Vec::new();
    if let Some(host) = args.get("host").and_then(|v| v.as_str()) {
        if !host.is_empty() {
            names.push(host);
        }
    }
    if let Some(hosts) = args.get("hosts").and_then(|v| v.as_array()) {
        names.extend(hosts.iter().filter_map(|v| v.as_str()));
    }
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return Vec::new();
    }

    let meta_list = ctx.bridge_store.get_all_host_meta().await;
    names
        .into_iter()
        .map(|name| {
            let meta = meta_list.iter().find(|meta| meta.hostname == name);
            let configured = ctx.router.get(name);
            crate::policy::HostResource {
                name: name.to_string(),
                group: meta
                    .map(|meta| meta.group.clone())
                    .filter(|group| !group.is_empty())
                    .or_else(|| {
                        configured
                            .as_ref()
                            .map(|host| host.group.clone())
                            .filter(|group| !group.is_empty())
                    }),
                labels: meta
                    .map(|meta| meta.labels.clone())
                    .filter(|labels| !labels.is_empty())
                    .or_else(|| configured.as_ref().map(|host| host.labels.clone()))
                    .unwrap_or_default(),
            }
        })
        .collect()
}

/// Host names a principal may see for a host-less tool, derived from its role
/// selectors. `None` means unrestricted (no API-key auth / bootstrap).
async fn allowed_host_names(
    ctx: &ToolContext,
    tool_name: &str,
) -> Option<std::collections::HashSet<String>> {
    let principal = ctx
        .principal_name
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let capability = crate::capability::capability_for_tool(tool_name)?;
    let caller_group = ctx
        .caller_group
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    // An `all` grant (admin / legacy-superadmin) is unrestricted: enumerating
    // the registered hosts would hide recordings whose host entry was removed.
    if ctx
        .policy
        .allows_all(&principal, caller_group.as_deref(), capability)
    {
        return None;
    }

    let meta = ctx.bridge_store.get_all_host_meta().await;
    let mut resources: std::collections::HashMap<String, crate::policy::HostResource> =
        std::collections::HashMap::new();
    for host in ctx.router.list() {
        resources.insert(
            host.name.clone(),
            crate::policy::HostResource {
                name: host.name.clone(),
                group: Some(host.group.clone()).filter(|group| !group.is_empty()),
                labels: host.labels.clone(),
            },
        );
    }
    for entry in meta {
        resources.insert(
            entry.hostname.clone(),
            crate::policy::HostResource {
                name: entry.hostname.clone(),
                group: Some(entry.group).filter(|group| !group.is_empty()),
                labels: entry.labels,
            },
        );
    }

    let mut allowed = std::collections::HashSet::new();
    for resource in resources.values() {
        if ctx
            .policy
            .check(
                &principal,
                caller_group.as_deref(),
                capability,
                Some(resource),
            )
            .allowed
        {
            allowed.insert(resource.name.clone());
        }
    }
    Some(allowed)
}

fn denied_message(
    tool_name: &str,
    decision: &crate::policy::PolicyDecision,
    host: Option<&crate::policy::HostResource>,
) -> String {
    // `classify_error_message` maps "not in your group" to FORBIDDEN; keep the
    // phrase in every denial so agents retain the stable error_code contract.
    match host {
        Some(host) => format!(
            "forbidden: '{tool_name}' requires capability '{}'; host '{}' is not in your group or role selector ({} )",
            decision.capability, host.name, decision.reason
        ),
        None => format!(
            "forbidden: '{tool_name}' requires capability '{}' (not in your group or role grants: {})",
            decision.capability, decision.reason
        ),
    }
}

/// Pre-P0 behaviour is preserved for principals without API-key auth:
/// stdio mode and bootstrap loopback run as superadmin. Authenticated
/// principals go through the policy snapshot (default deny).
async fn authorize(ctx: &ToolContext, tool_name: &str, args: &Value) -> Result<Authorization> {
    use crate::capability::Capability;

    let principal = ctx
        .principal_name
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(principal) = principal else {
        return Ok(Authorization::default());
    };

    let caller_group = ctx
        .caller_group
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    let Some(base_capability) = crate::capability::capability_for_tool(tool_name) else {
        // `execute_tool` reports "unknown tool" right after this check; do not
        // turn an unknown name into an authorization decision.
        return Ok(Authorization::default());
    };

    let read_exec = crate::capability::exec_tool_capabilities(tool_name).is_some();
    let candidates: Vec<(ExecMode, Capability)> = if read_exec {
        vec![
            (ExecMode::Full, Capability::CommandExec),
            (ExecMode::ReadOnly, Capability::CommandRead),
        ]
    } else {
        vec![(ExecMode::Full, base_capability)]
    };

    let hosts = resolve_call_host_resources(ctx, args).await;
    let mut first_denial: Option<String> = None;

    for (mode, capability) in candidates {
        let mut require_approval = false;
        let mut approval_reason = None;
        let mut denied = false;

        if hosts.is_empty() {
            let decision = ctx
                .policy
                .check(&principal, caller_group.as_deref(), capability, None);
            if !decision.allowed {
                denied = true;
                if first_denial.is_none() {
                    first_denial = Some(denied_message(tool_name, &decision, None));
                }
            } else if decision.require_approval {
                require_approval = true;
                approval_reason = Some(format!(
                    "role policy requires approval for '{}'",
                    decision.capability
                ));
            }
        } else {
            for host in &hosts {
                let decision =
                    ctx.policy
                        .check(&principal, caller_group.as_deref(), capability, Some(host));
                if !decision.allowed {
                    denied = true;
                    if first_denial.is_none() {
                        first_denial = Some(denied_message(tool_name, &decision, Some(host)));
                    }
                    break;
                }
                if decision.require_approval {
                    require_approval = true;
                    approval_reason = Some(format!(
                        "role policy requires approval for '{}' on host '{}'",
                        decision.capability, host.name
                    ));
                }
            }
        }

        if !denied {
            return Ok(Authorization {
                require_approval,
                approval_reason,
                exec_mode: mode,
            });
        }
    }

    let mut message =
        first_denial.unwrap_or_else(|| format!("forbidden: '{tool_name}' is not authorized"));
    if read_exec {
        message =
            format!("{message}; '{tool_name}' also accepts 'command:read' for read-only commands");
    }
    anyhow::bail!("{message}");
}

/// Audit/recordings 类工具集。工具面已冻结在 68 个（clum-docs/INVARIANTS.md §13，
/// 不再扩张）；若未来解冻并新增访问审计或录制数据的工具，必须用 allowed_host_names
/// 强制 caller-group 过滤，且覆盖"不传 host 参数"的场景（历史教训：search_recordings
/// 曾遗漏导致跨组越权）。本清单是文档锚点 + debug 自检（防清单名写错），
/// 不构成运行时拦截——四个工具各自的分发分支内实现组过滤的强制。
const GROUP_SCOPED_TOOLS: &[&str] = &[
    "audit_query",
    "list_recordings",
    "get_recording",
    "search_recordings",
];

/// Fingerprint for approvals that are not driven by a command payload.
/// Deterministic so a retry with the same args matches the pending approval.
fn policy_approval_fingerprint(tool_name: &str, args: &Value) -> String {
    use sha2::{Digest, Sha256};
    // `approval_id` is a retry credential: including it would make the retry
    // after approval hash differently and deadlock the one-shot approval.
    let mut args = args.clone();
    if let Some(object) = args.as_object_mut() {
        object.remove("approval_id");
    }
    let mut hasher = Sha256::new();
    hasher.update(b"policy-approval\0");
    hasher.update(tool_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(serde_json::to_vec(&args).unwrap_or_default());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Turn `require_approval` into the existing ask flow.
///
/// `deny` and `ask` verdicts are left untouched: a policy grant can only make
/// the decision stricter, never laxer.
fn escalate_to_ask(
    evaluation: &mut Option<rules::CallEvaluation>,
    tool_name: &str,
    args: &Value,
    approval_host: &str,
    reason: Option<&str>,
) {
    let source = "policy:require_approval".to_string();
    let reason = reason
        .unwrap_or("role policy requires approval")
        .to_string();

    match evaluation {
        Some(eval) if matches!(eval.verdict, rules::Verdict::Allow) => {
            eval.verdict = rules::Verdict::Ask { source, reason };
        }
        Some(_) => {}
        None => {
            *evaluation = Some(rules::CallEvaluation {
                verdict: rules::Verdict::Ask { source, reason },
                fingerprint: policy_approval_fingerprint(tool_name, args),
                display: if approval_host.is_empty() {
                    tool_name.to_string()
                } else {
                    format!("{tool_name} → {approval_host}")
                },
                command: None,
                decision: None,
                read_only: false,
            });
        }
    }
}

pub async fn execute_tool(
    ctx: &ToolContext,
    tool_name: &str,
    mut args: Value,
    progress: &mut crate::progress::ProgressReporter,
) -> Result<Value> {
    let authorization = authorize(ctx, tool_name, &args).await?;

    // 先解码、再裁决、再执行：三者必须针对同一字符串（见 normalize_payload_args）。
    normalize_payload_args(tool_name, &mut args);

    // ── 命令规则引擎：deny / ask / allow 三态裁决（权限层之后、执行层之前）──
    // host_groups 覆盖 batch 类工具：它们的目标主机在 `hosts` 数组而非 `host`。
    let host_groups = resolve_call_host_groups(ctx, &args).await;
    let approval_host = approval_scope_host(&args);

    let mut evaluation = rules::evaluate_call(ctx, tool_name, &args, &host_groups);

    // Read-only mode: only an explicitly read-only `Allow` may proceed.
    // Deny keeps its more specific rule source; everything else is refused
    // before any approval is created (approval must not elevate capability).
    if authorization.exec_mode == ExecMode::ReadOnly {
        let is_deny = evaluation
            .as_ref()
            .is_some_and(|eval| matches!(eval.verdict, rules::Verdict::Deny { .. }));
        let permitted = evaluation
            .as_ref()
            .is_some_and(|eval| matches!(eval.verdict, rules::Verdict::Allow) && eval.read_only);
        if !permitted && !is_deny {
            let command = evaluation
                .as_ref()
                .and_then(|eval| eval.command.as_deref())
                .unwrap_or(tool_name);
            audit_flagged(
                ctx,
                clum_core::types::AuditAction::CommandDenied,
                &approval_host,
                "",
                None,
                &format!("deny source=capability:read-only scope={approval_host} cmd={command}"),
                false,
                None,
                false,
                0,
                Some("read-only capability"),
            )
            .await;
            return Ok(rules::deny_payload(
                "capability:read-only",
                "read-only capability does not permit this command",
                command,
            ));
        }
    }

    if authorization.require_approval {
        // A `require_approval` grant makes an otherwise-allowed call ask for
        // human approval. deny/ask verdicts are kept as-is (deny wins).
        escalate_to_ask(
            &mut evaluation,
            tool_name,
            &args,
            &approval_host,
            authorization.approval_reason.as_deref(),
        );
    }

    // Command policy.
    //
    // `permissive` (default) collapses every `Ask` to `Allow` here — after all
    // Ask sources have converged (KB classification, parse-error/empty,
    // structural hazards, and require_approval escalation). `Deny` is never
    // touched. `strict` keeps the legacy three-state behaviour.
    let command_policy = *ctx.command_policy.read().unwrap_or_else(|e| e.into_inner());
    if command_policy == crate::server_config::CommandPolicy::Permissive {
        if let Some(eval) = evaluation.as_mut() {
            if matches!(eval.verdict, rules::Verdict::Ask { .. }) {
                eval.verdict = rules::Verdict::Allow;
            }
        }
    }

    if let Some(eval) = evaluation {
        let host = args
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(approval_host.as_str());
        // 借用匹配（不 move verdict），以便后续仍能整体借用 &eval
        match &eval.verdict {
            rules::Verdict::Allow => {
                // 放行默认不额外写审计：命令执行路径自身已写审计（如 exec → AuditAction::Exec）。
                // 例外：**规则**命中 allow 会覆盖知识库分类，是「为什么放行」最需要复盘的一类，
                // 低频且值得一条独立记录。知识库 allow 可由 KB 版本 + 命令语义复现，不重复写。
                if let Some(d) = &eval.decision {
                    if d.source.starts_with("rule:") {
                        audit_flagged(
                            ctx,
                            clum_core::types::AuditAction::CommandAllowed,
                            host,
                            "",
                            None,
                            &format!(
                                "allow rule={} scope={} cmd={}{}",
                                d.source,
                                approval_host,
                                eval.display,
                                decision_suffix(&eval.decision)
                            ),
                            false,
                            None,
                            true,
                            0,
                            None,
                        )
                        .await;
                    }
                }
            }
            rules::Verdict::Deny { source, reason } => {
                audit_flagged(
                    ctx,
                    clum_core::types::AuditAction::CommandDenied,
                    host,
                    "",
                    None,
                    &format!(
                        "deny rule={source} scope={approval_host} cmd={} why={reason}{}",
                        eval.display,
                        decision_suffix(&eval.decision)
                    ),
                    false,
                    None,
                    false,
                    0,
                    Some(reason),
                )
                .await;
                return Ok(rules::deny_payload(
                    source,
                    reason,
                    &agent_visible_command(&eval),
                ));
            }
            rules::Verdict::Ask { source, reason } => {
                // The approval channel was removed. In strict mode an `ask`
                // verdict fails closed; permissive mode never reaches this arm
                // because Ask is collapsed to Allow before the match.
                audit_flagged(
                    ctx,
                    clum_core::types::AuditAction::CommandDenied,
                    host,
                    "",
                    None,
                    &format!(
                        "deny source={source} scope={approval_host} cmd={} why={reason}",
                        eval.display
                    ),
                    false,
                    None,
                    false,
                    0,
                    Some(reason),
                )
                .await;
                return Ok(rules::deny_payload(
                    source,
                    reason,
                    &agent_visible_command(&eval),
                ));
            }
        }
    }

    debug_assert!(
        GROUP_SCOPED_TOOLS.iter().all(|t| matches!(
            *t,
            "audit_query" | "list_recordings" | "get_recording" | "search_recordings"
        )),
        "GROUP_SCOPED_TOOLS 含未知工具名，请勿随意增删"
    );

    let op = uuid::Uuid::now_v7().to_string();
    *ctx.current_op.lock().unwrap_or_else(|e| e.into_inner()) = Some(op.clone());
    let start = std::time::Instant::now();

    let result = match tool_name {
        "clum_usage_rules" => Ok(json!({})),
        "host_list" => discovery::host_list(ctx).await,
        "host_filter" => discovery::host_filter(ctx, args).await,
        "host_set_meta" => discovery::host_set_meta(ctx, args).await,
        "session_create" => session::session_create(ctx, args).await,
        "session_list" => session::session_list(ctx, args).await,
        "session_attach" => session::session_attach(ctx, args).await,
        "send_keys" => pane::send_keys(ctx, args).await,
        "capture_pane" => pane::capture_pane(ctx, args).await,
        "wait_for_text" => output::wait_for_text(ctx, args).await,
        "wait_exit" => output::wait_exit(ctx, args).await,
        "shell_command" => exec::shell_command(ctx, args).await,
        "respawn_pane" => session::respawn_pane(ctx, args).await,
        "broadcast_keys" => exec::broadcast_keys(ctx, args).await,
        "cmd_escape" => exec::cmd_escape(ctx, args).await,
        "split_window" => window::split_window(ctx, args).await,
        "stream_pane" => window::stream_pane(ctx, args).await,
        "file_upload" => file::file_upload(ctx, args, progress).await,
        "file_download" => file::file_download(ctx, args, progress).await,
        "exec" => exec::exec(ctx, args, progress).await,
        "close_pane" => pane::close_pane(ctx, args).await,
        "split_pane" => pane::split_pane(ctx, args).await,
        "resize_pane" => pane::resize_pane(ctx, args).await,
        "send_text" => pane::send_text(ctx, args).await,
        "set_pane_title" => pane::set_pane_title(ctx, args).await,
        "find_pane_text" => pane::find_pane_text(ctx, args).await,
        "close_window" => window::close_window(ctx, args).await,
        "kill_session" => session::kill_session(ctx, args).await,
        "rename_window" => window::rename_window(ctx, args).await,
        "list_window_panes" => window::list_window_panes(ctx, args).await,
        "resize_window" => window::resize_window(ctx, args).await,
        "select_window" => window::select_window(ctx, args).await,
        "select_layout" => window::select_layout(ctx, args).await,
        "pane_info" => window::pane_info(ctx, args).await,
        "window_info" => window::window_info(ctx, args).await,
        "pane_exists" => window::pane_exists(ctx, args).await,
        "batch_exec" => batch::batch_exec(ctx, args, progress).await,
        "batch_upload" => batch::batch_upload(ctx, args, progress).await,
        "batch_download" => batch::batch_download(ctx, args, progress).await,
        "batch_send_keys" => batch::batch_send_keys(ctx, args, progress).await,
        "forward_create" => forward::forward_create(ctx, args).await,
        "forward_list" => forward::forward_list(ctx).await,
        "forward_close" => forward::forward_close(ctx, args).await,
        "find_panes" => discovery::find_panes(ctx, args).await,
        "find_sessions" => discovery::find_sessions(ctx, args).await,
        "get_pane_title" => pane::get_pane_title(ctx, args).await,
        "find_text_all" => output::find_text_all(ctx, args).await,
        "clear_history" => pane::clear_history(ctx, args).await,
        "list_buffers" => buffer::list_buffers(ctx, args).await,
        "paste_buffer" => buffer::paste_buffer(ctx, args).await,
        "delete_buffer" => buffer::delete_buffer(ctx, args).await,
        "split_pane_with" => pane::split_pane_with(ctx, args).await,
        "get_pane_by_title" => pane::get_pane_by_title(ctx, args).await,
        "collect_until_exit" => exec::collect_until_exit(ctx, args).await,
        "break_pane" => pane::break_pane(ctx, args).await,
        "join_pane" => pane::join_pane(ctx, args).await,
        "swap_pane" => pane::swap_pane(ctx, args).await,
        "host_capabilities" => discovery::host_capabilities(ctx, args).await,
        "capture_region" => pane::capture_region(ctx, args).await,
        "wait_for_bytes" => output::wait_for_bytes(ctx, args).await,
        "wait_stable" => output::wait_stable(ctx, args).await,
        "deploy_bridge" => deploy::deploy_bridge(ctx, args, progress).await,
        "reload_config" => session::reload_config(ctx).await,
        "query_bridge_audit" => {
            let start = std::time::Instant::now();
            let host = args["host"].as_str().unwrap_or("").to_string();
            let result = bridge_audit::query_bridge_audit(ctx, args).await;
            let duration_ms = start.elapsed().as_millis() as u64;
            match &result {
                Ok(value) => {
                    let has_error = value.get("error").and_then(|v| v.as_str()).is_some();
                    audit(
                        ctx,
                        clum_core::types::AuditAction::BridgeAuditQuery,
                        &host,
                        "",
                        None,
                        "query_bridge_audit",
                        None,
                        !has_error,
                        duration_ms,
                        value.get("error").and_then(|v| v.as_str()),
                    )
                    .await;
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    audit(
                        ctx,
                        clum_core::types::AuditAction::BridgeAuditQuery,
                        &host,
                        "",
                        None,
                        "query_bridge_audit",
                        None,
                        false,
                        duration_ms,
                        Some(&err_msg),
                    )
                    .await;
                }
            }
            result
        }
        "audit_query" => {
            let host_names = match allowed_host_names(ctx, "audit_query").await {
                Some(names) => {
                    if names.is_empty() {
                        // No allowed hosts — return nothing; an empty filter
                        // would otherwise match every event.
                        return Ok(json!([]));
                    }
                    Some(names.into_iter().collect::<Vec<_>>())
                }
                None => None,
            };
            let params = crate::audit::query::QueryParams {
                host: args.get("host").and_then(|v| v.as_str()).map(String::from),
                action: args
                    .get("action")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                agent: args.get("agent").and_then(|v| v.as_str()).map(String::from),
                since: args.get("since").and_then(|v| v.as_str()).map(String::from),
                until: args.get("until").and_then(|v| v.as_str()).map(String::from),
                success: args.get("success").and_then(|v| v.as_bool()),
                limit: args.get("limit").and_then(|v| v.as_u64()).map(|v| v as u32),
                host_names,
            };
            match ctx
                .audit_db
                .query(params, crate::audit::query::OutputFormat::Json)
                .await
            {
                Ok(json_str) => Ok(
                    serde_json::from_str(&json_str).unwrap_or(json!({"ok": true, "events": []}))
                ),
                Err(e) => Ok(json!({"ok": false, "error": format!("{e:#}")})),
            }
        }
        "list_recordings" => {
            let start = std::time::Instant::now();
            let host = args.get("host").and_then(|v| v.as_str());
            let date = args.get("date").and_then(|v| v.as_str());
            let session = args.get("session").and_then(|v| v.as_str());
            let result =
                recording_sync::list_local_recordings(&ctx.recordings_dir, host, date, session)
                    .await;
            let duration_ms = start.elapsed().as_millis() as u64;
            match result {
                Ok(mut list) => {
                    if let Some(allowed) = allowed_host_names(ctx, "list_recordings").await {
                        list.retain(|r| {
                            r.get("host")
                                .and_then(|v| v.as_str())
                                .map(|h| allowed.contains(h))
                                .unwrap_or(false)
                        });
                    }
                    let value = json!({ "recordings": list, "count": list.len() });
                    audit(
                        ctx,
                        clum_core::types::AuditAction::AuditQuery,
                        "",
                        "",
                        None,
                        "list_recordings",
                        None,
                        true,
                        duration_ms,
                        None,
                    )
                    .await;
                    Ok(value)
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    audit(
                        ctx,
                        clum_core::types::AuditAction::AuditQuery,
                        "",
                        "",
                        None,
                        "list_recordings",
                        None,
                        false,
                        duration_ms,
                        Some(&err_msg),
                    )
                    .await;
                    Ok(json!({ "ok": false, "error": err_msg }))
                }
            }
        }
        "get_recording" => {
            let start = std::time::Instant::now();
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let allowed = allowed_host_names(ctx, "get_recording").await;
            let result = get_recording_checked(ctx, path, allowed.as_ref()).await;
            let duration_ms = start.elapsed().as_millis() as u64;
            match result {
                Ok(value) => {
                    audit(
                        ctx,
                        clum_core::types::AuditAction::AuditQuery,
                        "",
                        "",
                        None,
                        "get_recording",
                        None,
                        true,
                        duration_ms,
                        None,
                    )
                    .await;
                    Ok(value)
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    audit(
                        ctx,
                        clum_core::types::AuditAction::AuditQuery,
                        "",
                        "",
                        None,
                        "get_recording",
                        None,
                        false,
                        duration_ms,
                        Some(&err_msg),
                    )
                    .await;
                    Ok(json!({ "ok": false, "error": err_msg }))
                }
            }
        }
        "search_recordings" => {
            match allowed_host_names(ctx, "search_recordings").await {
                None => search::search_recordings(ctx, args, None).await,
                Some(allowed) => {
                    if allowed.is_empty() {
                        // No allowed hosts — return nothing; an empty filter
                        // would otherwise match every recording.
                        Ok(json!({
                            "ok": true,
                            "total": 0,
                            "matches": [],
                            "scanned_files": 0,
                            "scanned_bytes": 0,
                            "decrypt_errors": [],
                        }))
                    } else {
                        let allowed: Vec<String> = allowed.into_iter().collect();
                        search::search_recordings(ctx, args, Some(allowed.as_slice())).await
                    }
                }
            }
        }
        _ => {
            // schema 与 dispatch 是两份手写清单。新增工具只改 schema、忘记加
            // 分支时，调用方应看到可定位的 server bug，而不是含糊的 unknown tool。
            let registered = crate::schema::tools_definition()["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|t| t["name"].as_str() == Some(tool_name)));
            if registered {
                anyhow::bail!(
                    "server bug: tool '{}' is registered in the schema but has no dispatch branch",
                    tool_name
                );
            }
            anyhow::bail!("unknown tool: {}", tool_name)
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let outcome = match &result {
        Ok(v) if v.get("ok").and_then(|x| x.as_bool()) == Some(false) => "error",
        Ok(_) => "ok",
        Err(_) => "error",
    };
    tracing::info!(
        op = %op,
        tool = %tool_name,
        result = %outcome,
        duration_ms,
        "tool call"
    );
    *ctx.current_op.lock().unwrap_or_else(|e| e.into_inner()) = None;
    result
}

/// Validate and read a recording file for `get_recording`. Every failure is
/// returned as `Err` so the caller's single audit branch records it.
async fn get_recording_checked(
    ctx: &ToolContext,
    path: &str,
    allowed_hosts: Option<&std::collections::HashSet<String>>,
) -> Result<Value> {
    if path.is_empty() {
        anyhow::bail!("missing 'path'");
    }
    if !std::path::Path::new(path).is_absolute() {
        anyhow::bail!("path must be absolute (use the path returned by list_recordings)");
    }

    // Canonicalize FIRST, then run the group check against the same
    // resolved path that will be read — otherwise a `..` segment can
    // make the group check see host A while the read targets host B.
    let canonical_root = ctx
        .recordings_dir
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("recordings directory not found"))?;
    let canonical_path = std::path::Path::new(path)
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("failed to resolve path: {e}"))?;
    if !canonical_path.starts_with(&canonical_root) {
        anyhow::bail!("path outside recordings directory");
    }

    if let Some(allowed) = allowed_hosts {
        let host_from_path = canonical_path
            .strip_prefix(&canonical_root)
            .ok()
            .and_then(|rel| rel.components().next())
            .and_then(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .unwrap_or_default();
        if !allowed.contains(&host_from_path) {
            anyhow::bail!("host '{host_from_path}' is not in your group or role selector");
        }
    }

    read_recording_file_at(&canonical_path, &ctx.recording_keyring).await
}

/// Read an already-canonicalized recording path and decrypt its content.
/// The caller is responsible for canonicalizing and confining the path to
/// `recordings_dir` (see `get_recording_checked`), so this helper does no
/// path validation of its own.
async fn read_recording_file_at(
    canonical_path: &std::path::Path,
    keyring: &crate::recording_keyring::RecordingKeyring,
) -> Result<Value> {
    let raw = tokio::fs::read(canonical_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read recording: {e}"))?;
    let plain = keyring.decrypt_or_passthrough(&raw)?;
    let content = String::from_utf8_lossy(&plain).to_string();
    Ok(json!({
        "path": canonical_path.to_string_lossy(),
        "content": content,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HOSTS_YAML: &str =
        "hosts:\n  - name: devhost\n    group: dev\n  - name: prodhost\n    group: prod\n";

    fn grouped_ctx(caller_group: &str) -> (ToolContext, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hosts_path = tmp.path().join("hosts.yaml");
        std::fs::write(&hosts_path, HOSTS_YAML).expect("write hosts.yaml");
        let router = crate::router::HostRouter::from_file(&hosts_path).expect("load router");
        let audit_db =
            crate::audit::AuditDb::open(&tmp.path().join("audit.db")).expect("open audit db");
        let bridge_store = crate::bridge_store::BridgeStore::open(&tmp.path().join("bridge.db"))
            .expect("open bridge store");
        let ctx = ToolContext {
            router: Arc::new(router),
            ca_cert_path: None,
            audit_db: Arc::new(audit_db),
            agent_name: Arc::new(std::sync::Mutex::new("test".to_string())),
            caller_group: Arc::new(std::sync::Mutex::new(Some(caller_group.to_string()))),
            current_op: Arc::new(std::sync::Mutex::new(None)),
            forward_manager: Arc::new(crate::forward::ForwardManager::new()),
            stream_manager: Arc::new(crate::stream::StreamManager::new()),
            recordings_dir: tmp.path().join("recordings"),
            recording_keyring: Arc::new(
                crate::recording_keyring::RecordingKeyring::load_or_create(
                    &tmp.path().join("rec-keys"),
                )
                .unwrap(),
            ),
            bridge_registry: Arc::new(crate::registry::BridgeRegistry::new()),
            bridge_store: Arc::new(bridge_store),
            file_transfer: crate::server_config::FileTransferConfig::default(),
            command_kb: Arc::new(std::sync::RwLock::new(
                clum_command_kb::KnowledgeBase::builtin().expect("builtin command kb"),
            )),
            command_kb_path: tmp.path().join("command-kb.toml"),
            rules: Arc::new(std::sync::RwLock::new(clum_command_kb::RuleSet::default())),
            rules_path: tmp.path().join("command-rules.toml"),
            command_policy: Arc::new(std::sync::RwLock::new(
                crate::server_config::CommandPolicy::Strict,
            )),
            policy: crate::policy::PolicyStore::open(&tmp.path().join("audit.db"))
                .expect("open policy store"),
            principal_name: Arc::new(std::sync::Mutex::new(Some("test".to_string()))),
        };
        (ctx, tmp)
    }

    async fn ctx_with_command_capability(
        capabilities: &[crate::capability::Capability],
        rules: Option<clum_command_kb::RuleSet>,
    ) -> (ToolContext, tempfile::TempDir) {
        let (mut ctx, tmp) = grouped_ctx("dev");
        let db = tmp.path().join("policy.db");
        let key_store = crate::api_keys::ApiKeyStore::open(&db).unwrap();
        key_store.add("test", Some("dev")).await.unwrap();
        let policy = crate::policy::PolicyStore::open(&db).unwrap();
        policy
            .grant(
                "op",
                "host",
                &crate::policy::Selector::for_group("dev"),
                capabilities,
                false,
            )
            .unwrap();
        policy.bind("test", "op").unwrap();
        policy.unbind("test", "legacy-group:dev").unwrap();
        ctx.policy = policy;
        *ctx.principal_name.lock().unwrap() = Some("test".to_string());
        if let Some(rules) = rules {
            *ctx.rules.write().unwrap() = rules;
        }
        (ctx, tmp)
    }

    fn write_cast(recordings_dir: &std::path::Path, host: &str) -> std::path::PathBuf {
        let dir = recordings_dir.join(host).join("2026-08-06");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.cast");
        std::fs::write(&path, "{\"version\":2}\n[1.0, \"o\", \"hi\"]\n").unwrap();
        path
    }

    fn progress() -> crate::progress::ProgressReporter {
        crate::progress::ProgressReporter::new_stdout(
            None,
            Arc::new(tokio::sync::Mutex::new(tokio::io::stdout())),
        )
    }

    #[test]
    fn policy_approval_escalates_allow_and_keeps_deny() {
        let args = json!({"host": "h", "foo": "bar"});

        let mut allow = Some(rules::CallEvaluation {
            verdict: rules::Verdict::Allow,
            fingerprint: "f".to_string(),
            display: "d".to_string(),
            command: None,
            decision: None,
            read_only: false,
        });
        escalate_to_ask(&mut allow, "host_set_meta", &args, "h", Some("role policy"));
        assert!(matches!(
            allow.expect("allow is preserved").verdict,
            rules::Verdict::Ask { .. }
        ));

        let mut deny = Some(rules::CallEvaluation {
            verdict: rules::Verdict::Deny {
                source: "rule:x".to_string(),
                reason: "no".to_string(),
            },
            fingerprint: "f".to_string(),
            display: "d".to_string(),
            command: None,
            decision: None,
            read_only: false,
        });
        escalate_to_ask(&mut deny, "host_set_meta", &args, "h", None);
        assert!(matches!(
            deny.expect("deny is preserved").verdict,
            rules::Verdict::Deny { .. }
        ));

        let mut none = None;
        escalate_to_ask(&mut none, "host_set_meta", &args, "h", None);
        let eval = none.expect("policy ask must create an evaluation");
        assert!(matches!(eval.verdict, rules::Verdict::Ask { .. }));
        assert_eq!(
            eval.fingerprint,
            policy_approval_fingerprint("host_set_meta", &args)
        );
    }

    #[test]
    fn policy_fingerprint_ignores_approval_id() {
        let base = json!({"host": "h", "foo": "bar"});
        let with_id = json!({"host": "h", "foo": "bar", "approval_id": "ap-1"});
        assert_eq!(
            policy_approval_fingerprint("host_set_meta", &base),
            policy_approval_fingerprint("host_set_meta", &with_id),
            "approval_id is a retry credential and must not change the fingerprint"
        );
    }

    #[tokio::test]
    async fn read_only_gate_denies_write_and_allows_readonly() {
        let (ctx, _tmp) =
            ctx_with_command_capability(&[crate::capability::Capability::CommandRead], None).await;
        let mut progress = progress();

        let value = execute_tool(
            &ctx,
            "exec",
            json!({"host": "devhost", "command": "systemctl restart nginx"}),
            &mut progress,
        )
        .await
        .unwrap();
        assert_eq!(value["error_code"], json!("COMMAND_DENIED"));

        let result = execute_tool(
            &ctx,
            "exec",
            json!({"host": "devhost", "command": "ls -la"}),
            &mut progress,
        )
        .await;
        match result {
            Ok(value) => assert_ne!(
                value["error_code"],
                json!("COMMAND_DENIED"),
                "read-only command must pass the capability gate"
            ),
            Err(error) => assert!(
                !error.to_string().contains("read-only"),
                "read-only command must pass the capability gate, got: {error}"
            ),
        }
    }

    #[tokio::test]
    async fn read_only_gate_ignores_rule_allow() {
        let rules = clum_command_kb::RuleSet::from_toml_str(
            r#"
[[rule]]
id = "allow-rm"
priority = 1
tool = ["exec"]
program = ["rm"]
action = "allow"
"#,
        )
        .unwrap();
        let (ctx, _tmp) =
            ctx_with_command_capability(&[crate::capability::Capability::CommandRead], Some(rules))
                .await;
        let mut progress = progress();

        let value = execute_tool(
            &ctx,
            "exec",
            json!({"host": "devhost", "command": "rm -rf /tmp/x"}),
            &mut progress,
        )
        .await
        .unwrap();
        assert_eq!(value["error_code"], json!("COMMAND_DENIED"));
    }

    #[tokio::test]
    async fn command_read_does_not_open_other_command_tools() {
        let (ctx, _tmp) =
            ctx_with_command_capability(&[crate::capability::Capability::CommandRead], None).await;
        let error = authorize(&ctx, "shell_command", &json!({"host": "devhost"}))
            .await
            .expect_err("command:read must not authorize shell_command")
            .to_string();
        assert!(error.contains("not in your group"), "got: {error}");
    }

    #[tokio::test]
    async fn capability_layer_blocks_management_and_cross_group() {
        let (ctx, _tmp) = grouped_ctx("dev");
        let mut progress = progress();

        // Group-scoped legacy keys never had reload_config / host_set_meta.
        let error = execute_tool(&ctx, "reload_config", json!({}), &mut progress)
            .await
            .expect_err("group key must not reload config")
            .to_string();
        assert!(
            error.contains("not in your group"),
            "denial must keep the FORBIDDEN keyword, got: {error}"
        );

        // Authorization runs before the command rule engine: cross-group exec
        // is rejected without evaluating the command.
        let error = execute_tool(
            &ctx,
            "exec",
            json!({"host": "prodhost", "command": "ls"}),
            &mut progress,
        )
        .await
        .expect_err("cross-group exec must be rejected")
        .to_string();
        assert!(
            error.contains("not in your group"),
            "cross-group denial must keep the FORBIDDEN keyword, got: {error}"
        );

        // Host-less read tools keep the legacy pre-P0 behaviour.
        let value = execute_tool(&ctx, "host_list", json!({}), &mut progress)
            .await
            .expect("same-group read must be authorized");
        assert!(
            value.get("error").is_none(),
            "host_list should succeed, got {value}"
        );
    }

    #[tokio::test]
    async fn get_recording_rejects_cross_group_dotdot_path() {
        let (ctx, _tmp) = grouped_ctx("dev");
        write_cast(&ctx.recordings_dir, "devhost");
        write_cast(&ctx.recordings_dir, "prodhost");

        let evil = ctx
            .recordings_dir
            .join("devhost")
            .join("..")
            .join("prodhost")
            .join("2026-08-06")
            .join("x.cast");

        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "get_recording",
            json!({ "path": evil.to_string_lossy() }),
            &mut progress,
        )
        .await
        .expect("rejection must be a business error envelope, not a hard Err");
        assert_eq!(value["ok"], json!(false));
        let err = value["error"].as_str().expect("error message present");
        assert!(
            err.contains("not in your group"),
            "cross-group traversal must be rejected, got: {err}"
        );
        assert!(
            value.get("content").is_none(),
            "rejected path must not leak content"
        );
    }

    #[tokio::test]
    async fn get_recording_rejects_out_of_dir_path_with_error_envelope() {
        let (ctx, tmp) = grouped_ctx("dev");
        std::fs::create_dir_all(&ctx.recordings_dir).unwrap();
        let outside = tmp.path().join("outside.cast");
        std::fs::write(&outside, "{}").unwrap();

        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "get_recording",
            json!({ "path": outside.to_string_lossy() }),
            &mut progress,
        )
        .await
        .expect("rejection must be a business error envelope, not a hard Err");
        assert_eq!(value["ok"], json!(false));
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|e| e.contains("outside recordings directory")),
            "out-of-dir path must be rejected, got: {}",
            value["error"]
        );
    }

    #[tokio::test]
    async fn get_recording_allows_same_group_path() {
        let (ctx, _tmp) = grouped_ctx("dev");
        let path = write_cast(&ctx.recordings_dir, "devhost");

        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "get_recording",
            json!({ "path": path.to_string_lossy() }),
            &mut progress,
        )
        .await
        .expect("same-group recording should succeed");
        assert!(value.get("content").and_then(|v| v.as_str()).is_some());
    }

    #[test]
    fn extracts_command_per_tool() {
        assert_eq!(
            rules::extract_command("exec", &json!({"command": "ls -la"})).as_deref(),
            Some("ls -la")
        );
        assert_eq!(
            rules::extract_command("batch_exec", &json!({"command": "df -h"})).as_deref(),
            Some("df -h")
        );
        assert_eq!(
            rules::extract_command("send_keys", &json!({"keys": "rm -rf /\n"})).as_deref(),
            Some("rm -rf /\n")
        );
        assert_eq!(rules::extract_command("capture_pane", &json!({})), None);
    }

    #[tokio::test]
    async fn runtime_kb_override_changes_verdict_without_recompiling() {
        let tmp = tempfile::tempdir().unwrap();
        let kb_path = tmp.path().join("command-kb.toml");
        std::fs::write(
            &kb_path,
            "schema_version = 1\n[[command]]\nname = \"ls\"\neffect = \"ask\"\n",
        )
        .unwrap();

        let kb = crate::load_command_kb(&kb_path).unwrap();
        assert_eq!(
            kb.lookup("ls", &[]).0,
            clum_command_kb::Effect::Ask,
            "运行时覆盖必须压过内嵌默认值"
        );
        assert_eq!(
            kb.lookup("cat", &[]).0,
            clum_command_kb::Effect::Allow,
            "未被覆盖的条目应保持内嵌默认值"
        );

        let missing = crate::load_command_kb(&tmp.path().join("does-not-exist.toml")).unwrap();
        assert_eq!(missing.lookup("ls", &[]).0, clum_command_kb::Effect::Allow);
    }

    #[tokio::test]
    async fn denies_destructive_command_via_execute_tool() {
        let (ctx, _tmp) = grouped_ctx("dev");
        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "exec",
            json!({ "host": "devhost", "command": "rm -rf /" }),
            &mut progress,
        )
        .await
        .expect("deny must be a business error envelope, not a hard Err");
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["error_code"], json!("COMMAND_DENIED"));
        assert_eq!(value["rule_id"], json!("kb:rm dangerous-flag(-rf)"));
    }

    /// 回归：`send_keys` 的转义必须在裁决前解码。旧实现评估 `ls \x0arm -rf /`
    /// （只有一段、program=ls）→ Allow，实际执行却是 `ls` + `rm -rf /`。
    #[tokio::test]
    async fn send_keys_decodes_before_adjudication() {
        let (ctx, _tmp) = grouped_ctx("dev");
        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "send_keys",
            json!({"host": "devhost", "keys": "ls \\x0arm -rf /"}),
            &mut progress,
        )
        .await
        .expect("must be a business envelope, not a connection attempt");
        assert_eq!(
            value["error_code"], "COMMAND_DENIED",
            "decoded payload contains rm -rf /, got: {value}"
        );
    }

    /// 回归：`respawn_pane` / `split_pane_with` 的 `command` 载荷必须进入裁决。
    #[tokio::test]
    async fn respawn_and_split_payloads_are_adjudicated() {
        let (ctx, _tmp) = grouped_ctx("dev");
        for (tool, args) in [
            (
                "respawn_pane",
                json!({"host": "devhost", "pane_id": "%0", "command": "rm -rf /"}),
            ),
            (
                "split_pane_with",
                json!({"host": "devhost", "pane_id": "%0", "command": "rm -rf /"}),
            ),
        ] {
            let mut progress = progress();
            let value = execute_tool(&ctx, tool, args, &mut progress)
                .await
                .expect("must be a business envelope");
            assert_eq!(
                value["error_code"], "COMMAND_DENIED",
                "{tool} must be adjudicated, got: {value}"
            );
        }
    }

    /// 回归：`cmd_escape` 的参数是 rmux CLI，未收录子命令必须 fail-closed。
    #[tokio::test]
    async fn cmd_escape_is_fail_closed_to_deny() {
        let (ctx, _tmp) = grouped_ctx("dev");
        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "cmd_escape",
            json!({"host": "devhost", "args": ["kill-session", "-t", "x"]}),
            &mut progress,
        )
        .await
        .expect("must be a business envelope");
        assert_eq!(
            value["error_code"], "COMMAND_DENIED",
            "unclassified rmux subcommand must fail closed, got: {value}"
        );
    }

    /// 回归：CR 在 TTY 上是 Enter。`cat /etc/shadow\rrm -rf /` 不得判 Allow。
    #[tokio::test]
    async fn carriage_return_payload_is_denied() {
        let (ctx, _tmp) = grouped_ctx("dev");
        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "send_keys",
            json!({"host": "devhost", "keys": "cat /etc/shadow\rrm -rf /"}),
            &mut progress,
        )
        .await
        .expect("must be a business envelope");
        assert_eq!(
            value["error_code"], "COMMAND_DENIED",
            "CR must split commands, got: {value}"
        );
    }

    /// 回归：行编辑控制字符（Ctrl-U/Ctrl-W/ESC）在 TTY 上改变实际执行，
    /// strict 模式下必须直接拒绝（normalize 会记 ControlChar hazard）。
    #[tokio::test]
    async fn tty_control_payload_is_denied() {
        let (ctx, _tmp) = grouped_ctx("dev");
        for keys in ["cat\x15rm -rf /", "cat\x17rm -rf /", "cat\x1b[A"] {
            let mut progress = progress();
            let value = execute_tool(
                &ctx,
                "send_keys",
                json!({"host": "devhost", "keys": keys}),
                &mut progress,
            )
            .await
            .expect("must be a business envelope");
            assert_eq!(
                value["error_code"], "COMMAND_DENIED",
                "control characters must fail closed: {keys:?} -> {value}"
            );
        }
    }

    /// 回归：bare respawn 会重跑 pane 既有进程规格，必须 fail-closed。
    #[tokio::test]
    async fn bare_respawn_is_denied() {
        let (ctx, _tmp) = grouped_ctx("dev");
        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "respawn_pane",
            json!({"host": "devhost", "pane_id": "%0"}),
            &mut progress,
        )
        .await
        .expect("must be a business envelope");
        assert_eq!(
            value["error_code"], "COMMAND_DENIED",
            "bare respawn must not bypass adjudication, got: {value}"
        );
    }

    /// permissive: KB 的 ask 不再产生审批/拒绝。测试环境无 bridge，
    /// 执行阶段连接失败即说明已越过裁决闸门。
    #[tokio::test]
    async fn permissive_collapses_ask_to_allow() {
        // Use a real grant so a capability-layer rejection cannot masquerade as
        // "past the adjudication gate".
        let (ctx, _tmp) =
            ctx_with_command_capability(&[crate::capability::Capability::CommandExec], None).await;
        *ctx.command_policy.write().unwrap() = crate::server_config::CommandPolicy::Permissive;

        let result = execute_tool(
            &ctx,
            "exec",
            json!({ "host": "devhost", "command": "mkdir /tmp/a-omp01" }),
            &mut progress(),
        )
        .await;
        match result {
            Ok(v) => assert!(
                v["error_code"] != json!("APPROVAL_REQUIRED")
                    && v["error_code"] != json!("COMMAND_DENIED")
                    && v["error_code"] != json!("FORBIDDEN"),
                "permissive must not gate a normal write: {v}"
            ),
            Err(_) => { /* no bridge in tests: execution was reached */ }
        }
    }

    /// permissive 不绕过显式 deny。
    #[tokio::test]
    async fn permissive_still_denies_destructive_rule() {
        let (ctx, _tmp) =
            ctx_with_command_capability(&[crate::capability::Capability::CommandExec], None).await;
        *ctx.command_policy.write().unwrap() = crate::server_config::CommandPolicy::Permissive;
        *ctx.rules.write().unwrap() = clum_command_kb::RuleSet::from_toml_str(
            r#"schema_version = 1
[[rule]]
id = "deny-rm-root"
priority = 10
tool = ["exec"]
program = ["rm"]
argv_regex = '^rm\s+(-[a-zA-Z]+\s+)*(/|/\*|/etc|/usr|/var)(/|\s|$)'
action = "deny"
reason = "destructive"
"#,
        )
        .expect("rule set parses");

        let value = execute_tool(
            &ctx,
            "exec",
            json!({ "host": "devhost", "command": "rm -rf /etc" }),
            &mut progress(),
        )
        .await
        .expect("deny is a business envelope");
        assert_eq!(value["error_code"], json!("COMMAND_DENIED"));
    }

    /// D-2：`tool` 维度规则必须能命中「无命令字符串」的工具。
    /// 否则 `config/command-rules.toml:30-38` 的示例规则（paste_buffer/cmd_escape 强制 ask）
    /// 是死规则——TOOLS.md 与 INVARIANTS §14.6 承诺的缓解措施从未生效。
    #[test]
    fn tool_scoped_rule_gates_commandless_tool() {
        let (ctx, _tmp) = grouped_ctx("dev");
        *ctx.rules.write().unwrap() = clum_command_kb::RuleSet::from_toml_str(
            r#"schema_version = 1
[[rule]]
id = "paste-buffer-approval"
priority = 20
tool = ["paste_buffer", "cmd_escape"]
action = "ask"
reason = "该通道内容无法静态检查，需人工确认"
"#,
        )
        .expect("rule set parses");

        let eval = rules::evaluate_call(
            &ctx,
            "paste_buffer",
            &json!({ "host": "devhost", "buffer_name": "b0" }),
            &[None],
        );
        assert!(
            eval.is_some(),
            "tool 维度规则必须命中无命令字符串的工具，否则示例规则是死规则"
        );
        assert!(
            matches!(eval.unwrap().verdict, rules::Verdict::Ask { .. }),
            "action=ask 的规则必须产生 Ask 裁决"
        );
    }

    /// 反向守卫：未启用此类规则时，无命令工具不得被误升为 ask（保持「不检查」语义，
    /// 否则每次 paste_buffer 都要人工批准，闸门会被运维关掉）。
    #[test]
    fn commandless_tool_without_rule_is_not_gated() {
        let (ctx, _tmp) = grouped_ctx("dev");
        assert!(
            rules::evaluate_call(&ctx, "paste_buffer", &json!({ "host": "devhost" }), &[None])
                .is_none(),
            "默认空规则集下 paste_buffer 不应被裁决"
        );
    }

    /// D-4 / Task 15：`batch_exec` 的目标主机在 `hosts` 数组里，不在 `host`。
    /// 只读 `args["host"]` 会让 host_group 恒为 None，使所有按主机组限定的规则被跳过。
    /// 修复前实测：同一条 prod deny 规则，`exec` 拦住、`batch_exec` 直接放行。
    #[tokio::test]
    async fn batch_exec_enforces_host_group_rules() {
        let (ctx, _tmp) = grouped_ctx("prod");
        *ctx.rules.write().unwrap() = clum_command_kb::RuleSet::from_toml_str(
            r#"schema_version = 1
[[rule]]
id = "prod-no-cat"
priority = 10
host_group = ["prod"]
program = ["cat"]
action = "deny"
reason = "prod 禁读敏感文件"
"#,
        )
        .expect("rule set parses");

        let single = execute_tool(
            &ctx,
            "exec",
            json!({ "host": "prodhost", "command": "cat /etc/shadow" }),
            &mut progress(),
        )
        .await
        .expect("deny 是业务错误信封，不是 hard Err");
        assert_eq!(
            single["error_code"],
            json!("COMMAND_DENIED"),
            "对照组：exec 必须被 prod 规则拦住"
        );

        let batch = execute_tool(
            &ctx,
            "batch_exec",
            json!({ "hosts": ["prodhost"], "command": "cat /etc/shadow" }),
            &mut progress(),
        )
        .await
        .expect("deny 是业务错误信封，不是 hard Err");
        assert_eq!(
            batch["error_code"],
            json!("COMMAND_DENIED"),
            "batch_exec 不得绕过 host_group 规则"
        );
    }
}
