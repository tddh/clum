use anyhow::{Context, Result};
use futures::StreamExt;
use opencode::{create_opencode_client, OpencodeClient, OpencodeClientConfig, RequestOptions};
use serde_json::json;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::tui::ai_panel::{AiPanel, QuestionInfo};

// ── 全局状态 ──

static SESSION_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static SERVE_CHILD: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
static OPENCODE_DIR: OnceLock<String> = OnceLock::new();
const SERVE_PORT: u16 = 14096;

fn session_cell() -> &'static Mutex<Option<String>> {
    SESSION_ID.get_or_init(|| Mutex::new(None))
}

// ── opencode serve 生命周期 ──

pub fn init_opencode_dir(dir: &str) {
    OPENCODE_DIR.get_or_init(|| dir.to_string());
}

pub async fn kill_serve() {
    let child_cell = SERVE_CHILD.get();
    if let Some(cell) = child_cell {
        if let Some(ref mut child) = *cell.lock().await {
            let _ = child.kill().await;
        }
    }
}

async fn ensure_serve_impl(force: bool) -> Result<()> {
    if !force
        && tokio::net::TcpStream::connect(("127.0.0.1", SERVE_PORT))
            .await
            .is_ok()
    {
        return Ok(());
    }

    let child_cell = SERVE_CHILD.get_or_init(|| Mutex::new(None));
    if let Some(ref mut child) = *child_cell.lock().await {
        child.kill().await.ok();
    }

    let dir = OPENCODE_DIR.get_or_init(|| String::from(".")).clone();

    let mut cmd = tokio::process::Command::new("opencode");
    cmd.args(["serve", "--port", &SERVE_PORT.to_string()])
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = cmd.spawn().context("failed to start opencode serve")?;

    *child_cell.lock().await = Some(child);

    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if tokio::net::TcpStream::connect(("127.0.0.1", SERVE_PORT))
            .await
            .is_ok()
        {
            tokio::time::sleep(Duration::from_secs(2)).await;
            return Ok(());
        }
    }

    anyhow::bail!("opencode serve failed to start on port {}", SERVE_PORT)
}

async fn ensure_serve() -> Result<()> {
    ensure_serve_impl(false).await
}

fn create_client() -> Result<OpencodeClient> {
    let config = OpencodeClientConfig {
        base_url: format!("http://127.0.0.1:{}", SERVE_PORT),
        timeout: Duration::from_secs(600),
        ..Default::default()
    };
    create_opencode_client(Some(config)).context("failed to create opencode client")
}

async fn get_or_create_session(client: &OpencodeClient) -> Result<String> {
    let mut session_id = session_cell().lock().await;

    if let Some(ref id) = *session_id {
        if client
            .session()
            .get(RequestOptions::default().with_path("sessionID", id))
            .await
            .is_ok()
        {
            return Ok(id.clone());
        }
    }

    let response = client.session().create(RequestOptions::default()).await?;
    let id = response.data["id"]
        .as_str()
        .context("session response missing id")?
        .to_string();

    *session_id = Some(id.clone());
    Ok(id)
}

// ── 文本提取 ──

