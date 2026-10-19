//! Server-side policy management CLI (authorization-design.md §8).
//!
//! These commands run on the server host. They are intentionally not exposed
//! as MCP tools: an agent can only change policy if a human runs this CLI.
//! Every mutation is written to the audit hash chain as `ConfigReload`.
//!
//! P1 will gate mutations behind MCP elicitation; until then the trust boundary
//! is "shell access to the server host".

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use crate::audit;
use crate::capability::{capability_for_tool, Capability};
use crate::policy::{HostResource, PolicyStore, Selector};
use crate::resolve_audit_db_path;

#[derive(Parser)]
struct RoleCli {
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: RoleCommand,
}

#[derive(Subcommand)]
enum RoleCommand {
    /// List roles, their grants, and all principal bindings.
    List,
    /// Add (or update) a grant on a role. Creates the role if it does not exist.
    Grant {
        role: String,
        #[arg(long, default_value = "host")]
        resource: String,
        /// Comma-separated host groups.
        #[arg(long, value_delimiter = ',')]
        group: Vec<String>,
        /// Comma-separated `key=value` labels.
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Hostname glob, e.g. 'tf*'.
        #[arg(long)]
        name: Option<String>,
        /// Match every host (global grant).
        #[arg(long)]
        all: bool,
        /// Comma-separated capability verbs, e.g. command:exec,file:read.
        #[arg(long, value_delimiter = ',')]
        actions: Vec<String>,
        /// Require human approval for calls matched by this grant.
        #[arg(long)]
        require_approval: bool,
    },
    /// Bind an API key to a role.
    Bind {
        #[arg(long = "key")]
        key: String,
        #[arg(long)]
        role: String,
    },
    /// Remove a role binding from an API key.
    Unbind {
        #[arg(long = "key")]
        key: String,
        #[arg(long)]
        role: String,
    },
}

