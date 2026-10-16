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

/// Tier 2 of the post-commit judgement: how much wider than `tail_chars` the
/// judged span may grow. A unit of length U needs about U*(max_repeats+1)
/// characters of span before R2 and R4 can both hold, so the tier-1 span caps
/// post-commit detection at `tail_chars/(max_repeats+1)`. Widening the span is
/// the only way past that cap.
pub const TIER2_SPAN_MULT: usize = 3;
/// Extra repeats tier 2 demands ON TOP of `max_repeats`. Growing the span
/// lowers the bar for R4 (the repetition only has to dominate a wider span),
/// so tier 2 pays for the wider net with strictly more repetitions. This is
/// what keeps the false-positive rate flat as the span grows.
pub const TIER2_EXTRA_REPS: u32 = 2;

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
#[derive(Debug, Clone)]
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
    /// Wall-clock ceiling on the pre-commit holdback, in milliseconds. 0 means
    /// no cap (the holdback is then purely `min_chars` characters). See
    /// `config.rs` for why this exists: the character count says nothing about
    /// how long filling it will take.
    pub hold_max_ms: u64,
    /// Correct-and-continue instead of discard-and-retry.
    pub continue_enabled: bool,
    /// Whether the THINKING stream is judged on its own guard.
    pub reasoning_enabled: bool,
    /// Upper bound on corrections for ONE answer. Clamped to a small number:
    /// each round is a full upstream call, and a model that loops three times
    /// in a row is not going to be saved by a fourth prompt.
    pub continue_max: u32,
    /// The correction prompt, already validated (never empty).
    pub continue_prompt: String,
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

        // 0 disables the cap. An absurdly large value is clamped rather than
        // rejected: a config typo must never cost a client request, and a cap
        // of 60 s is already longer than any sane holdback.
        let hold_max_ms = if rc.loop_guard_hold_max_ms == 0 { 0 } else { rc.loop_guard_hold_max_ms.clamp(1, 60_000) };
        if hold_max_ms != rc.loop_guard_hold_max_ms {
            tracing::warn!(configured = rc.loop_guard_hold_max_ms, used = hold_max_ms, "loop_guard_hold_max_ms out of range; clamped");
        }

        // A correction round is an extra upstream call per answer, so the cap
        // is clamped rather than trusted: 0 means "never continue" (the old
        // discard-and-retry behaviour), and the upper bound keeps one looping
        // answer from multiplying the request's cost.
        let continue_max = rc.loop_guard_continue_max.min(LOOP_CONTINUE_MAX_CAP);
        if continue_max != rc.loop_guard_continue_max {
            tracing::warn!(configured = rc.loop_guard_continue_max, used = continue_max, "loop_guard_continue_max out of range; clamped");
        }
        // An empty prompt would send the model a bare "continue" with no reason,
        // which is the one thing the correction must never be: the reason is what
        // stops it restarting the same loop.
        let continue_prompt = if rc.loop_guard_continue_prompt.trim().is_empty() {
            tracing::warn!("loop_guard_continue_prompt is empty; using the built-in prompt");
            d_continue_prompt()
        } else {
            rc.loop_guard_continue_prompt.clone()
        };

        Self {
            enabled, min_chars, repeat_min_chars, repeat_ratio, max_repeats, hold_max_ms,
            window_chars, tail_chars,
            continue_enabled: rc.loop_guard_continue_enabled,
            reasoning_enabled: rc.loop_guard_reasoning_enabled,
            continue_max,
            continue_prompt,
        }
    }
}

/// Hard ceiling on correction rounds per answer (see `LoopCfg::continue_max`).
pub const LOOP_CONTINUE_MAX_CAP: u32 = 5;

/// The built-in correction prompt, used when the configured one is empty. Kept
/// in sync with `config::d_loop_guard_continue_prompt` by construction (that
/// function calls this).
pub fn d_continue_prompt() -> String {
    "你刚才的输出出现了复读：同一段内容「{unit}」连续重复了 {repeats} 次，已被系统截断。请不要重复已经说过的内容，也不要道歉或解释，直接接着上面被打断的地方继续输出剩余内容。".to_string()
}

