//! Loop guard: internal detection of degenerate (repeating) model output.
//!
//! A model that falls into a repetition loop streams the same short unit over
//! and over. This module watches the tail of a rolling character window and
//! reports a hit once that repetition is clearly useless output. The detection
//! lives inside the gateway's PRE-COMMIT window, so a hit costs the client zero
//! bytes and the retry that follows stays invisible (see `docs/loopguard-spec.md`).
//!
//! `enabled == false` must mean *no buffering at all* (`min_chars == 0`), not a
//! muted detector: the holdback is exactly what delays the first byte, so a
//! switch that keeps the delay while removing the detection would be a lie.

use crate::config::RetryConfig;
use std::collections::VecDeque;

/// Diagnostics only: at most this many characters of the repeated unit are
/// copied into `LoopHit::unit` (char-boundary safe by construction).
const UNIT_SNIPPET_CHARS: usize = 120;

/// R6 (`detect_loose`) bounds. The repeated unit is bounded so the scan stays
/// cheap on a long window, and the trailing remark is bounded so it can never
/// itself become the "repetition" R6 finds.
const LOOSE_UNIT_MAX_CHARS: usize = 128;
const LOOSE_TRAILER_CHARS: usize = 32;

/// Does this span contain a blank line -- a line break with only whitespace
/// after it? A single line break is ordinary formatting; a blank line inside a
/// repetition means layout interrupted content, which is what R6 keys on.
fn has_blank_line(chars: &[char]) -> bool {
    let mut seen_break = false;
    for &ch in chars {
        match ch {
            '\n' => {
                if seen_break {
                    return true;
                }
                seen_break = true;
            }
            c if c.is_whitespace() => {}
            _ => seen_break = false,
        }
    }
    false
}

/// Runtime configuration for degenerate-repeat detection, normalized from
/// `RetryConfig` and clamped into a safe range.
#[derive(Debug, Clone, Copy)]
pub struct LoopCfg {
    pub enabled: bool,
    pub min_chars: usize,
    pub repeat_min_chars: usize,
    pub repeat_ratio: f64,
    pub max_repeats: u32,
    pub window_chars: usize,
    /// How much of the answer's TAIL is judged once the answer has already been
    /// committed. Pre-commit a hit is free (the whole attempt is discarded), so
    /// the guard may consider its entire window; post-commit the bytes are gone
    /// and the only remedy is truncation, so the evidence must be recent and
    /// overwhelming. Bounding the judged span is what makes R4 read as "the
    /// recent tail is dominated by one repetition" instead of "the repetition
    /// has taken over a 4000-character window", which would let the loop run for
    /// thousands of characters before it is caught.
    pub tail_chars: usize,
}

impl LoopCfg {
    /// Normalize from `RetryConfig`. Illegal values are clamped to defaults with
    /// a `tracing::warn!` — never panic, never return an error: a config typo
    /// must never cost a client request.
    pub fn from_retry(rc: &RetryConfig) -> Self {
        let window_chars = rc.loop_guard_window_chars.clamp(64, 65_536);
        if window_chars != rc.loop_guard_window_chars {
            tracing::warn!(configured = rc.loop_guard_window_chars, used = window_chars, "loop_guard_window_chars out of range; clamped");
        }

        let enabled = rc.loop_guard_enabled;
        let mut min_chars = rc.loop_guard_min_chars.min(window_chars);
        if min_chars != rc.loop_guard_min_chars {
            tracing::warn!(configured = rc.loop_guard_min_chars, used = min_chars, "loop_guard_min_chars exceeds the window; clamped");
        }
        if !enabled {
            // Disabled == no holdback: the first frame commits immediately, i.e.
            // the behaviour from before this feature existed.
            min_chars = 0;
        }

        let repeat_min_chars = rc.loop_guard_repeat_min_chars.clamp(1, window_chars);
        if repeat_min_chars != rc.loop_guard_repeat_min_chars {
            tracing::warn!(configured = rc.loop_guard_repeat_min_chars, used = repeat_min_chars, "loop_guard_repeat_min_chars out of range; clamped");
        }

        let max_repeats = rc.loop_guard_max_repeats.max(2);
        if max_repeats != rc.loop_guard_max_repeats {
            tracing::warn!(configured = rc.loop_guard_max_repeats, used = max_repeats, "loop_guard_max_repeats below 2; clamped");
        }

        let configured_ratio = rc.loop_guard_repeat_ratio;
        let repeat_ratio = if !configured_ratio.is_finite() || configured_ratio <= 0.0 || configured_ratio > 1.0 {
            tracing::warn!(configured = configured_ratio, used = 0.75_f64, "loop_guard_repeat_ratio invalid; using default");
            0.75
        } else {
            configured_ratio
        };

        // The judged tail is clamped against the window it slices: a tail wider
        // than the window would be meaningless, and one so narrow that no unit
        // fits (2 characters) could never satisfy R2.
        let tail_chars = rc.loop_guard_tail_chars.clamp(16, window_chars);
        if tail_chars != rc.loop_guard_tail_chars {
            tracing::warn!(configured = rc.loop_guard_tail_chars, used = tail_chars, "loop_guard_tail_chars out of range; clamped");
        }

        Self { enabled, min_chars, repeat_min_chars, repeat_ratio, max_repeats, window_chars, tail_chars }
    }
}

