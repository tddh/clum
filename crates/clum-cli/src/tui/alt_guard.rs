//! 备用屏退出守卫（输出流，仅 raw 直通启用）。
//!
//! 背景：Ghostty 的滚动区域（DECSTBM）是 Terminal 级全局字段——主屏与备用屏
//! 共用同一份（src/terminal/Terminal.zig:67 的 `scrolling_region`），切屏函数
//! `switchScreen` 不保存/恢复它；而 TUI（htop 等）在备用屏内设置滚动区域后，
//! 退出时并不重置（xterm 语义下每个屏幕缓冲各自持有 margin，应用无需重置）。
//! 于是该区域泄漏到主屏：若 shell 光标恰好落在区域之外的最后一行，后续换行
//! 不再触发滚动，所有输出覆盖写在同一行——表现为"屏幕像卡死、只有一行在动"，
//! 而输入输出全程正常。
//!
//! 处置：检测到离开备用屏（`l` 形式的 47 / 1047 / 1049）时补发
//! `\x1b[s\x1b[r\x1b[u`（保存光标 → 重置滚动区域 → 恢复光标）。区域本已是
//! 全屏时 DECSTBM 是空操作，故对正常路径无内容改动、无额外延迟。
//! 不启用时逐字节透传。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    /// ESC [ 之后：收集中间/参数字节，等待终止字节。
    Params,
}

pub struct AltScreenGuard {
    active: bool,
    state: State,
    /// 已解析出的参数数字（非数字标记如 '?' 忽略）。
    nums: Vec<u16>,
    cur: Option<u16>,
    /// 当前 CSI 序列的原始字节，用于原样放行。
    pending: Vec<u8>,
}

/// CSI 序列最大保护长度，超出即判定异常并原样放行，防无限缓冲。
const MAX_SEQ_LEN: usize = 32;

/// 离开备用屏后补发：保存光标 → 重置滚动区域 → 恢复光标。
const ALT_OFF_GUARD: &[u8] = b"\x1b[s\x1b[r\x1b[u";

/// 触发守卫的模式编号（`l` 形式）。
const ALT_OFF_MODES: [u16; 3] = [47, 1047, 1049];

impl AltScreenGuard {
    pub fn new(active: bool) -> Self {
        Self {
            active,
            state: State::Ground,
            nums: Vec::new(),
            cur: None,
            pending: Vec::new(),
        }
    }

    fn flush_pending(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.pending);
        self.pending.clear();
    }

    fn reset_seq(&mut self) {
        self.pending.clear();
        self.nums.clear();
        self.cur = None;
        self.state = State::Ground;
    }

    fn end_param(&mut self) {
        if let Some(v) = self.cur.take() {
            self.nums.push(v);
        }
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
                        self.pending.push(b);
                        self.state = State::Esc;
                    } else {
                        out.push(b);
                    }
                }
                State::Esc => {
                    self.pending.push(b);
                    if b == b'[' {
                        self.state = State::Params;
                    } else {
                        // 非 CSI 走向：整体还原直通
                        self.end_param();
                        self.flush_pending(out);
                        self.reset_seq();
                    }
                }
                State::Params => {
                    self.pending.push(b);
                    match b {
                        b'0'..=b'9' => {
                            let d = (b - b'0') as u16;
                            self.cur =
                                Some(self.cur.unwrap_or(0).saturating_mul(10).saturating_add(d));
                        }
                        b';' => self.end_param(),
                        b'?' | b'<' | b'=' | b'>' | b':' | b' ' => {}
                        b'l' => {
                            self.end_param();
                            let leak = self.nums.iter().any(|n| ALT_OFF_MODES.contains(n));
                            self.flush_pending(out);
                            if leak {
                                out.extend_from_slice(ALT_OFF_GUARD);
                            }
                            self.reset_seq();
                        }
                        _ => {
                            // 其它终止字节（h/m/H/c 等）：原样放行
                            self.end_param();
                            self.flush_pending(out);
                            self.reset_seq();
                        }
                    }
                    if self.state == State::Params && self.pending.len() > MAX_SEQ_LEN {
                        self.end_param();
                        self.flush_pending(out);
                        self.reset_seq();
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
        let mut g = AltScreenGuard::new(active);
        let mut out = Vec::new();
        for c in chunks {
            g.feed(c, &mut out);
        }
        out
    }

    const GUARD: &[u8] = b"\x1b[s\x1b[r\x1b[u";

    #[test]
    fn inactive_passthrough() {
        assert_eq!(run(false, &[b"a\x1b[?1049lb"]), b"a\x1b[?1049lb");
    }

    #[test]
    fn appends_guard_after_alt_off() {
        let mut want = Vec::new();
        want.extend_from_slice(b"a\x1b[?1049l");
        want.extend_from_slice(GUARD);
        want.extend_from_slice(b"b");
        assert_eq!(run(true, &[b"a\x1b[?1049lb"]), want);
    }

    #[test]
    fn covers_47_and_1047() {
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[47l");
        want.extend_from_slice(GUARD);
        want.extend_from_slice(b"\x1b[1047l");
        want.extend_from_slice(GUARD);
        assert_eq!(run(true, &[b"\x1b[47l\x1b[1047l"]), want);
    }

    #[test]
    fn ignores_alt_on_and_other_modes() {
        // 'h' 形式（进入备用屏）、鼠标/括号粘贴/光标等既有模式不得触发
        for seq in [
            &b"\x1b[?1049h"[..],
            b"\x1b[?47h",
            b"\x1b[?2004l",
            b"\x1b[?25l",
            b"\x1b[?1000l",
            b"\x1b[?1000;1006l",
            b"\x1b[?2031l",
            b"\x1b[?996n",
            b"\x1b[2J",
            b"\x1b[1;32r",
        ] {
            assert_eq!(run(true, &[seq]), seq, "seq={seq:?}");
        }
    }

    #[test]
    fn detects_mode_inside_multi_param_list() {
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[?1049;1000l");
        want.extend_from_slice(GUARD);
        assert_eq!(run(true, &[b"\x1b[?1049;1000l"]), want);
    }

    #[test]
    fn split_across_chunks() {
        let mut want = Vec::new();
        want.extend_from_slice(b"x\x1b[?1049l");
        want.extend_from_slice(GUARD);
        want.extend_from_slice(b"y");
        assert_eq!(run(true, &[b"x\x1b[", b"?10", b"49", b"l", b"y"]), want);
    }

    #[test]
    fn split_non_matching_sequence_across_chunks() {
        assert_eq!(run(true, &[b"\x1b[", b"?20", b"04l"]), b"\x1b[?2004l");
    }

    #[test]
    fn truncated_tail_flushed_on_next_feed() {
        let mut g = AltScreenGuard::new(true);
        let mut out = Vec::new();
        g.feed(b"pre\x1b[?10", &mut out);
        assert_eq!(out, b"pre");
        g.feed(b"49l", &mut out);
        let mut want = Vec::new();
        want.extend_from_slice(b"pre\x1b[?1049l");
        want.extend_from_slice(GUARD);
        assert_eq!(out, want);
    }

    #[test]
    fn non_csi_escape_passthrough() {
        // \x1b7 / \x1b8 / \x1bc 等非 CSI 序列原样放行
        assert_eq!(run(true, &[b"\x1b7\x1b8\x1bc"]), b"\x1b7\x1b8\x1bc");
    }
}