fn extract_text_from_message(msg: &serde_json::Value) -> Option<String> {
    if msg.get("info")?.get("role")?.as_str()? != "assistant" {
        return None;
    }
    let parts = msg.get("parts")?.as_array()?;
    let mut text = String::new();
    let mut thinking = String::new();
    for p in parts {
        let Some(part_type) = p.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        match part_type {
            "text"
                if !p
                    .get("synthetic")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false) =>
            {
                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
            "reasoning" | "thinking" => {
                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    thinking.push_str(t);
                }
            }
            _ => {}
        }
    }
    if !thinking.is_empty() {
        text = format!("```thinking\n{}\n```\n\n{}", thinking, text);
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn extract_text_from_messages(data: &serde_json::Value) -> Option<String> {
    let messages = data.as_array()?;
    for msg in messages.iter().rev() {
        if let Some(t) = extract_text_from_message(msg) {
            return Some(t);
        }
    }
    None
}

fn extract_code_blocks(text: &str) -> Vec<String> {
    text.split("```")
        .skip(1)
        .step_by(2)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ── 核心 API：SSE 流式推送 + prompt 阻塞 ──

pub async fn ask_opencode(prompt: &str, ai_panel: &AiPanel) -> Result<String> {
    ensure_serve().await?;
    let client = create_client()?;
    let session_id = get_or_create_session(&client).await?;

    let body = json!({
        "parts": [{"type": "text", "text": prompt}]
    });

    // SSE 流：实时推送思考 + 文本到 AI 面板
    let mut events = client.event().subscribe(RequestOptions::default()).await?;
    let panel = ai_panel.clone();
    let sid = session_id.clone();

    let streaming = tokio::spawn(async move {
        // delta 事件只带 {sessionID, partID, field, delta}，不含 part 类型；
        // 需从 part.updated（含完整 part）维护 partID → type 映射。
        let mut part_types: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut last_part_id = String::new();
        while let Some(Ok(event)) = events.next().await {
            let data = event.data.as_str();
            if data.is_empty() {
                continue;
            }
            let parsed: serde_json::Value = match serde_json::from_str(data) {
                Ok(p) => p,
                Err(_) => continue,
            };

            match parsed["type"].as_str().unwrap_or("") {
                "message.part.delta" => {
                    let props = &parsed["properties"];
                    if props["sessionID"].as_str().unwrap_or("") != sid {
                        continue;
                    }
                    // 只显示文本增量；tool 参数 JSON 等 field 不刷到面板
                    if props["field"].as_str().unwrap_or("") != "text" {
                        continue;
                    }
                    let part_id = props["partID"].as_str().unwrap_or("");
                    let delta = props["delta"].as_str().unwrap_or("");
                    if delta.is_empty() {
                        continue;
                    }
                    if part_id != last_part_id {
                        last_part_id = part_id.to_string();
                        let is_thinking = matches!(
                            part_types.get(part_id).map(String::as_str),
                            Some("reasoning") | Some("thinking")
                        );
                        if is_thinking {
                            panel.append_streaming("\n[思考] ").await;
                        }
                    }
                    panel.append_streaming(delta).await;
                }
                "message.part.updated" => {
                    let props = &parsed["properties"];
                    if props["sessionID"].as_str().unwrap_or("") != sid {
                        continue;
                    }
                    if let (Some(id), Some(t)) =
                        (props["part"]["id"].as_str(), props["part"]["type"].as_str())
                    {
                        part_types.insert(id.to_string(), t.to_string());
                    }
                }
                "session.error" => {
                    let props = &parsed["properties"];
                    let esid = props["sessionID"].as_str().unwrap_or("");
                    if !esid.is_empty() && esid != sid {
                        continue;
                    }
                    let msg = props["error"].as_str().unwrap_or("unknown");
                    panel.append_streaming(&format!("\n[错误] {}", msg)).await;
                }
                "question.asked" => {
                    let props = &parsed["properties"];
                    if props["sessionID"].as_str().unwrap_or("") != sid {
                        continue;
                    }
                    let qid = props["questionID"].as_str().unwrap_or("").to_string();
                    let qtext = props["question"].as_str().unwrap_or("请回答").to_string();
                    panel.set_thinking(false).await;
                    panel
                        .append_streaming(&format!("\n[AI 提问] {}", qtext))
                        .await;
                    panel
                        .set_pending_question(Some(QuestionInfo {
                            question_id: qid,
                            text: qtext,
                        }))
                        .await;
                }
                _ => {}
            }
        }
    });

    // 阻塞发送 prompt，等 AI 完成
    let resp = client
        .session()
        .prompt(
            RequestOptions::default()
                .with_path("sessionID", &session_id)
                .with_body(body),
        )
        .await?;

    streaming.abort();

    // 提取最终文本；prompt 响应提取失败时回退拉取消息历史
    let final_text = match extract_text_from_message(&resp.data) {
        Some(t) => Some(t),
        None => client
            .session()
            .messages(RequestOptions::default().with_path("sessionID", &session_id))
            .await
            .ok()
            .and_then(|r| extract_text_from_messages(&r.data)),
    };

    let text = final_text.unwrap_or_default();

    // 用完整结果替换流式内容
    let blocks = extract_code_blocks(&text);
    ai_panel.finish_streaming(blocks).await;

    if text.is_empty() {
        anyhow::bail!("opencode returned empty response")
    } else {
        Ok(text)
    }
}

pub async fn answer_question(ai_panel: &AiPanel, answer: &str) -> Result<()> {
    let Some(q) = ai_panel.pending_question().await else {
        anyhow::bail!("no pending question");
    };

    let client = create_client()?;
    client
        .call_operation(
            "question.reply",
            RequestOptions::default()
                .with_path("requestID", &q.question_id)
                .with_body(json!({"answer": answer})),
        )
        .await?;

    ai_panel
        .add_message(crate::tui::ai_panel::Message {
            role: crate::tui::ai_panel::Role::User,
            content: answer.to_string(),
            code_blocks: vec![],
        })
        .await;
    ai_panel.set_pending_question(None).await;

    // 回复后 AI 继续思考，追加一个空的 assistant 消息作为流式目标
    ai_panel
        .add_message(crate::tui::ai_panel::Message {
            role: crate::tui::ai_panel::Role::Assistant,
            content: String::new(),
            code_blocks: vec![],
        })
        .await;

    Ok(())
}

pub async fn reset_session() {
    *session_cell().lock().await = None;
}