/// A detected degenerate repetition: `unit` was repeated `repeats` times and
/// covered `chars` characters at the tail of the window.
#[derive(Debug, Clone)]
pub struct LoopHit {
    /// The repeated unit (truncated to <= 120 characters).
    pub unit: String,
    pub repeats: u32,
    /// Characters covered by the repetition = repeats * unit_len.
    pub chars: usize,
}

/// Rolling character window plus the detection rules.
pub struct LoopGuard {
    cfg: LoopCfg,
    window: VecDeque<char>,
}

impl LoopGuard {
    pub fn new(cfg: &LoopCfg) -> Self {
        Self { cfg: *cfg, window: VecDeque::new() }
    }

    /// Current window length (diagnostics/tests). Never exceeds `window_chars`.
    pub fn window_len(&self) -> usize {
        self.window.len()
    }

    /// Append a text delta; returns a hit when the window tail is a degenerate
    /// repetition. With `enabled == false` this is always `None` and nothing is
    /// accumulated.
    pub fn push(&mut self, delta: &str) -> Option<LoopHit> {
        if !self.cfg.enabled {
            return None;
        }
        self.append(delta);
        // The strict suffix rule first -- it is the cheaper scan and the one the
        // existing tests pin down -- then the whitespace-tolerant rule it cannot
        // see. Both stay PRE-COMMIT: here a hit costs the client zero bytes.
        self.detect().or_else(|| self.detect_loose(self.window.len(), self.cfg.min_chars))
    }

    /// Post-commit counterpart of `push`: accumulate the delta and judge only
    /// the recent tail. The bytes are already on the wire by the time this is
    /// called, so the caller's only remedy is to stop emitting — which is why
    /// the evidence must be recent and overwhelming rather than merely present.
    pub fn push_tail(&mut self, delta: &str) -> Option<LoopHit> {
        if !self.cfg.enabled {
            return None;
        }
        self.append(delta);
        self.judge_tail()
    }

    /// Append characters and trim the window from the head to `window_chars`.
    fn append(&mut self, delta: &str) {
        for ch in delta.chars() {
            self.window.push_back(ch);
        }
        let cap = self.cfg.window_chars;
        if self.window.len() > cap {
            let excess = self.window.len() - cap;
            self.window.drain(..excess);
        }
    }

    /// Judge only the recent tail. Unlike `push`, which reports a hit the
    /// moment ANY suffix of the window degenerates, this fires only once the
    /// last `tail_chars` characters are dominated by one repetition: the span
    /// is the tail itself, so R4 reads "the recent output is a loop" rather
    /// than "the loop has swallowed the whole trailing window".
    ///
    /// That distinction is the whole point. R4 scales with the judged span, so
    /// judging a 4000-character window lets a loop run for ~0.75 * 4000 = 3000
    /// characters before it is caught — by which time the answer is ruined. A
    /// short span catches it in a few dozen characters instead, at the cost of
    /// needing the repetition to be *recent and total* rather than merely
    /// present.
    ///
    /// Returns `None` until the tail is saturated: a half-filled tail cannot be
    /// "dominated" by anything yet.
    pub fn judge_tail(&self) -> Option<LoopHit> {
        if !self.cfg.enabled {
            return None;
        }
        if self.window.len() < self.cfg.tail_chars {
            return None;
        }
        // R1 is deliberately NOT `min_chars` here: that threshold exists to
        // avoid judging before the PRE-COMMIT holdback has filled, and a
        // saturated tail is already the evidence. Reusing it would make a
        // `tail_chars` smaller than `min_chars` (e.g. 32 vs 64) silently
        // impossible to satisfy.
        self.detect_span(self.cfg.tail_chars, 2)
            .or_else(|| self.detect_loose(self.cfg.tail_chars, 2))
    }

