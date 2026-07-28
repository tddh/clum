use super::AuditDb;
use anyhow::{Context, Result};
use serde::Serialize;

// ── Query parameter types ──────────────────────────────────────────────────

pub struct QueryParams {
    pub host: Option<String>,
    pub action: Option<String>,
    pub agent: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub success: Option<bool>,
    pub limit: Option<u32>,
}

pub enum OutputFormat {
    Table,
    Json,
    Jsonl,
}

#[derive(Serialize)]
pub struct AuditRow {
    pub id: i64,
    pub timestamp: String,
    pub agent_name: String,
    pub host_name: String,
    pub session_name: String,
    pub pane_id: Option<String>,
    pub action: String,
    pub detail: String,
    pub output_summary: Option<String>,
    pub success: bool,
    pub duration_ms: i64,
    pub error_message: Option<String>,
}

// ── query() ────────────────────────────────────────────────────────────────

impl AuditDb {
    pub async fn query(&self, params: QueryParams, format: OutputFormat) -> Result<String> {
        let db = self.conn_ref().clone();
        tokio::task::spawn_blocking(move || -> Result<String> {
            let conn = db.lock().unwrap_or_else(|e| e.into_inner());

            let mut sql = String::from(
                "SELECT id, timestamp, agent_name, host_name, session_name,
                        pane_id, action, detail, output_summary, success,
                        duration_ms, error_message
                 FROM audit_events WHERE 1=1",
            );
            let mut bind_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

            if let Some(ref h) = params.host {
                sql.push_str(" AND host_name = ?");
                bind_values.push(Box::new(h.clone()));
            }
            if let Some(ref a) = params.action {
                sql.push_str(" AND action = ?");
                bind_values.push(Box::new(a.clone()));
            }
            if let Some(ref a) = params.agent {
                sql.push_str(" AND agent_name = ?");
                bind_values.push(Box::new(a.clone()));
            }
            if let Some(ref s) = params.since {
                sql.push_str(" AND timestamp >= ?");
                bind_values.push(Box::new(s.clone()));
            }
            if let Some(ref u) = params.until {
                sql.push_str(" AND timestamp <= ?");
                bind_values.push(Box::new(u.clone()));
            }
            if let Some(s) = params.success {
                sql.push_str(" AND success = ?");
                bind_values.push(Box::new(s as i32));
            }

            sql.push_str(" ORDER BY timestamp DESC");

            if let Some(l) = params.limit {
                sql.push_str(&format!(" LIMIT {}", l));
            }

            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                bind_values.iter().map(|b| b.as_ref()).collect();

            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(param_refs.as_slice(), |row| {
                Ok(AuditRow {
                    id: row.get(0)?,
                    timestamp: row.get(1)?,
                    agent_name: row.get(2)?,
                    host_name: row.get(3)?,
                    session_name: row.get(4)?,
                    pane_id: row.get(5)?,
                    action: row.get(6)?,
                    detail: row.get(7)?,
                    output_summary: row.get(8)?,
                    success: row.get::<_, i32>(9)? != 0,
                    duration_ms: row.get(10)?,
                    error_message: row.get(11)?,
                })
            })?;

            let results: Vec<AuditRow> = rows.filter_map(|r| r.ok()).collect();

            match format {
                OutputFormat::Table => Ok(format_table(&results)),
                OutputFormat::Json => Ok(serde_json::to_string_pretty(&results)?),
                OutputFormat::Jsonl => {
                    let lines: Vec<String> = results
                        .iter()
                        .map(|r| serde_json::to_string(r).unwrap_or_default())
                        .collect();
                    Ok(lines.join("\n"))
                }
            }
        })
        .await?
        .context("audit query failed")
    }

    // ── stats() ────────────────────────────────────────────────────────────