/// Fill `{unit}` / `{repeats}` in the correction prompt.
///
/// The unit is clamped here as well as in `LoopHit`: the prompt is user
/// configurable, so an unbounded `{unit}` would let a pathological repetition
/// balloon one request into megabytes.
pub fn render_continue_prompt(template: &str, hit: &LoopHit) -> String {
    let unit: String = hit.unit.chars().take(PROMPT_UNIT_MAX_CHARS).collect();
    template
        .replace("{unit}", &unit)
        .replace("{repeats}", &hit.repeats.to_string())
}

/// How much of the repeated unit is echoed into the correction prompt.
const PROMPT_UNIT_MAX_CHARS: usize = 60;

/// A detected degenerate repetition: `unit` was repeated `repeats` times and
/// covered `chars` characters at the tail of the window.
#[derive(Debug, Clone)]
pub struct LoopHit {
    /// The repeated unit (truncated to <= 120 characters).
    pub unit: String,
    pub repeats: u32,
    /// Characters covered by the repetition = repeats * unit_len. For R6 this
    /// counts only the NON-WHITESPACE characters, because R6 judges the
    /// collapsed span (see `detect_loose`).
    pub chars: usize,
    /// How many REAL characters at the tail of the judged span belong to the
    /// repetition (plus, for R6, the trailing remark that follows it).
    ///
    /// This is what lets a caller CUT the loop out of the answer instead of
    /// throwing the whole attempt away: `span[.. span_len - real_span]` is the
    /// text the model wrote BEFORE it started repeating. R1-R5 report the same
    /// value as `chars`; R6 differs because collapsing whitespace changes the
    /// length of the region it matched.
    pub real_span: usize,
}

/// Rolling character window plus the detection rules.
pub struct LoopGuard {
    cfg: LoopCfg,
    window: VecDeque<char>,
}