    /// R6, the whitespace-tolerant fallback for R1-R5.
    ///
    /// The suffix rule needs the repeated unit to sit *contiguously* at the
    /// window tail, so a model that repeats a unit and then separates the runs
    /// with blank lines slips through (the newlines break the shift-compare on
    /// its first step). Worse, R5 skips candidate unit lengths inside a trailing
    /// blank run, which is exactly where such tail sits.
    ///
    /// R6 asks a slightly weaker question: ignore whitespace entirely, then look
    /// for one unit repeated at the tail of what is left. A model that emits
    /// "hello " 22 times around two blank lines is repeating the same unit; the
    /// blank lines are formatting, not evidence of thought.
    ///
    /// Deliberately narrow, so the weaker question does not become a
    /// false-positive machine:
    ///   * the span must contain a blank line (a line break with only
    ///     whitespace after it). A single newline is ordinary prose formatting;
    ///     a blank line inside a repetition is a strong "I am looping" signal,
    ///     and requiring it keeps every existing single-line case untouched.
    ///   * the threshold rules R2/R3/R4 are reused unchanged -- only R1's notion
    ///     of "contiguous" is relaxed, not the amount of evidence required.
    ///   * the repeated unit must contain an alphanumeric character, so runs of
    ///     pure punctuation or separators stay invisible (R5's concern, kept).
    ///   * a short TRAILER of characters may follow the repetition, because the
    ///     live shapes end with a remark like "（共 20 遍）" that the model adds
    ///     as a self-summary. Without it, the remark would hide the loop behind
    ///     itself -- the exact failure this rule exists to fix.
    fn detect_loose(&self, n: usize, min_span: usize) -> Option<LoopHit> {
        let len = self.window.len();
        let n = n.min(len);
        if n < 2 || n < min_span {
            return None; // R1
        }
        let start = len - n;
        let span = self.window.range(start..len).copied().collect::<Vec<char>>();
        if !has_blank_line(&span) {
            return None;
        }
        // Collapse whitespace: R6 judges shape, not layout.
        let dense: Vec<char> = span.into_iter().filter(|c| !c.is_whitespace()).collect();
        let m = dense.len();
        if m < 2 {
            return None;
        }
        // The remark after the repetition; bounded, so it cannot become the
        // repetition itself.
        let trailer_max = LOOSE_TRAILER_CHARS.min(m.saturating_sub(1));
        let mut best: Option<(usize, usize, usize)> = None; // (covered, unit_len, tail_end)
        for end in (m - trailer_max)..=m {
            let max_unit = (end / 2).min(LOOSE_UNIT_MAX_CHARS);
            for unit_len in 1..=max_unit {
                // Cheap pre-filter, exactly as in detect_span.
                if dense[end - 1] != dense[end - 1 - unit_len] {
                    continue;
                }
                let mut repeats = 0usize;
                while (repeats + 1) * unit_len <= end
                    && dense[end - (repeats + 1) * unit_len..end - repeats * unit_len]
                        == dense[end - unit_len..end]
                {
                    repeats += 1;
                }
                if repeats < 2 {
                    continue;
                }
                let covered = repeats * unit_len;
                if (repeats as u32) < self.cfg.max_repeats {
                    continue; // R2
                }
                if covered < self.cfg.repeat_min_chars {
                    continue; // R3
                }
                if (covered as f64) < self.cfg.repeat_ratio * (m as f64) {
                    continue; // R4, over the collapsed span
                }
                let unit = &dense[end - unit_len..end];
                if !unit.iter().any(|c| c.is_alphanumeric()) {
                    continue; // the R5 concern, restated for a collapsed span
                }
                if best.map_or(true, |(b, _, _)| covered > b) {
                    best = Some((covered, unit_len, end));
                }
                if covered == m {
                    break;
                }
            }
        }
        best.map(|(covered, unit_len, end)| LoopHit {
            unit: dense[end - unit_len..end].iter().collect::<String>().chars().take(UNIT_SNIPPET_CHARS).collect(),
            repeats: (covered / unit_len) as u32,
            chars: covered,
        })
    }

    /// R1-R5 over every candidate `unit_len` of the LAST `n` characters.
    /// Returns the best hit: the largest covered span, ties broken by the
    /// smallest unit length.
    fn detect(&self) -> Option<LoopHit> {
        self.detect_span(self.window.len(), self.cfg.min_chars)
    }

