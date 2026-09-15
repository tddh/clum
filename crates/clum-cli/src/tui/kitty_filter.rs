//! kitty keyboard 激活序列过滤器（输出流）。
//!
//! 设计目的：远端 TUI 程序（vim/Neovim 类）通过 push/set 序列（\x1b[>Nu）
//! 把本地终端切进 kitty progressive enhancement；程序异常退出时若恢复
//! 序列丢失，完整实现该协议的终端会残留编码状态。本过滤器从输出流丢弃
//! 一切 push/set 激活序列，pop/restore（'<' 开头）与非 kitty 序列
//! （含 DA2 应答 \x1b[>1;2c 等同为 '>' 开头的序列）原样放行。
//!
//! 默认关闭（2026-09-15 实战复盘：用户病例实测该假说不成立，按 TERM_PROGRAM
//! 自动启用属于为上游报告里的问题盲目用药）。仅在显式设置
//! `CLUM_KITTY_FILTER=1` 时启用。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    /// ESC [ 之后的 intro 字节（'>' | '=' | '<'）——决定处置。
    Intro(u8),
    /// 参数区内（0-9 ; :），intro 字节已记在 kind。
    Params {
        kind: u8,
        len: usize,
    },
}

pub struct KittyEnableFilter {
    active: bool,
    state: State,
    pending: Vec<u8>,
}

/// kitty 参数序列的最大保护长度：超出判定异常整体放行，防无限缓冲。
const MAX_SEQ_LEN: usize = 32;

impl KittyEnableFilter {
    pub fn new(active: bool) -> Self {
        Self {
            active,
            state: State::Ground,
            pending: Vec::new(),
        }
    }

    /// 是否启用（仅显式 opt-in：`CLUM_KITTY_FILTER=1`）。状态机与单测
    /// 不受影响——启用后历史所有用例语义不变。
    pub fn opt_in_requested() -> bool {
        std::env::var("CLUM_KITTY_FILTER")
            .map(|v| v == "1")
            .unwrap_or(false)
    }

    fn flush_pending(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.pending);
        self.pending.clear();
    }

    /// 喂入一个输出 chunk，产出转发块。跨 chunk 安全（序列半截时驻留缓冲）。
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        if !self.active {
            out.extend_from_slice(chunk);
            return;
        }
        for &b in chunk {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                        self.pending.push(b);
                    } else {
                        out.push(b);
                    }
                }
                State::Esc => {
                    // 非 CSI 走向：还原直通
                    if b != b'[' {
                        self.pending.push(b);
                        self.flush_pending(out);
                        self.state = State::Ground;
                    } else {
                        self.state = State::Intro(b'[');
                        self.pending.push(b);
                        // 长度保护
                        if self.pending.len() > MAX_SEQ_LEN {
                            self.flush_pending(out);
                            self.state = State::Ground;
                        }
                    }
                }
                State::Intro(_) => match b {
                    b'>' | b'=' | b'<' => {
                        self.state = State::Params {
                            kind: b,
                            len: self.pending.len(),
                        };
                        self.pending.push(b);
                    }
                    _ => {
                        // ESC [ 后不是 kitty intro：可能是 DA/其他 CSI，整体放行
                        self.pending.push(b);
                        self.flush_pending(out);
                        self.state = State::Ground;
                    }
                },
                State::Params { kind, len } => {
                    self.pending.push(b);
                    match b {
                        b'0'..=b'9' | b';' | b':' => {
                            // 仍留在参数区：唯一需要长度保护的地方（防无限缓冲）
                            if self.pending.len() - len > MAX_SEQ_LEN {
                                self.flush_pending(out);
                                self.state = State::Ground;
                            }
                        }
                        b'u' => {
                            // kitty 序列完成：'>' / '=' 激活 → 吞；'<' 恢复 → 放行
                            if kind == b'<' {
                                self.flush_pending(out);
                            }
                            // 丢弃即清空
                            self.pending.clear();
                            self.state = State::Ground;
                        }
                        _ => {
                            // 非 'u' 终止：不是 kitty 序列（如 DA2 '\x1b[>1;2c'）→ 放行
                            self.flush_pending(out);
                            self.state = State::Ground;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(active: bool, chunks: &[&[u8]]) -> Vec<u8> {
        let mut f = KittyEnableFilter::new(active);
        let mut out = Vec::new();
        for c in chunks {
            f.feed(c, &mut out);
        }
        out
    }

    #[test]
    fn inactive_passthrough() {
        let out = run(false, &[b"a\x1b[>1ub"]);
        assert_eq!(out, b"a\x1b[>1ub");
    }

    #[test]
    fn drops_kitty_push_and_set() {
        let out = run(true, &[b"a\x1b[>1ub\x1b[=1;5uc"]);
        assert_eq!(out, b"abc");
    }

    #[test]
    fn passes_kitty_pop() {
        let out = run(true, &[b"\x1b[<u"]);
        assert_eq!(out, b"\x1b[<u");
    }

    #[test]
    fn passes_da2_reply_and_other_csi() {
        // 同为 '>' 开头的 DA2 应答必须无恙
        let out = run(true, &[b"\x1b[>1;2cx"]);
        assert_eq!(out, b"\x1b[>1;2cx");
        // 普通鼠标/颜色序列无恙
        let out = run(true, &[b"\x1b[?1000h\x1b[31m"]);
        assert_eq!(out, b"\x1b[?1000h\x1b[31m");
    }

    #[test]
    fn split_sequence_across_chunks() {
        // 激活序列被 chunk 边界切开：("\x1b[", ">1", "u") 仍被吞
        let out = run(true, &[b"x\x1b[", b">1", b"u", b"y"]);
        assert_eq!(out, b"xy");
    }

    #[test]
    fn split_passthrough_across_chunks() {
        // DA2 被切开：_FRAGMENT 各半也必须完整放行
        let out = run(true, &[b"\x1b[", b">1;2", b"c"]);
        assert_eq!(out, b"\x1b[>1;2c");
    }

    #[test]
    fn truncated_tail_at_stream_end_is_flushed_on_next_feed() {
        // 流末悬着半个序列：下一次 feed 任何字节都能把它带出去或判定掉
        let mut f = KittyEnableFilter::new(true);
        let mut out = Vec::new();
        f.feed(b"pre\x1b[>", &mut out);
        assert_eq!(out, b"pre");
        f.feed(b"1;2c", &mut out);
        assert_eq!(out, b"pre\x1b[>1;2c");
    }
}
