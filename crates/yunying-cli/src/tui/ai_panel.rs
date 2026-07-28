use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

#[derive(Clone)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub code_blocks: Vec<String>,
}

const MAX_MESSAGES: usize = 200;

#[derive(Clone)]
pub struct QuestionInfo {
    pub question_id: String,
    pub text: String,
}

#[derive(Clone)]
pub struct AiPanel {
    pub messages: Arc<Mutex<Vec<Message>>>,
    pub input: Arc<Mutex<String>>,
    pub thinking: Arc<Mutex<bool>>,
    thinking_since: Arc<Mutex<Option<Instant>>>,
    pub pending_question: Arc<Mutex<Option<QuestionInfo>>>,
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// None = 贴底跟随；Some(s) 钳制在合法范围内。返回实际使用的纵向偏移。
fn effective_scroll(scroll: Option<usize>, total_lines: usize, viewport: usize) -> usize {
    let max_scroll = total_lines.saturating_sub(viewport);
    match scroll {
        Some(s) => s.min(max_scroll),
        None => max_scroll,
    }
}

impl AiPanel {
    pub fn new() -> Self {
        Self {
            messages: Arc::new(Mutex::new(Vec::new())),
            input: Arc::new(Mutex::new(String::new())),
            thinking: Arc::new(Mutex::new(false)),
            thinking_since: Arc::new(Mutex::new(None)),
            pending_question: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn set_thinking(&self, v: bool) {
        *self.thinking.lock().await = v;
        *self.thinking_since.lock().await = if v { Some(Instant::now()) } else { None };
    }

    pub async fn pending_question(&self) -> Option<QuestionInfo> {
        self.pending_question.lock().await.clone()
    }

    pub async fn set_pending_question(&self, q: Option<QuestionInfo>) {
        *self.pending_question.lock().await = q;
    }

    pub async fn add_message(&self, msg: Message) {
        let mut msgs = self.messages.lock().await;
        if msgs.len() >= MAX_MESSAGES {
            msgs.remove(0);
        }
        msgs.push(msg);
    }

    pub async fn clear(&self) {
        self.messages.lock().await.clear();
    }

    pub async fn append_streaming(&self, text: &str) {
        let mut msgs = self.messages.lock().await;
        let needs_new = msgs
            .last()
            .is_none_or(|m| !matches!(m.role, Role::Assistant));
        if needs_new {
            if msgs.len() >= MAX_MESSAGES {
                msgs.remove(0);
            }
            msgs.push(Message {
                role: Role::Assistant,
                content: text.to_string(),
                code_blocks: vec![],
            });
        } else {
            msgs.last_mut().unwrap().content.push_str(text);
        }
    }

    pub async fn finish_streaming(&self, code_blocks: Vec<String>) {
        let mut msgs = self.messages.lock().await;
        if let Some(last) = msgs.last_mut() {
            if matches!(last.role, Role::Assistant) {
                last.code_blocks = code_blocks;
            }
        }
    }

    /// 返回 max_scroll（内容总行数 - 可视高度），供调用方管理跟随/手动滚动。
    pub fn render(
        &self,
        f: &mut Frame,
        area: Rect,
        is_focused: bool,
        scroll: Option<usize>,
        tick: usize,
    ) -> usize {
        let msgs = match self.messages.try_lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        let input = match self.input.try_lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        let thinking = match self.thinking.try_lock() {
            Ok(g) => *g,
            Err(_) => false,
        };

        let mut lines: Vec<Line> = Vec::new();

        for msg in msgs.iter() {
            match msg.role {
                Role::User => {
                    lines.push(Line::from(Span::styled(
                        format!("> {}", msg.content),
                        Style::default().fg(Color::Cyan),
                    )));
                }
                Role::Assistant => {
                    for line in msg.content.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("  {}", line),
                            Style::default().fg(Color::Green),
                        )));
                    }
                }
                Role::System => {
                    lines.push(Line::from(Span::styled(
                        &msg.content,
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }

        if thinking {
            let elapsed = self
                .thinking_since
                .try_lock()
                .ok()
                .and_then(|t| *t)
                .map(|i| i.elapsed().as_secs())
                .unwrap_or(0);
            lines.push(Line::from(Span::styled(
                format!("{} AI 思考中… {}s", SPINNER[tick % SPINNER.len()], elapsed),
                Style::default().fg(Color::Yellow),
            )));
        } else if let Ok(q) = self.pending_question.try_lock() {
            if let Some(ref q) = *q {
                lines.push(Line::from(Span::styled(
                    format!("🔍 AI 提问: {}", q.text),
                    Style::default().fg(Color::Yellow),
                )));
            }
        }

        if is_focused {
            let has_question = self.pending_question.try_lock().is_ok_and(|q| q.is_some());
            if has_question {
                lines.push(Line::from(Span::styled(
                    "输入回答后按 Enter →",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(format!("> {}_", input)));
            } else if !thinking {
                lines.push(Line::from(format!("> {}_", input)));
            }
        }

        let block = Block::default()
            .borders(Borders::TOP)
            .title(" AI ")
            .border_style(if is_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            });

        let viewport = area.height.saturating_sub(1) as usize;
        let max_scroll = lines.len().saturating_sub(viewport);
        let scroll_y = effective_scroll(scroll, lines.len(), viewport);

        let p = Paragraph::new(Text::from(lines))
            .block(block)
            .scroll((scroll_y as u16, 0));
        f.render_widget(p, area);
        max_scroll
    }
}

#[cfg(test)]
mod tests {
    use super::effective_scroll;

    #[test]
    fn none_follows_tail() {
        assert_eq!(effective_scroll(None, 100, 20), 80);
    }

    #[test]
    fn some_is_clamped_to_max() {
        assert_eq!(effective_scroll(Some(95), 100, 20), 80);
        assert_eq!(effective_scroll(Some(30), 100, 20), 30);
    }

    #[test]
    fn no_scroll_when_content_fits() {
        assert_eq!(effective_scroll(None, 10, 20), 0);
        assert_eq!(effective_scroll(Some(5), 10, 20), 0);
    }

    #[test]
    fn empty_content() {
        assert_eq!(effective_scroll(None, 0, 20), 0);
        assert_eq!(effective_scroll(None, 5, 0), 5);
    }
}