    /// The rules themselves, end-relative: every index below counts backwards
    /// from the window tail, so `n` is how many trailing characters are judged
    /// and `len` is how many the window actually holds (`n <= len`).
    fn detect_span(&self, n: usize, min_span: usize) -> Option<LoopHit> {
        let len = self.window.len();
        debug_assert!(n <= len, "judged span {n} exceeds the window {len}");
        if n < 2 || n < min_span {
            return None; // R1
        }
        let max_unit = n / 2;
        // R5, cheaply: a unit that lies entirely inside the trailing whitespace
        // run cannot contain a non-whitespace character, so the scan can start
        // above that run. Without this, a long run of blanks (indentation,
        // padding) would make every push rescan the whole window for nothing.
        // Only the judged span's own trailing whitespace matters, so a tail
        // that ends inside a blank run is skipped for the same reason the
        // whole-window scan skips it.
        let first_unit_len = self.trailing_whitespace_within(n) + 1;
        let mut best: Option<(usize, usize)> = None; // (characters covered, unit_len)
        for unit_len in first_unit_len..=max_unit {
            // Cheap pre-filter: the unit must repeat at the very tail, otherwise
            // the span is 0 and no rule can pass.
            if self.window[len - 1] != self.window[len - 1 - unit_len] {
                continue;
            }
            let span = self.match_span(unit_len, n, len);
            let repeats = span / unit_len;
            if repeats < 2 {
                continue; // max_repeats is clamped to >= 2, so R2 cannot pass
            }
            let covered = repeats * unit_len;
            if (repeats as u32) < self.cfg.max_repeats {
                continue; // R2
            }
            if covered < self.cfg.repeat_min_chars {
                continue; // R3
            }
            if (covered as f64) < self.cfg.repeat_ratio * (n as f64) {
                continue; // R4
            }
            // R5 is already guaranteed: unit_len > trailing whitespace run.
            if best.map_or(true, |(b, _)| covered > b) {
                best = Some((covered, unit_len));
            }
            if covered == n {
                break; // cannot be improved
            }
        }
        best.map(|(covered, unit_len)| LoopHit {
            unit: self.window.range(len - unit_len..len).take(UNIT_SNIPPET_CHARS).collect(),
            repeats: (covered / unit_len) as u32,
            chars: covered,
        })
    }

    /// Number of characters at the window tail that are all whitespace.
    fn trailing_whitespace(&self) -> usize {
        self.trailing_whitespace_within(self.window.len())
    }

    /// Same, but counting at most `within` characters: the judged span may be
    /// shorter than the window, and whitespace further back is irrelevant.
    fn trailing_whitespace_within(&self, within: usize) -> usize {
        let n = self.window.len().min(within);
        let mut k = 0usize;
        while k < n && self.window[self.window.len() - 1 - k].is_whitespace() {
            k += 1;
        }
        k
    }