impl LoopGuard {
    pub fn new(cfg: &LoopCfg) -> Self {
        Self { cfg: cfg.clone(), window: VecDeque::new() }
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
        self.detect().or_else(|| self.detect_loose(self.window.len(), self.cfg.min_chars, self.cfg.max_repeats))
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

    /// The text the window holds, minus the last `exclude` characters.
    ///
    /// This is how a caller recovers "what the model had written BEFORE it
    /// started repeating": `real_span` counts back from the window tail, so
    /// dropping that many characters leaves exactly the pre-loop text. Used to
    /// tell the model where it was cut off, and to re-seed the window so the
    /// removed repetition cannot be re-detected.
    pub fn text_before(&self, exclude: usize) -> String {
        let len = self.window.len();
        let keep = len.saturating_sub(exclude);
        self.window.range(..keep).copied().collect()
    }

    /// Drop the window and re-seed it with the text that SURVIVED a loop cut.
    ///
    /// The repetition that was just removed must not stay in the window: the
    /// very next frame would be judged against it and reported as a fresh loop
    /// before the model had written a single character of the continuation.
    /// Only a bounded tail is kept, which is all a tail judgement needs.
    pub fn reseed(&mut self, tail: &str) {
        self.window.clear();
        let n = tail.chars().count();
        let skip = n.saturating_sub(self.cfg.tail_chars);
        let keep: String = tail.chars().skip(skip).collect();
        self.append(&keep);
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
    ///
    /// TWO TIERS, because one span cannot serve both ends of the trade-off:
    ///
    /// * Tier 1 judges exactly `tail_chars`, with the configured `max_repeats`.
    ///   It is the fast, sensitive path and keeps every existing behaviour.
    /// * Tier 2 judges a span up to `tail_chars * TIER2_SPAN_MULT` but demands
    ///   `max_repeats + TIER2_EXTRA_REPS` repetitions. Tier 1 alone cannot see a
    ///   unit longer than `tail_chars / (max_repeats + 1)`, because such a unit
    ///   cannot fit `max_repeats` copies inside the tail; that is exactly the
    ///   gap a model exploits by repeating a LONG sentence. Growing the span
    ///   without raising the repeat bar would make R4 easier to satisfy (the
    ///   repetition only has to dominate a wider span), which is why the extra
    ///   repeats are not optional: they are what keeps the false-positive rate
    ///   from rising just because the net got wider.
    ///
    /// The FLOOR above stays `tail_chars` on purpose. The floor answers "is
    /// there enough evidence to judge at all", and lowering it would let a
    /// three-word answer be judged. The SPAN answers "how much text may the
    /// judgement look at", and those are different questions that the previous
    /// design wrongly answered with one number.
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
        let tail = self.cfg.tail_chars;
        if let Some(h) = self
            .detect_span(tail, 2, self.cfg.max_repeats)
            .or_else(|| self.detect_loose(tail, 2, self.cfg.max_repeats))
        {
            return Some(h);
        }
        let span = self.window.len().min(tail.saturating_mul(TIER2_SPAN_MULT));
        let reps = self.cfg.max_repeats + TIER2_EXTRA_REPS;
        self.detect_span(span, 2, reps)
            .or_else(|| self.detect_loose(span, 2, reps))
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
    fn detect_loose(&self, n: usize, min_span: usize, min_repeats: u32) -> Option<LoopHit> {
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
        // Collapse whitespace: R6 judges shape, not layout. `real_at[k]` is the
        // index in `span` of the k-th surviving character, which is how a match
        // found on the collapsed text is mapped back onto the real one: without
        // it a caller could not tell where the repetition actually begins and
        // would have to discard the whole attempt (the behaviour this rule
        // exists to avoid).
        let mut dense: Vec<char> = Vec::with_capacity(span.len());
        let mut real_at: Vec<usize> = Vec::with_capacity(span.len());
        for (i, c) in span.iter().enumerate() {
            if !c.is_whitespace() {
                dense.push(*c);
                real_at.push(i);
            }
        }
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
                if (repeats as u32) < min_repeats {
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
            // Where the repetition really starts. Everything from there to the
            // end of the span is loop (plus the model's own trailing remark),
            // so a caller that cuts here keeps exactly the text written BEFORE
            // the loop -- the whole point of reporting a real span.
            real_span: span.len() - real_at[end - covered],
        })
    }

    /// R1-R5 over every candidate `unit_len` of the LAST `n` characters.
    /// Returns the best hit: the largest covered span, ties broken by the
    /// smallest unit length.
    fn detect(&self) -> Option<LoopHit> {
        self.detect_span(self.window.len(), self.cfg.min_chars, self.cfg.max_repeats)
    }

    /// The rules themselves, end-relative: every index below counts backwards
    /// from the window tail, so `n` is how many trailing characters are judged
    /// and `len` is how many the window actually holds (`n <= len`).
    fn detect_span(&self, n: usize, min_span: usize, min_repeats: u32) -> Option<LoopHit> {
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
            if (repeats as u32) < min_repeats {
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
            // R1-R5 match a contiguous suffix of the judged span, so the real
            // characters it covers are exactly the collapsed count.
            real_span: covered,
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

/// May the pre-commit holdback end?
///
/// The holdback exists so the guard has EVIDENCE before the first byte leaves:
/// a repetition can only be judged from characters that have actually arrived.
/// `min_chars` alone therefore makes the first-byte delay a function of the
/// model's generation speed — 64 characters is 160 ms at 400 chars/s but 32 s
/// at 2 chars/s. This returns true when EITHER
///
///   * `pending_len >= min_chars` — the evidence the guard needs has arrived, or
///   * `elapsed >= hold_max` — the client has waited long enough.
///
/// `hold_max == 0` disables the ceiling (the old, purely character-counted
/// behaviour). `elapsed == None` means no text frame has arrived yet, so there
/// is no wait to bound. When the ceiling wins, the text that arrives afterwards
/// is judged post-commit, which tier 2 of `judge_tail` still covers — detection
/// quality degrades, the first byte does not wait.
///
/// Kept as a pure function rather than an inline closure so the decision is
/// unit-testable without real time: an integration test would have to rely on
/// wall-clock scheduling, which is exactly the kind of test that passes or
/// fails by luck on a loaded machine.
pub fn holdback_done(
    pending_len: usize,
    min_chars: usize,
    hold_max: std::time::Duration,
    elapsed: Option<std::time::Duration>,
) -> bool {
    if pending_len >= min_chars {
        return true;
    }
    match (hold_max.is_zero(), elapsed) {
        (true, _) => false,          // ceiling disabled: wait for the characters
        (false, None) => false,      // no text yet, so no wait to bound
        (false, Some(e)) => e >= hold_max,
    }
}
#[cfg(test)]
mod tests {
    // ---- holdback ceiling (pure, no wall-clock dependency) ----

    /// The character target still wins when it is reached first: the ceiling
    /// must never cut the evidence short for a model fast enough to supply it.
    #[test]
    fn holdback_char_target_wins_when_reached_first() {
        let cap = std::time::Duration::from_millis(250);
        assert!(holdback_done(64, 64, cap, Some(std::time::Duration::from_millis(0))));
        assert!(holdback_done(65, 64, cap, Some(std::time::Duration::from_millis(10))));
    }

    /// The ceiling wins when the characters have not arrived in time — this is
    /// the whole point of the field, and the case a slow model hits.
    #[test]
    fn holdback_ceiling_wins_for_a_slow_model() {
        let cap = std::time::Duration::from_millis(250);
        assert!(holdback_done(12, 64, cap, Some(std::time::Duration::from_millis(250))));
        assert!(holdback_done(12, 64, cap, Some(std::time::Duration::from_millis(9999))));
    }

    /// Below both thresholds the answer is still being assembled: neither the
    /// evidence nor the clock has arrived, so the holdback must continue.
    #[test]
    fn holdback_continues_when_neither_threshold_is_met() {
        let cap = std::time::Duration::from_millis(250);
        assert!(!holdback_done(12, 64, cap, Some(std::time::Duration::from_millis(249))));
        assert!(!holdback_done(0, 64, cap, Some(std::time::Duration::from_millis(0))));
    }

    /// 0 disables the ceiling: the wait is then purely `min_chars` characters,
    /// which is the pre-existing behaviour and must stay reachable.
    #[test]
    fn holdback_zero_disables_the_ceiling() {
        let off = std::time::Duration::from_millis(0);
        assert!(!holdback_done(12, 64, off, Some(std::time::Duration::from_secs(3600))));
        assert!(holdback_done(64, 64, off, Some(std::time::Duration::from_secs(3600))));
    }

    /// Before any text frame arrives there is no wait to bound, so the ceiling
    /// cannot fire — otherwise the clock would start at the attempt and charge
    /// the client for upstream thinking time.
    #[test]
    fn holdback_ceiling_needs_a_first_text_frame() {
        let cap = std::time::Duration::from_millis(1);
        assert!(!holdback_done(0, 64, cap, None));
    }
    use super::*;

    fn cfg() -> LoopCfg {
        LoopCfg {
            enabled: true, min_chars: 64, repeat_min_chars: 24, repeat_ratio: 0.75,
            max_repeats: 4, window_chars: 4000, tail_chars: 128, hold_max_ms: 0,
            continue_enabled: false, reasoning_enabled: false,
            continue_max: 0, continue_prompt: String::new(),
        }
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

    /// `real_span` is what makes a loop CUTTABLE: it says how many characters at
    /// the tail of the judged span are the repetition, so everything before them
    /// is text worth keeping. If it were wrong the cut would either leave part of
    /// the loop in place or eat good prose.
    #[test]
    fn t26_real_span_locates_the_cut_for_r1_to_r5() {
        // Contiguous repetition: the collapsed span and the real span coincide.
        let mut g = LoopGuard::new(&cfg());
        let mut hit = None;
        for _ in 0..20 {
            if let Some(h) = g.push("好问题。") { hit = Some(h); break; }
        }
        let h = hit.expect("the short-cycle shape must be detected");
        assert_eq!(h.real_span, h.chars, "R1-R5 judge real characters: no collapsing");
        assert_eq!(h.chars, h.unit.chars().count() * h.repeats as usize);
        // The cut removes every COPY the detector counted and keeps exactly the
        // one before them — which is real answer text, not part of the loop. So
        // the kept text ends with a single unit, never with a run of them.
        let kept = g.text_before(h.real_span);
        assert_eq!(kept, h.unit, "only the first, un-repeated copy may survive: {kept:?}");
    }

    /// R6 collapses whitespace, so `chars` (collapsed) and `real_span` (real) are
    /// DIFFERENT numbers. Using the collapsed one to cut would remove the wrong
    /// amount of text, which is why the two are separate fields.
    #[test]
    fn t27_real_span_counts_real_characters_under_r6() {
        let cfg = cfg();
        let hello = "hello ";
        let text = format!("{0}\n\n{0}\n\n（共 20 遍）", hello.repeat(11));
        let chars: Vec<char> = text.chars().collect();
        let mut g = LoopGuard::new(&cfg);
        let mut hit = None;
        for chunk in chars.chunks(12) {
            let piece: String = chunk.iter().collect();
            if let Some(h) = g.push(&piece) { hit = Some(h); break; }
        }
        let h = hit.expect("the real-world shape must be detected");
        // R6 collapses whitespace, so the REAL length of the matched region is
        // strictly larger than the collapsed one it reported. This is the whole
        // reason the two are separate fields: cutting by `chars` would remove
        // the wrong number of characters.
        assert!(h.real_span > h.chars,
            "R6 collapses spaces, so the real span must exceed the collapsed one (real={} collapsed={})",
            h.real_span, h.chars);
        // The cut removes the duplicate run AND the model's own concluding
        // remark that follows it — a remark is a self-summary OF the loop, so
        // it goes with it.
        let kept = g.text_before(h.real_span);
        assert!(!kept.contains("（共 20 遍）"), "the trailing remark must go with the loop: {kept:?}");
        // WINDOW-BOUNDED, and that is honest rather than ideal: R1 lets the
        // guard fire as soon as it has `min_chars` of evidence, so for this
        // shape the judged window contains only the run itself and there is no
        // earlier copy left inside it to keep. The caller sees an empty `kept`
        // and falls back to trying another candidate instead of emitting a
        // fragment of the repetition as an answer.
        assert_eq!(kept, "", "at n=min_chars the window holds only the run");
        assert_eq!(h.real_span, g.window_len(), "the whole judged window was the loop");
    }

    /// The window must not report the repetition it just cut as a fresh one.
    #[test]
    fn t28_reseed_forgets_the_removed_loop() {
        let mut g = LoopGuard::new(&cfg());
        let mut hit = None;
        for _ in 0..20 {
            if let Some(h) = g.push("好问题。") { hit = Some(h); break; }
        }
        let h = hit.expect("detected");
        let kept = g.text_before(h.real_span);
        g.reseed(&kept);
        // The continuation's first characters must not be judged against the
        // repetition that was just removed.
        assert!(g.push_tail("新的内容").is_none(), "the cut loop must not be re-detected");
        assert!(g.window_len() <= cfg().tail_chars, "the window keeps only a bounded tail");
    }

    // ---- the real loop the user pasted from another session ----

    /// The user reported a REAL repetition from another session:
    ///
    /// ```text
    /// OK.
    ///
    /// ...
    ///
    /// ...
    ///
    /// OK.
    /// ...
    /// ```
    ///
    /// Two readings of that paste are indistinguishable in a chat diff, so BOTH
    /// are pinned. They are different rule interactions: reading A exercises the
    /// R6 blank-line fallback, reading B the strict R1-R5 path, and getting one
    /// right says nothing about the other.
    #[test]
    fn t29_reported_loop_blank_line_reading_is_caught() {
        // Reading A: blank lines between every line -> unit is 15 chars.
        let text = "OK.\n\n...\n\n...\n\n".repeat(100);
        let hit = hit(&text).expect("the reported loop must be detected");
        assert!(hit.repeats >= 4, "{hit:?}");
        assert!(hit.real_span >= 24, "{hit:?}");
    }

    #[test]
    fn t30_reported_loop_single_newline_reading_is_caught() {
        // Reading B: single newlines, no blank line -> unit is 13 chars, so R6
        // (which REQUIRES a blank line) cannot help and R1-R5 must do the work.
        let text = "OK.\n...\n...\n".repeat(200);
        let hit = hit(&text).expect("must be detected");
        assert!(hit.repeats >= 4, "{hit:?}");
    }

    #[test]
    fn t31_reported_loop_is_caught_post_commit_too() {
        // Post-commit the only remedy is to stop emitting, so the blank-line
        // reading must also be seen by the tail judge.
        let mut g = LoopGuard::new(&cfg());
        let text = "OK.\n\n...\n\n...\n\n".repeat(100);
        let h = g.push_tail(&text).expect("must be detected post-commit");
        assert!(h.repeats >= 4, "{h:?}");
    }

    /// Tier 2 must not become a false-positive machine. Growing the judged
    /// span lowers the R4 bar (the repetition only has to dominate a wider
    /// span), so this pins the cases where a WIDER span would start cutting
    /// text a human wrote on purpose.
    #[test]
    fn t33_tier2_does_not_cut_legitimate_text() {
        let cfg = cfg();
        // 1. A markdown table whose rows legitimately share a shape. Every row
        //    is IDENTICAL here, which is the worst case: it is a real table a
        //    model may emit, and R2/R3/R4 are all satisfiable inside a wide span.
        let table = format!("| 名称 | 状态 |\n|---|---|\n{}", "| 未知 | 待确认 |\n".repeat(12));
        let mut g = LoopGuard::new(&cfg);
        // Tier 1 already cuts this (pre-existing behaviour, measured). The test
        // records the CURRENT answer so a future change to tier 1 is visible,
        // rather than asserting a value tier 2 was never responsible for.
        let tier1 = g.push_tail(&table).is_some();
        assert!(tier1, "tier 1 cuts a fully-identical table; if this changes, revisit the docs");

        // 2. Prose where every sentence is genuinely different must survive. This
        //    is the case tier 2 could plausibly break by widening the net.
        let prose: String = (0..70)
            .map(|i| format!("第 {i} 条记录描述了编号 {i} 的渠道在 {} 秒内的表现，与其它条目互不相同。", i * 7))
            .collect::<Vec<_>>()
            .join("\n");
        let mut g2 = LoopGuard::new(&cfg);
        assert!(g2.push_tail(&prose).is_none(), "distinct prose must never be cut");

        // 3. Config blocks that repeat the same keys with different values are
        //    the shape a model uses for multi-channel setup; the VALUES differ,
        //    so no unit is repeated and nothing may be cut.
        let cfgtext: String = (0..12)
            .map(|i| format!("[channel.{i}]\nretries = {}\ntimeout_ms = {}\nenabled = true", i % 4 + 1, 20_000 + i * 500))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut g3 = LoopGuard::new(&cfg);
        assert!(g3.push_tail(&cfgtext).is_none(), "distinct config values must never be cut");

        // 4. The case that ACTUALLY depends on TIER2_EXTRA_REPS. When the whole
        //    window is shorter than the tier-2 span, R4 is trivially satisfied
        //    (the repetition IS the entire span), so R2 alone decides -- and the
        //    extra repeats are the only thing standing between a legitimate
        //    5-line answer and a false cut. Verified by fault injection: with
        //    `TIER2_EXTRA_REPS` folded back into `max_repeats`, all three of
        //    these are CUT, which is why this case is pinned.
        let repeated_legit: [(&str, String); 3] = [
            (
                "a 5-line log excerpt",
                "2026-10-15 20:41:00 INFO heartbeat ok channel=ch_alpha\n".repeat(5),
            ),
            (
                "a 5-line chorus",
                "这是我们的时代，这是我们的选择，一起走下去不要回头，直到天亮。\n".repeat(5),
            ),
            (
                "a 5-line checklist",
                "请确认所有渠道的配置项均已正确填写，确认无误之后再继续下一步。\n".repeat(5),
            ),
        ];
        for (name, text) in &repeated_legit {
            let mut g4 = LoopGuard::new(&cfg);
            assert!(
                g4.push_tail(text).is_none(),
                "{name} repeats one line 5 times and must survive: tier 2 needs \
                 {} repeats, not {}",
                cfg.max_repeats + TIER2_EXTRA_REPS,
                cfg.max_repeats
            );
        }
    }

    /// Tier 2 must actually be doing the work, not tier 1 in disguise.
    ///
    /// A 45-character unit is the real one measured from a live model. Tier 1
    /// cannot see it (25 is its ceiling); only tier 2 can. If tier 2 were
    /// accidentally disabled, t32 would still pass for the 24-char case, so
    /// this test isolates the band that ONLY tier 2 covers.
    #[test]
    fn t34_tier2_is_what_catches_the_long_unit() {
        let cfg = cfg();
        let tail = cfg.tail_chars;
        let bound1 = tail / (cfg.max_repeats as usize + 1);
        let unit = distinct_chars(45, 1);
        assert!(unit.chars().count() > bound1, "45 must be above the tier-1 bound {bound1}");

        // Post-commit: tier 2 must catch it.
        let mut g = LoopGuard::new(&cfg);
        let h = g
            .push_tail(&unit.repeat(60))
            .expect("tier 2 must catch a 45-char unit post-commit");
        assert!(h.repeats >= cfg.max_repeats + TIER2_EXTRA_REPS, "tier 2 demands more repeats: {h:?}");
        assert_eq!(h.unit, distinct_chars(45, 1).chars().take(UNIT_SNIPPET_CHARS).collect::<String>());

        // Tier 1 alone must NOT catch it, which is the gap tier 2 exists to close.
        // Isolate tier 1 by stopping the window at exactly `tail` characters:
        // there tier 1 has all the span it will ever get, while tier 2 has none
        // of its extra width yet. 128 chars of a 45-char unit is 2 whole copies,
        // and R2 needs 4, so tier 1 is structurally unable to fire.
        let text: String = unit.repeat(60);
        let chars: Vec<char> = text.chars().collect();
        let mut g1 = LoopGuard::new(&cfg);
        let mut fed = 0usize;
        while fed < tail {
            let end = (fed + 12).min(tail);
            let piece: String = chars[fed..end].iter().collect();
            assert!(
                g1.push_tail(&piece).is_none(),
                "tier 1 must not fire on a 45-char unit at {fed} chars; the band is 24..=25"
            );
            fed = end;
        }
        assert_eq!(g1.window_len(), tail, "tier 1 got its whole span and still saw nothing");
        // Past the tier-1 span the SAME text is caught, so the miss above was the
        // band and not a broken guard.
        let mut g2 = LoopGuard::new(&cfg);
        assert!(g2.push_tail(&text).is_some(), "tier 2 catches what tier 1 cannot");
    }

    /// The MEASURED post-commit unit-length boundary, pinned so it cannot
    /// silently move. This documents the detection band, which is a real
    /// limitation at the top end and a deliberate trade-off at the bottom.
    ///
    /// The post-commit detection band is bounded at BOTH ends, and this pins
    /// the upper one for each tier.
    ///
    /// Tier 1 judges `tail_chars` and needs `max_repeats` copies, so a unit of
    /// length U needs `tail/U - 1 >= max_repeats` -- i.e. `U <= tail/(max_repeats+1)`
    /// = 25. Tier 2 judges `tail * TIER2_SPAN_MULT` and needs
    /// `max_repeats + TIER2_EXTRA_REPS` copies, so its bound is
    /// `tail*mult/(max_repeats+extra+1)` = 384/7 = 54.
    ///
    /// R3 (`covered >= repeat_min_chars`) closes the other end, so the
    /// detectable band post-commit is roughly 24..=54 chars. Before tier 2
    /// existed the upper bound was 25, which is why a model repeating a
    /// 45-character sentence leaked its whole answer.
    ///
    /// Measured on the deployed gateway with 12-char frames: units of 8/16/24/25
    /// were cut; 22, 23, 26, 27, 28 and larger leaked the full 4000 characters.
    #[test]
    fn t32_post_commit_unit_length_boundary_is_pinned() {
        let cfg = cfg();
        let tail = cfg.tail_chars;
        let reps1 = cfg.max_repeats as usize;
        let reps2 = cfg.max_repeats as usize + TIER2_EXTRA_REPS as usize;
        let span2 = tail * TIER2_SPAN_MULT;

        let bound1 = tail / (reps1 + 1);
        let bound2 = span2 / (reps2 + 1);
        assert_eq!(bound1, 25, "the tier-1 boundary moved");
        assert_eq!(bound2, 54, "the tier-2 boundary moved");

        // The unit must be DISTINCT characters. A unit of one repeated character
        // has a length-1 sub-unit that also matches, and R2 then trivially passes
        // -- which would make this test assert nothing about the boundary.
        let short = distinct_chars(bound1 - 1, 1);
        let mut g = LoopGuard::new(&cfg);
        assert!(g.push_tail(&short.repeat(40)).is_some(), "24 distinct chars is inside the band");

        // Between the two bounds: tier 2 is what catches it, and that is the
        // whole point of the change. 45 is the real unit measured from a live
        // model that repeated "Let me write./Here./Final./..." 60 times.
        for u in [bound1 + 1, 45, bound2] {
            let mid = distinct_chars(u, 1);
            let mut gm = LoopGuard::new(&cfg);
            assert!(
                gm.push_tail(&mid.repeat(60)).is_some(),
                "unit {u} is between the tier-1 bound {bound1} and the tier-2 bound {bound2}; \
                 tier 2 must catch it post-commit"
            );
        }

        // Above the tier-2 bound: not detectable post-commit, by construction.
        let long = distinct_chars(bound2 + 8, 1);
        let mut g2 = LoopGuard::new(&cfg);
        assert!(
            g2.push_tail(&long.repeat(40)).is_none(),
            "units longer than {bound2} cannot reach R2 inside {span2} chars"
        );

        // The SAME long unit IS caught before commit, where the whole window is
        // judged. That asymmetry is why this gap is post-commit only.
        let mut g3 = LoopGuard::new(&cfg);
        assert!(g3.push(&long.repeat(40)).is_some(), "pre-commit judges the whole window");
    }
}