pub async fn run_role_command(args: &[String]) -> Result<()> {
    let cli = RoleCli::parse_from(
        std::iter::once("clum-mcp role".to_string()).chain(args.iter().cloned()),
    );
    let (db_path, policy) = open_policy(cli.db)?;

    match cli.command {
        RoleCommand::List => print_roles(&policy),
        RoleCommand::Grant {
            role,
            resource,
            group,
            labels,
            name,
            all,
            actions,
            require_approval,
        } => {
            if actions.is_empty() {
                anyhow::bail!("--actions is required");
            }
            let mut parsed = Vec::new();
            for action in &actions {
                let Some(capability) = Capability::parse(action) else {
                    anyhow::bail!(
                        "unknown capability '{action}'; valid values: {}",
                        Capability::ALL
                            .iter()
                            .map(|capability| capability.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                };
                if !parsed.contains(&capability) {
                    parsed.push(capability);
                }
            }

            let mut label_map = std::collections::HashMap::new();
            for label in &labels {
                let Some((key, value)) = label.split_once('=') else {
                    anyhow::bail!("--labels expects key=value, got '{label}'");
                };
                label_map.insert(key.to_string(), value.to_string());
            }
            if !all && group.is_empty() && label_map.is_empty() && name.is_none() {
                anyhow::bail!("a grant needs a selector: --group, --labels, --name, or --all");
            }
            let selector = Selector {
                group: if group.is_empty() {
                    None
                } else {
                    Some(group.clone())
                },
                labels: label_map,
                name,
                all,
            };

            let role_existed = policy
                .list_roles()
                .iter()
                .any(|candidate| candidate.name == role);
            let created = policy.grant(&role, &resource, &selector, &parsed, require_approval)?;

            let detail = format!(
                "policy: role grant role={role} resource={resource} selector={} actions={} require_approval={require_approval} {}",
                selector.describe(),
                parsed
                    .iter()
                    .map(|capability| capability.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                if created { "created" } else { "updated" }
            );
            log_change(&db_path, detail).await?;

            if !role_existed {
                println!("Created role '{role}'.");
            }
            println!(
                "{} grant on role '{role}': {} {}",
                if created { "Added" } else { "Updated" },
                selector.describe(),
                if require_approval {
                    "(requires approval)"
                } else {
                    ""
                }
            );
        }
        RoleCommand::Bind { key, role } => {
            let changed = policy.bind(&key, &role)?;
            let detail = format!("policy: role bind key={key} role={role}");
            log_change(&db_path, detail).await?;
            println!(
                "{} '{key}' → '{role}'",
                if changed { "Bound" } else { "Already bound:" }
            );
        }
        RoleCommand::Unbind { key, role } => {
            let changed = policy.unbind(&key, &role)?;
            let detail = format!("policy: role unbind key={key} role={role}");
            log_change(&db_path, detail).await?;
            println!(
                "{} '{key}' ✕ '{role}'",
                if changed {
                    "Unbound"
                } else {
                    "No such binding:"
                }
            );
        }
    }
    Ok(())
}

fn print_roles(policy: &PolicyStore) {
    for role in policy.list_roles() {
        println!(
            "{} [{}]",
            role.name,
            if role.builtin { "builtin" } else { "custom" }
        );
        if role.grants.is_empty() {
            println!("  (no grants)");
        }
        for grant in &role.grants {
            let mut actions = grant.actions.clone();
            actions.sort();
            println!(
                "  {}  resource={} selector={} actions={}{}",
                grant.id,
                grant.resource_type,
                grant.selector.describe(),
                actions.join(","),
                if grant.require_approval {
                    " require_approval"
                } else {
                    ""
                }
            );
        }
    }

    let bindings = policy.list_bindings();
    println!("\nbindings:");
    if bindings.is_empty() {
        println!("  (none)");
    }
    for (principal, role) in bindings {
        println!("  {principal} → {role}");
    }
}

#[derive(Parser)]
struct PolicyCli {
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// hosts.yaml used to resolve `--host` when the bridge DB has no metadata.
    #[arg(long, global = true, default_value = "config/hosts.yaml")]
    hosts_file: PathBuf,

    #[command(subcommand)]
    command: PolicyCommand,
}

#[derive(Subcommand)]
enum PolicyCommand {
    /// Explain the authorization decision for one key/tool/host.
    Explain {
        #[arg(long = "key")]
        key: String,
        /// Tool name (68-tool surface) or a raw capability verb.
        #[arg(long)]
        tool: String,
        #[arg(long)]
        host: Option<String>,
    },
}

pub async fn run_policy_command(args: &[String]) -> Result<()> {
    let cli = PolicyCli::parse_from(
        std::iter::once("clum-mcp policy".to_string()).chain(args.iter().cloned()),
    );
    let (db_path, policy) = open_policy(cli.db)?;

    match cli.command {
        PolicyCommand::Explain { key, tool, host } => {
            let capability = capability_for_tool(&tool)
                .or_else(|| Capability::parse(&tool))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown tool or capability '{tool}'; pass one of the 68 tool names or a capability verb"
                    )
                })?;

            let host_resource = match &host {
                Some(host) => Some(resolve_host(&db_path, &cli.hosts_file, host).await),
                None => None,
            };
            let group_hint = policy.principal_group(&key);
            let decision = policy.check(
                &key,
                group_hint.as_deref(),
                capability,
                host_resource.as_ref(),
            );

            println!("principal:  {key}");
            if let Some(group) = &group_hint {
                println!("key group:  {group}");
            }
            println!("capability: {}", decision.capability);
            match &host_resource {
                Some(resource) => println!(
                    "host:       {} (group={}, labels={:?})",
                    resource.name,
                    resource.group.as_deref().unwrap_or("(none)"),
                    resource.labels
                ),
                None => println!("host:       (none)"),
            }
            println!(
                "decision:   {}",
                if decision.allowed { "ALLOW" } else { "DENY" }
            );
            if crate::capability::exec_tool_capabilities(&tool).is_some() {
                let read_only = policy.check(
                    &key,
                    group_hint.as_deref(),
                    crate::capability::Capability::CommandRead,
                    host_resource.as_ref(),
                );
                let mode = if decision.allowed {
                    "full"
                } else if read_only.allowed {
                    "read-only"
                } else {
                    "denied"
                };
                println!("exec mode:  {mode}");
            }
            println!("approval:   {}", decision.require_approval);
            if !decision.allowed {
                println!("reason:     {}", decision.reason);
            }
            for matched in &decision.matched {
                println!(
                    "match:      role={} grant={} selector={}{}",
                    matched.role,
                    matched.grant_id,
                    matched.selector,
                    if matched.require_approval {
                        " require_approval"
                    } else {
                        ""
                    }
                );
            }
        }
    }
    Ok(())
}

fn open_policy(db: Option<PathBuf>) -> Result<(PathBuf, std::sync::Arc<PolicyStore>)> {
    let db_path = db.unwrap_or_else(|| resolve_audit_db_path(None));
    let policy = PolicyStore::open(&db_path)?;
    Ok((db_path, policy))
}

async fn resolve_host(db_path: &Path, hosts_file: &Path, host: &str) -> HostResource {
    let mut resource = HostResource {
        name: host.to_string(),
        ..HostResource::default()
    };

    if let Ok(store) = crate::bridge_store::BridgeStore::open(db_path) {
        if let Some(meta) = store
            .get_all_host_meta()
            .await
            .into_iter()
            .find(|meta| meta.hostname == host)
        {
            resource.group = Some(meta.group).filter(|group| !group.is_empty());
            resource.labels = meta.labels;
            return resource;
        }
    }

    if let Ok(router) = crate::router::HostRouter::from_file(hosts_file) {
        if let Some(config) = router.get(host) {
            resource.group = Some(config.group.clone()).filter(|group| !group.is_empty());
            resource.labels = config.labels.clone();
        }
    }
    resource
}

async fn log_change(db_path: &Path, detail: String) -> Result<()> {
    let db = audit::AuditDb::open(db_path)?;
    db.log(clum_core::types::AuditEvent {
        event_id: uuid::Uuid::new_v4(),
        timestamp: chrono::Utc::now(),
        agent_name: "cli".to_string(),
        host_name: String::new(),
        session_name: String::new(),
        pane_id: None,
        operation_id: None,
        action: clum_core::types::AuditAction::ConfigReload,
        detail,
        redacted: false,
        output_summary: None,
        success: true,
        duration_ms: 0,
        error_message: None,
    })
    .await;
    Ok(())
}