    /// Longest suffix that matches "shift back by `unit_len`", i.e. the span
    /// definition: compare `W[len-1-j]` with `W[len-1-unit_len-j]`. The scan is
    /// bounded by the JUDGED span (`n`), so a short tail judges a short
    /// repetition even when a much longer identical run precedes it: the
    /// question is "is the recent output a loop", not "how long is the run".
    fn match_span(&self, unit_len: usize, n: usize, len: usize) -> usize {
        let mut j = 0usize;
        while j < n - unit_len && self.window[len - 1 - j] == self.window[len - 1 - unit_len - j] {
            j += 1;
        }
        j
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LoopCfg {
        LoopCfg { enabled: true, min_chars: 64, repeat_min_chars: 24, repeat_ratio: 0.75, max_repeats: 4, window_chars: 4000, tail_chars: 128 }
    }

    fn hit(text: &str) -> Option<LoopHit> {
        LoopGuard::new(&cfg()).push(text)
    }

    /// `count` distinct characters (CJK block), optionally strided.
    fn distinct_chars(count: usize, step: u32) -> String {
        (0..count as u32).filter_map(|i| char::from_u32(0x4E00 + i * step)).collect()
    }

    #[test]
    fn t01_single_char_repeat_hits() {
        let h = hit(&"啊".repeat(200)).expect("a single repeated character must hit");
        assert_eq!(h.unit.chars().count(), 1);
        assert!(h.repeats >= 4);
        assert_eq!(h.chars, h.repeats as usize);
    }

    #[test]
    fn t02_short_sentence_repeat_hits() {
        let h = hit(&"好的，我来处理。".repeat(30)).expect("a repeated sentence must hit");
        assert_eq!(h.unit.chars().count(), 8);
        assert!(h.repeats >= 4);
    }

    #[test]
    fn t03_pure_newline_repeat_does_not_hit() {
        assert!(hit(&"\n".repeat(200)).is_none(), "R5: blank-only repetition is not degenerate output");
    }

    #[test]
    fn t04_pure_space_repeat_does_not_hit() {
        assert!(hit(&" ".repeat(200)).is_none(), "R5: whitespace-only repetition is not degenerate output");
    }

    #[test]
    fn t05_english_word_repeat_hits() {
        assert!(hit(&"hello world ".repeat(40)).is_some());
    }

    #[test]
    fn t06_normal_prose_does_not_hit() {
        let prose = "网关把客户端的请求转交给上游模型，并在上游出现故障时保持对外的稳定表现。配置以内存快照的形式存在，热更新时原子地替换指针，正在处理的请求继续使用旧快照，因此不会读到半新半旧的配置。路由按层级展开：先尝试一级候选，再尝试二级候选，最后进入循环，每次尝试都受独立的时间预算约束，避免某个上游拖垮整个请求。健康登记表记录每个渠道与模型的连续失败次数，达到阈值后写入冷却记录，冷却期间该候选不会再被选中。如果所有候选都在冷却中，网关会在总截止时间允许的范围内等待，等待结束后重新构建候选计划。统计页面展示请求量、成功率与各渠道的用量，日志页面保留每次切换的原因与延迟，便于事后定位问题。代码里没有任何全局可变状态参与请求路径，所有共享结构都用引用计数包裹并保持不可变，这让人读起来放心。文档描述边界时应当直白：能覆盖的场景写清楚，覆盖不到的场景也要写清楚，而不是含糊其辞。运维人员排查问题时，最先看的是请求日志里的错误分类，其次是健康面板上的剩余冷却时间。测试覆盖成功、超时、连接失败、错误响应体与空响应等路径，尽量让每种失败都能被复现。这段文字没有规律地重复，只是普通的说明性段落，用来验证检测器不会对正常输出产生误判。如果把同一句话连续写上很多遍，那就属于需要被拦截的退化输出，而不是正常的中文表达。上游返回的状态码分为几类：鉴权失败、限流、服务端错误与网络错误，它们的退避策略并不相同。鉴权失败意味着密钥已经失效，重试再多次也不会成功，因此直接进入长期隔离。限流通常是暂时的，等待一小段时间后重新尝试往往能够成功。服务端错误介于两者之间，先按短冷却处理，连续出现多次再逐步升级。网络错误多半与本地链路或代理有关，需要结合连接超时与首字节超时一起判断。流式响应必须在收到第一帧内容之后才算提交，提交之前的任何失败都可以安全地换一个候选。提交之后的失败只能如实告诉客户端，因为此时部分内容已经发出，再拼接另一段回答会造成错乱。死循环检测属于提交之前的判断，它的代价是首字节被推迟几十个字符的时间。这个代价是真实存在的，文档里必须写明，不能因为数字小就省略。误判的代价是一次多余的上游调用，用户拿到的仍然是正常回答。窗口越大越能发现长句重复，但缓冲的字符也越多，两者需要权衡。默认值选择六十四，既能抓住短句循环，又不会让等待变得明显。配置项的名称保持自解释，避免使用缩写，让使用者在界面上就能看懂它的含义。每一项都有范围检查，越界的值会被夹到安全区间并记录一条警告，而不是让整个请求失败。这样的取舍是有意的：配置笔误的代价应该由管理员承担，而不是由正在等待回答的用户承担。日志里会保留原始的错误信息，方便对照上游的行为进行排查。";
        assert!(prose.chars().count() >= 1000, "the prose fixture must be substantial");
        assert!(hit(prose).is_none(), "normal prose must not be flagged");
    }

    #[test]
    fn t07_normal_code_does_not_hit() {
        let mut code = String::new();
        for i in 0..100usize {
            code.push_str(&format!("let value_{i} = transform_{i}(input, {i}); // step {i}\n"));
        }
        assert_eq!(code.lines().count(), 100);
        assert!(hit(&code).is_none(), "distinct code lines must not be flagged");
    }

    #[test]
    fn t08_below_min_chars_does_not_hit() {
        let short = "好的，我来处理。".repeat(7);
        assert!(short.chars().count() < 64);
        assert!(hit(&short).is_none(), "R1: nothing is judged before min_chars");
    }

    #[test]
    fn t09_window_trims_to_capacity() {
        let mut c = cfg();
        c.window_chars = 4000;
        let mut g = LoopGuard::new(&c);
        assert!(g.push(&distinct_chars(5000, 1)).is_none());
        assert_eq!(g.window_len(), 4000, "the window must be trimmed from the head");
        let unit = "甲乙丙丁戊己庚辛";
        let h = g.push(&unit.repeat(600)).expect("a loop after trimming must still hit");
        assert_eq!(h.unit, unit);
        assert_eq!(g.window_len(), 4000);
    }

    #[test]
    fn t10_multibyte_is_char_safe() {
        let mut g = LoopGuard::new(&cfg());
        let text = "中文🚀測試\u{200d}👨👩👧混\u{0301}合";
        for ch in text.chars() {
            let _ = g.push(&ch.to_string()); // every split point is a char boundary
        }
        assert_eq!(g.window_len(), text.chars().count());
        assert!(g.push(&"🎯".repeat(400)).is_some());
    }

    #[test]
    fn t11_disabled_never_hits_and_never_accumulates() {
        let mut c = cfg();
        c.enabled = false;
        c.min_chars = 0;
        let mut g = LoopGuard::new(&c);
        for _ in 0..50 {
            assert!(g.push("啊").is_none());
        }
        assert!(g.push(&"好的，我来处理。".repeat(30)).is_none());
        assert_eq!(g.window_len(), 0, "a disabled guard must not accumulate anything");
    }

    #[test]
    fn t12_mid_buffer_repetition_is_not_a_hit() {
        let mut g = LoopGuard::new(&cfg());
        let mut text = "啊".repeat(200);
        text.push_str(&distinct_chars(300, 3));
        assert!(g.push(&text).is_none(), "R4: the tail is normal text, so nothing repeats there");
    }

    #[test]
    fn t13_short_span_fails_repeat_min_chars() {
        let mut g = LoopGuard::new(&cfg());
        let mut text = distinct_chars(100, 7);
        text.push_str(&"abc".repeat(4));
        assert!(g.push(&text).is_none(), "R3: 12 covered characters is below repeat_min_chars");
    }

    #[test]
    fn t14_eight_char_unit_in_a_64_char_window_hits() {
        let mut c = cfg();
        c.window_chars = 64;
        c.min_chars = 64;
        let mut g = LoopGuard::new(&c);
        let h = g.push(&"好的，我来处理。".repeat(8)).expect("the end-to-end probe case must hit");
        assert_eq!(h.unit.chars().count(), 8);
        assert_eq!(h.repeats, 7);
        assert_eq!(h.chars, 56);
        assert_eq!(g.window_len(), 64);
    }

    #[test]
    fn t15_repeated_pushes_after_a_hit_are_safe() {
        let mut g = LoopGuard::new(&cfg());
        let unit = "好的，我来处理。";
        let mut hits = 0;
        for _ in 0..40 {
            if g.push(unit).is_some() {
                hits += 1;
            }
        }
        assert!(hits > 0);
        let _ = g.push(&"x".repeat(5000));
        assert_eq!(g.window_len(), 4000);
    }

    #[test]
    fn t16_from_retry_clamps_invalid_values() {
        // Disabled must remove the holdback entirely, not just the detection.
        let mut rc = RetryConfig::default();
        rc.loop_guard_enabled = false;
        let c = LoopCfg::from_retry(&rc);
        assert!(!c.enabled);
        assert_eq!(c.min_chars, 0, "a disabled switch must not keep the first-byte delay");

        // Illegal values must be clamped, never panic.
        let mut rc = RetryConfig::default();
        rc.loop_guard_repeat_ratio = f64::NAN;
        rc.loop_guard_window_chars = 0;
        rc.loop_guard_min_chars = usize::MAX;
        rc.loop_guard_repeat_min_chars = 0;
        rc.loop_guard_max_repeats = 0;
        let c = LoopCfg::from_retry(&rc);
        assert_eq!(c.window_chars, 64);
        assert_eq!(c.min_chars, 64);
        assert_eq!(c.repeat_min_chars, 1);
        assert_eq!(c.repeat_ratio, 0.75);
        assert_eq!(c.max_repeats, 2);
        assert!(c.enabled);
    }

    /// The R5 shortcut starts the scan at `tailBlank + 1`. This is its reverse
    /// risk: a window that ends in a long blank run must still catch a genuine
    /// repetition whose unit is exactly one character longer than that run.
    /// Unit = "甲" + 300 spaces (301 chars), tailBlank == 300, so the only
    /// viable unit_len is 301 -- the first one the shortcut does not skip.
    #[test]
    fn t17_long_trailing_blank_run_does_not_hide_a_later_hit() {
        let mut g = LoopGuard::new(&cfg());
        let unit = format!("甲{}", " ".repeat(300));
        assert_eq!(unit.chars().count(), 301);
        let h = g.push(&unit.repeat(13)).expect("a long trailing blank run must not hide the repetition");
        assert_eq!(h.repeats, 12);
        assert_eq!(h.chars, 3612);
        assert_eq!(g.window_len(), 3913);
        assert!(h.unit.starts_with('甲'), "the repeated unit must carry a non-whitespace character");
        assert_eq!(h.unit.chars().count(), 120, "the snippet is truncated for diagnostics only");
    }

    /// The critical boundary of the shortcut: `unit_len == tailBlank + 1`.
    /// "x  " x30 -> n = 90, tailBlank = 2, so unit_len = 3 must still be scanned.
    #[test]
    fn t18_unit_one_char_longer_than_the_tail_blank_run_hits() {
        let h = hit(&("x  ".to_string()).repeat(30)).expect("unit_len == tailBlank + 1 must still be scanned");
        assert_eq!(h.unit, "x  ");
        assert_eq!(h.repeats, 29);
        assert_eq!(h.chars, 87);
    }

    /// Same boundary with a wider unit: "ab     " x30 -> n = 210, tailBlank = 5,
    /// unit_len = 7.
    #[test]
    fn t19_wider_unit_than_the_tail_blank_run_hits() {
        let h = hit(&("ab     ".to_string()).repeat(30)).expect("a 7-char unit above a 5-char blank run must hit");
        assert_eq!(h.unit, "ab     ");
        assert_eq!(h.repeats, 29);
        assert_eq!(h.chars, 203);
    }

    /// Leading blanks must not hide the loop behind them, and R4 passes exactly
    /// on its boundary: 40 spaces + the frame x20, fed in 8-char frames, hits at
    /// n = 192 with covered = 144 = 0.75 * 192.
    #[test]
    fn t20_leading_blank_run_does_not_hide_a_later_hit() {
        let frame = "好的，我来处理。";
        let text = format!("{}{}", " ".repeat(40), frame.repeat(20));
        let chars: Vec<char> = text.chars().collect();
        assert_eq!(chars.len(), 200);
        let mut g = LoopGuard::new(&cfg());
        let mut found = None;
        for chunk in chars.chunks(8) {
            let piece: String = chunk.iter().collect();
            if let Some(h) = g.push(&piece) {
                found = Some(h);
                break;
            }
        }
        let h = found.expect("leading blanks must not hide the loop behind them");
        assert_eq!(h.unit, frame);
        assert_eq!(h.repeats, 18);
        assert_eq!(h.chars, 144);
        assert_eq!(g.window_len(), 192);
    }

    /// Boundary no-hit: 300 blanks + the frame x113 covers 896 characters, just
    /// below 0.75 * 1204 = 903, so this is correctly NOT a hit.
    #[test]
    fn t21_blank_run_plus_short_frame_run_stays_below_the_ratio() {
        let frame = "好的，我来处理。";
        let text = format!("{}{}", " ".repeat(300), frame.repeat(113));
        assert_eq!(text.chars().count(), 1204);
        assert!(hit(&text).is_none(), "covered 896 < 0.75 * 1204 = 903 is correctly not a hit");
    }

    /// R6 regression: the shapes that motivated it. Each replays the LIVE call
    /// R6 regression: the shapes that motivated it. Each replays the LIVE call
    /// sequence -- 12-char frames from the upstream, pre-commit push() until the
    /// window reaches min_chars, then post-commit push_tail(). A whole-string
    /// push gives different answers, because a pure-repetition moment can fall
    /// between frames; the frame stride is what the gateway actually sees.
    ///
    /// Shape c is the one the R1-R5 suffix rule cannot see at all: the model
    /// repeats a unit, inserts a blank line, repeats it again, and closes with a
    /// one-line remark. Before R6 it streamed to completion uncaught.
    #[test]
    fn t22_blank_line_separated_runs_are_caught_pre_commit() {
        let hello = "hello ";
        let frame = "好的，我来处理。";
        let cases: Vec<(&str, String)> = vec![
            (
                "两段 hello 复读 + 结尾说明",
                format!("{}\n\n{}\n\n（共 20 遍）", hello.repeat(11), hello.repeat(11)),
            ),
            (
                "两段中文帧复读 + 结尾说明",
                format!("{}\n\n{}\n\n（共 12 遍）", frame.repeat(6), frame.repeat(6)),
            ),
            (
                "两段各四次 + 结尾说明",
                format!("{}\n\n{}\n\n（共 8 遍）", frame.repeat(4), frame.repeat(4)),
            ),
        ];
        let mut stride = |text: &str| -> (bool, Option<LoopHit>) {
            let chars: Vec<char> = text.chars().collect();
            let mut g = LoopGuard::new(&cfg());
            let mut committed = false;
            for chunk in chars.chunks(12) {
                let piece: String = chunk.iter().collect();
                if !committed {
                    if let Some(h) = g.push(&piece) {
                        return (false, Some(h)); // pre-commit: the client saw nothing
                    }
                    if g.window_len() >= cfg().min_chars {
                        committed = true;
                    }
                } else if let Some(h) = g.push_tail(&piece) {
                    return (true, Some(h));
                }
            }
            (committed, None)
        };
        for (name, text) in cases {
            let (post, hit) = stride(&text);
            assert!(!post, "{name}: must be caught PRE-commit, so the retry stays invisible");
            let h = hit.unwrap_or_else(|| panic!("{name} must be caught, not streamed to completion"));
            assert!(h.repeats >= cfg().max_repeats, "{name}: repeats {} below max_repeats", h.repeats);
            assert!(h.chars >= cfg().repeat_min_chars, "{name}: covered {} below repeat_min_chars", h.chars);
            assert!(
                h.unit.chars().any(|c| c.is_alphanumeric()),
                "{name}: the unit must carry content, got {:?}",
                h.unit
            );
        }
    }

    /// The boundary of the above: an answer shorter than the pre-commit holdback
    /// is never judged, and that is correct -- a 59-character answer cannot ruin
    /// a response, and holding it back to inspect it would add latency for
    /// nothing. Recorded so the limit stays visible instead of looking like a bug.
    #[test]
    fn t23_short_blank_line_shapes_are_below_the_judged_window() {
        let frame = "好的，我来处理。";
        let text = format!("{}\n\n{}\n\n（共 6 遍）", frame.repeat(3), frame.repeat(3));
        assert_eq!(text.chars().count(), 59);
        assert!(text.chars().count() < cfg().min_chars);
        assert!(hit(&text).is_none(), "below min_chars nothing is judged");
    }

    /// R6 must not fire on ordinary multi-paragraph prose, lists, headings or
    /// separators: the blank-line requirement plus the reused R2/R3/R4
    /// thresholds are what keep it narrow. These are the closest normal shapes
    /// to a loop, replayed through the live frame stride.
    #[test]
    fn t24_blank_line_formatting_alone_is_not_a_hit() {
        let paragraph = |n: usize| {
            (0..n)
                .map(|i| {
                    format!(
                        "第 {} 段：这一段说明配置项的含义、默认值与取值范围，越界之后网关会如何处理。\n",
                        i + 1
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let cases: Vec<(&str, String)> = vec![
            ("多段散文 6 段", paragraph(6)),
            ("多段散文 12 段", paragraph(12)),
            (
                "清单 5 条相同",
                format!("{}\n\n共 5 项待确认", (0..5).map(|_| "- 待确认".to_string()).collect::<Vec<_>>().join("\n")),
            ),
            ("markdown 小节标题", "\n\n# 一、概述\n\n## 二、配置\n\n### 三、部署".to_string()),
            ("分隔线 + 空行说明", format!("{}\n\n以上是分隔线", "-".repeat(40))),
            (
                "正文 + 空行 + 四行不同表格",
                format!("补充说明如下。\n\n| 项 | 值 |\n|---|---|\n| a | 1 |\n| b | 2 |\n| c | 3 |"),
            ),
            ("纯换行", "\n".repeat(200)),
            ("纯空格", " ".repeat(200)),
        ];
        for (name, text) in cases {
            let chars: Vec<char> = text.chars().collect();
            let mut g = LoopGuard::new(&cfg());
            let mut committed = false;
            let mut fired = None;
            for chunk in chars.chunks(12) {
                let piece: String = chunk.iter().collect();
                if !committed {
                    if let Some(h) = g.push(&piece) {
                        fired = Some(format!("PRE n={} unit={:?} x{}", g.window_len(), h.unit, h.repeats));
                        break;
                    }
                    if g.window_len() >= cfg().min_chars {
                        committed = true;
                    }
                } else if let Some(h) = g.push_tail(&piece) {
                    fired = Some(format!("POST n={} unit={:?} x{}", g.window_len(), h.unit, h.repeats));
                    break;
                }
            }
            assert!(fired.is_none(), "{name} must not be judged a loop, but got {fired:?}");
        }
    }

    /// The post-commit half of R6 is load-bearing too. Once the answer is
    /// committed the bytes are gone, so the only question is how MUCH leaks
    /// before the truncation. Both cases below end in a blank line followed by
    /// more of the same unit, which the contiguous tail rule cannot join up;
    /// without the loose rule they either miss the tail window entirely or
    /// declare later with a narrower unit.
    #[test]
    fn t25_post_commit_loose_tail_catches_sooner() {
        let hello = "hello ";
        let prose: String = (0..80).map(|i| char::from_u32(0x4E00 + i).unwrap()).collect();
        // Blank line inside the last 128 characters: the contiguous rule sees
        // only the short run after the blank line and stays under R4.
        let text = format!("{prose}{}\n\n{}", hello.repeat(6), hello.repeat(14));
        let chars: Vec<char> = text.chars().collect();
        let mut g = LoopGuard::new(&cfg());
        let mut committed = false;
        let mut emitted = 0usize;
        let mut hit = None;
        for chunk in chars.chunks(12) {
            let piece: String = chunk.iter().collect();
            if !committed {
                assert!(g.push(&piece).is_none(), "this case must reach the post-commit path");
                if g.window_len() >= cfg().min_chars {
                    committed = true;
                    emitted = g.window_len();
                }
            } else {
                emitted += chunk.len();
                if let Some(h) = g.push_tail(&piece) {
                    hit = Some(h);
                    break;
                }
            }
        }
        let h = hit.expect("a blank line inside the tail window must not hide the repetition");
        assert_eq!(h.unit, "hello", "the collapsed unit has no trailing space");
        assert_eq!(h.repeats, 18);
        assert_eq!(h.chars, 90);
        assert_eq!(emitted, 192, "the leak is bounded by the tail window, not the whole tail");
    }
}