    pub async fn stats(&self, since: Option<String>) -> Result<String> {
        let db = self.conn_ref().clone();
        tokio::task::spawn_blocking(move || -> Result<String> {
            let conn = db.lock().unwrap_or_else(|e| e.into_inner());

            let (total, succeeded): (i64, i64) = if let Some(ref s) = since {
                (
                    conn.query_row("SELECT COUNT(*) FROM audit_events WHERE timestamp >= ?1", [s.as_str()], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM audit_events WHERE success = 1 AND timestamp >= ?1", [s.as_str()], |r| r.get(0))?,
                )
            } else {
                (
                    conn.query_row("SELECT COUNT(*) FROM audit_events", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM audit_events WHERE success = 1", [], |r| r.get(0))?,
                )
            };

            let success_rate = if total > 0 {
                format!("{:.1}%", succeeded as f64 / total as f64 * 100.0)
            } else {
                "N/A".to_string()
            };

            let avg_duration: f64 = conn.query_row(
                "SELECT COALESCE(AVG(duration_ms), 0) FROM audit_events",
                [],
                |row| row.get(0),
            )?;

            let mut output = String::new();
            output.push_str(&format!("Total events:     {}\n", total));
            output.push_str(&format!("Success rate:     {}\n", success_rate));

            // Top hosts
            let mut stmt = conn.prepare(
                "SELECT host_name, COUNT(*) AS cnt FROM audit_events
                 GROUP BY host_name ORDER BY cnt DESC LIMIT 3",
            )?;
            let top_hosts: Vec<(String, i64)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            if !top_hosts.is_empty() {
                output.push_str(&format!(
                    "Top hosts:        {}\n",
                    top_hosts
                        .iter()
                        .map(|(h, c)| format!("{h} ({c})"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }

            // Top actions
            let mut stmt = conn.prepare(
                "SELECT action, COUNT(*) AS cnt FROM audit_events
                 GROUP BY action ORDER BY cnt DESC LIMIT 5",
            )?;
            let top_actions: Vec<(String, i64)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            if !top_actions.is_empty() {
                output.push_str(&format!(
                    "Top actions:      {}\n",
                    top_actions
                        .iter()
                        .map(|(a, c)| format!("{a} ({c})"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }

            // Top agents
            let mut stmt = conn.prepare(
                "SELECT agent_name, COUNT(*) AS cnt FROM audit_events
                 GROUP BY agent_name ORDER BY cnt DESC LIMIT 3",
            )?;
            let top_agents: Vec<(String, i64)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            if !top_agents.is_empty() {
                output.push_str(&format!(
                    "Top agents:       {}\n",
                    top_agents
                        .iter()
                        .map(|(a, c)| format!("{a} ({c})"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }

            output.push_str(&format!("Avg duration:     {:.0}ms\n", avg_duration));

            let failed: i64 = total - succeeded;
            output.push_str(&format!("Failed events:    {}\n", failed));
            if failed > 0 {
                let latest_failure: String = conn
                    .query_row(
                        "SELECT COALESCE(action || ' on ' || host_name || '/' || COALESCE(pane_id, 'N/A')
                         || ' — ' || timestamp || ' — ' || COALESCE(error_message, 'unknown error'), 'none')
                         FROM audit_events WHERE success = 0
                         ORDER BY timestamp DESC LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or_else(|_| "none".to_string());
                output.push_str(&format!("  Latest failure: {}\n", latest_failure));
            }

            Ok(output)
        })
        .await?
        .context("audit stats failed")
    }
}

// ── format_table() ─────────────────────────────────────────────────────────

fn format_table(rows: &[AuditRow]) -> String {
    if rows.is_empty() {
        return "No events found.".to_string();
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{:<6} {:<26} {:<6} {:<10} {:<15} {:<20} {:<6} {}\n",
        "ID", "TIME", "HOST", "AGENT", "ACTION", "DETAIL", "DUR", "OK"
    ));
    out.push_str(&"-".repeat(110));
    out.push('\n');

    for row in rows {
        let time_short = if row.timestamp.len() > 25 {
            &row.timestamp[..25]
        } else {
            &row.timestamp
        };
        let dur = if row.duration_ms > 0 {
            format!("{}ms", row.duration_ms)
        } else {
            "-".to_string()
        };
        let ok = if row.success { "✅" } else { "❌" };
        let detail = if row.detail.len() > 18 {
            format!("{}…", &row.detail[..17])
        } else {
            row.detail.clone()
        };
        out.push_str(&format!(
            "{:<6} {:<26} {:<6} {:<10} {:<15} {:<20} {:<6} {}\n",
            row.id, time_short, row.host_name, row.agent_name, row.action, detail, dur, ok
        ));
    }
    out
}
