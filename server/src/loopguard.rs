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
/// The post-commit ladder: `(span multiplier over tail_chars, extra repeats)`.
///
/// A single fixed span is a hard ceiling on the unit length the post-commit
/// path can ever see. `N` contiguous copies of a unit of `U` chars yield
/// `repeats = N - 1` (see `match_span`), and R4 requires the repetition to
/// dominate the span, so a span of `tail * m` cannot report any unit longer
/// than about `tail * m / (max_repeats + extra + 1)`. Every longer unit leaks
/// IN FULL, however many times it repeats -- the failure is total, not gradual.
///
/// Each further tier buys a wider span and pays for it with MORE required
/// repetitions. That payment is the point: R4 ("the repetition must dominate
/// the judged span") gets easier as the span grows, so widening the span
/// without raising the bar would quietly lower the evidence standard.
///
/// Unit-length ceiling per tier at the defaults (`tail_chars` 128,
/// `max_repeats` 4): **25 / 54 / 128 / 314**. The final ceiling is structural:
/// covering `U` needs a span of about `U * (reps + 1)`, and raising `reps`
/// further raises that span again, so with `window_chars` 4000 the bar cannot
/// be pushed much past ~314 without dropping below tier 3's evidence standard.
pub const LADDER: [(usize, u32); 4] =
    [(1, 0), (TIER2_SPAN_MULT, TIER2_EXTRA_REPS), (9, 4), (27, 6)];

/// Paraphrase repetition: how long a character run two paragraphs must SHARE
/// before they count as the same sentence reworded rather than two paragraphs
/// that merely share a template.
///
/// This is the discriminator, and it was chosen by measurement. Counting
/// repeated n-grams does NOT work: a real 4287-char answer contains a 12-char
/// unit 67 times (indentation and table rules), far more than a genuine
/// paraphrase loop. Longest-common-substring does work, because the two cases
/// differ structurally:
///
///   * paraphrase -> the SAME sentence is re-said, so a long run survives;
///   * table / code / checklist -> the same SKELETON is refilled with different
///     values, so the shared run stops at the skeleton.
///
/// Measured on 10 real model answers (933..4287 chars) and on the reported
/// 6-paragraph loop: every real answer peaked at 30 or below (a template line),
/// while the loop reached 52. 24 sits between them with margin on both sides.
const PARAPHRASE_LCS_CHARS: usize = 24;

/// How many CONSECUTIVE paragraphs must look paraphrased before it is a loop.
/// Two is deliberate: the reported case repeats six times, and demanding three
/// would let a model that re-says itself three times through unfixed. One
/// paragraph is never enough -- a single reworded paragraph is normal prose.
const PARAPHRASE_RUN_PARAS: usize = 2;

/// Two paragraphs count as restatements only if the shared run is NOT almost
/// the whole paragraph. `ratio > this` means the paragraphs are essentially
/// identical, which is a TABLE ROW, a LOG LINE or a CODE LINE -- legitimate
/// output that repeats a template with different values.
///
/// Measured: identical log/template lines sit at 0.95-1.00, while a genuine
/// rewording peaks at 0.67 (the reported loop) because the reworded half
/// still differs. Without this cap the detector cuts every log excerpt.
const PARAPHRASE_MAX_RATIO: f64 = 0.85;

/// A paragraph at least this long gets the lower bar below.
const PARAPHRASE_LONG_PARA_CHARS: usize = 80;

/// The shared-run bar for long paragraphs. Lower than `PARAPHRASE_LCS_CHARS`
/// because overlap is only meaningful relative to length: an 18-char run out of
/// a 100-char paragraph is a strong restatement signal, while the same run out
/// of a 30-char table row is incidental. Both numbers are pinned by `t41`
/// (real answers) and `t40` (the reported loop).
const PARAPHRASE_LCS_CHARS_LONG: usize = 18;

/// Only compare against the previous this-many paragraphs.
///
/// A model restating itself does so in a local run. Comparing the whole window
/// instead lets two far-apart paragraphs collide on a shared phrase: measured
/// on a real answer, p11 and p21 both contain `Content-Type: text/event-stream`
/// and share a 30-char run -- 10 paragraphs apart, and not a loop at all.
const PARAPHRASE_NEIGHBOUR_PARAS: usize = 6;

/// Paragraphs shorter than this are not compared: a short line cannot carry a
/// long shared run, so including them only adds cost without evidence.
const PARAPHRASE_MIN_PARA_CHARS: usize = 20;

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
        self.detect()
            .or_else(|| self.detect_loose(self.window.len(), self.cfg.min_chars, self.cfg.max_repeats))
            // Paraphrase repetition belongs here too, and NOT only post-commit.
            // A model that restates its plan six times does so at the very start
            // of the answer, so a post-commit-only rule fires after all six have
            // already been relayed: the client keeps the whole loop and the
            // correction merely appends more text. Judged here, a hit costs the
            // client zero bytes and the retry picks a different attempt.
            .or_else(|| self.detect_paraphrase())
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
        let tail = self.cfg.tail_chars;
        let len = self.window.len();
        if len < tail {
            return None;
        }
        // R1 is deliberately NOT `min_chars` here: that threshold exists to
        // avoid judging before the PRE-COMMIT holdback has filled, and a
        // saturated tail is already the evidence. Reusing it would make a
        // `tail_chars` smaller than `min_chars` (e.g. 32 vs 64) silently
        // impossible to satisfy.
        //
        // Narrowest tier first, returning on the first hit, so the leak stays
        // bounded by the narrowest span that can see the unit: a 24-char loop
        // is still cut at exactly the same place it was. Only units that no
        // narrower tier can see pay for the wider scan.
        let mut unit_lo = 1usize;
        for (mult, extra) in LADDER {
            let span = len.min(tail.saturating_mul(mult));
            let reps = self.cfg.max_repeats + extra;
            // The longest unit this span can report. Above it R2 cannot hold:
            // `N` copies give `repeats = N - 1 <= span / unit_len - 1`.
            let unit_hi = span / (reps as usize + 1);
            if unit_hi >= unit_lo {
                // Each tier scans only the band it newly covers. That is sound,
                // not merely cheap: a unit short enough for a narrower tier needs
                // FEWER copies there (`reps` grows with the tier), so a unit missed
                // by the narrow tier is missed by every wider one as well. The
                // ladder therefore costs about one scan of the widest span in
                // total, instead of one full scan per tier.
                if let Some(h) = self
                    .detect_span_band(span, 2, reps, unit_lo, unit_hi)
                    .or_else(|| self.detect_loose_band(span, 2, reps, unit_lo))
                {
                    return Some(h);
                }
            }
            if span == len {
                break; // the window is the binding constraint; wider is impossible
            }
            unit_lo = unit_hi.saturating_add(1).max(unit_lo);
        }
        // Last, and only after every verbatim rung missed: the same content
        // re-said in different words. Kept separate from the ladder because it
        // answers a different question (longest SHARED RUN, not an exact shift)
        // and must never pre-empt a precise hit, which reports a tighter cut.
        self.detect_paraphrase()
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
        self.detect_loose_band(n, min_span, min_repeats, 1)
    }

    /// `detect_loose` restricted to units of at least `unit_lo` characters.
    ///
    /// R6 keeps its own natural cap (half the span, `LOOSE_UNIT_MAX_CHARS`):
    /// R2 already makes a longer unit impossible, so tightening it here would
    /// only lose coverage. Only the lower bound is needed for banding.
    fn detect_loose_band(
        &self,
        n: usize,
        min_span: usize,
        min_repeats: u32,
        unit_lo: usize,
    ) -> Option<LoopHit> {
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
            for unit_len in unit_lo..=max_unit {
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
    /// R1-R5 over every candidate `unit_len` of the last `n` characters.
    fn detect_span(&self, n: usize, min_span: usize, min_repeats: u32) -> Option<LoopHit> {
        self.detect_span_band(n, min_span, min_repeats, 1, usize::MAX)
    }

    /// `detect_span` restricted to a band of candidate unit lengths.
    ///
    /// The ladder uses this to skip unit lengths a narrower tier already
    /// covered. `unit_hi` may only exclude units that this span could not
    /// report anyway (see `LADDER`), so the result matches `detect_span`
    /// whenever the band is wide enough.
    fn detect_span_band(
        &self,
        n: usize,
        min_span: usize,
        min_repeats: u32,
        unit_lo: usize,
        unit_hi: usize,
    ) -> Option<LoopHit> {
        let len = self.window.len();
        debug_assert!(n <= len, "judged span {n} exceeds the window {len}");
        if n < 2 || n < min_span {
            return None; // R1
        }
        let max_unit = (n / 2).min(unit_hi);
        // R5, cheaply: a unit that lies entirely inside the trailing whitespace
        // run cannot contain a non-whitespace character, so the scan can start
        // above that run. Without this, a long run of blanks (indentation,
        // padding) would make every push rescan the whole window for nothing.
        // Only the judged span's own trailing whitespace matters, so a tail
        // that ends inside a blank run is skipped for the same reason the
        // whole-window scan skips it.
        let first_unit_len = self.trailing_whitespace_within(n) + 1;
        let first_unit_len = first_unit_len.max(unit_lo);
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

    /// Paraphrase repetition: the same thing re-said in different words.
    ///
    /// The R1-R6 rules compare characters at an exact shift, so they can only
    /// see a unit repeated VERBATIM. A model that restates its whole plan six
    /// times with slightly different wording defeats all of them: no two
    /// paragraphs are identical, and the longest run any two of them share is
    /// ~26 chars, which is far below `repeat_min_chars`.
    ///
    /// The signal that survives rewording is the longest common substring:
    /// re-saying one sentence keeps a long spine intact, while two paragraphs
    /// that merely share a table/code skeleton stop matching at the skeleton
    /// (measured: 12-16 chars). See `PARAPHRASE_LCS_CHARS` for the numbers.
    ///
    /// HONEST LIMIT: the rule needs `PARAPHRASE_RUN_PARAS` restatements before
    /// it has evidence, so the FIRST statements always escape. Measured on the
    /// reported 6-paragraph case, pre-commit fires at 348/574 chars (61%), i.e.
    /// after three paragraphs are already relayed. Making it fire earlier would
    /// mean treating ONE reworded paragraph as proof, which cuts ordinary prose
    /// (a summary that restates the opening line). This is the same trade the
    /// verbatim rules make: a loop is always visible only after it has looped.
    ///
    /// Reports a hit only after `PARAPHRASE_RUN_PARAS` CONSECUTIVE paragraphs
    /// look paraphrased. `real_span` covers those paragraphs so the caller cuts
    /// exactly the redundant restatement and keeps the first one.
    fn detect_paraphrase(&self) -> Option<LoopHit> {
        let text: String = self.window.iter().copied().collect();
        // Paragraph boundaries: a blank line, or a line break. Both are what a
        // model uses to separate "one attempt at saying it" from the next.
        let paras: Vec<(usize, String)> = split_paragraphs(&text)
            .into_iter()
            .filter(|(_, p)| p.chars().count() >= PARAPHRASE_MIN_PARA_CHARS)
            .collect();
        if paras.len() <= PARAPHRASE_RUN_PARAS {
            return None;
        }
        let dense: Vec<Vec<char>> = paras
            .iter()
            .map(|(_, p)| p.chars().filter(|c| !c.is_whitespace()).collect())
            .collect();
        // Indices of paragraphs judged to be restatements. NOT required to be
        // consecutive: a model rewording itself drifts, so one paragraph in the
        // middle may share too little while the run as a whole is clearly a loop.
        let mut restated_at: Vec<usize> = Vec::new();
        for i in 1..paras.len() {
            let mut restated = false;
            // Only the RECENT previous paragraphs are compared: a restatement
            // is a local run, and a wider net starts matching shared phrases
            // across unrelated sections (see PARAPHRASE_NEIGHBOUR_PARAS).
            let from = i.saturating_sub(PARAPHRASE_NEIGHBOUR_PARAS);
            for j in from..i {
                if dense[j].len() < PARAPHRASE_LCS_CHARS
                    || dense[i].len() < PARAPHRASE_LCS_CHARS
                {
                    continue;
                }
                let shared = lcs_len(&dense[j], &dense[i]);
                // Long paragraphs need less overlap to prove a restatement: a
                // 100-char paragraph sharing 18 chars is already telling, while
                // a 30-char table row sharing 18 is not. Measured: this fires at
                // 49% of the reported loop instead of 66%, still with 0 false
                // positives on the 10 real answers.
                let bar = if dense[i].len() >= PARAPHRASE_LONG_PARA_CHARS {
                    PARAPHRASE_LCS_CHARS_LONG
                } else {
                    PARAPHRASE_LCS_CHARS
                };
                if shared < bar {
                    continue;
                }
                // The shared run must leave something DIFFERENT: a run covering
                // (almost) the whole paragraph means the same template refilled
                // with new values, which is a table row or a log line, not a
                // restatement. See PARAPHRASE_MAX_RATIO.
                let shorter = dense[j].len().min(dense[i].len());
                let ratio = shared as f64 / shorter as f64;
                if ratio <= PARAPHRASE_MAX_RATIO {
                    restated = true;
                    break;
                }
            }
            if restated {
                restated_at.push(i);
                // Only restatements close together count: two mentions of the
                // same thing 20 paragraphs apart are not a loop.
                let window: Vec<usize> = restated_at
                    .iter()
                    .copied()
                    .filter(|&k| i.saturating_sub(k) < PARAPHRASE_NEIGHBOUR_PARAS)
                    .collect();
                if window.len() >= PARAPHRASE_RUN_PARAS {
                    // Cut from the EARLIEST restatement in the window, so the
                    // original statement survives and the copies are dropped.
                    let run_start = window[0] - 1;
                    let run = window.len();
                    let cut_at = paras[run_start + 1].0;
                    let real_span = text.chars().count() - text[..cut_at].chars().count();
                    let unit: String = paras[run_start + 1].1.chars().take(UNIT_SNIPPET_CHARS).collect();
                    return Some(LoopHit {
                        unit,
                        repeats: (run + 1) as u32,
                        chars: real_span,
                        real_span,
                    });
                }
            }
        }
        None
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
/// Split `text` into paragraphs, returning each paragraph's byte offset too.
///
/// A paragraph ends at a blank line or a single newline: both separate one
/// attempt at saying something from the next, and a model that restates itself
/// uses whichever its formatting calls for.
fn split_paragraphs(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let para = text[start..i].trim();
            if !para.is_empty() {
                out.push((start, para.to_string()));
            }
            i += 1;
            start = i;
            continue;
        }
        i += 1;
    }
    let para = text[start..].trim();
    if !para.is_empty() {
        out.push((start, para.to_string()));
    }
    out
}

/// Longest common substring length of two char slices.
///
/// Deliberately a substring (contiguous) rather than a subsequence: a reworded
/// sentence keeps whole phrases verbatim, whereas scattered shared characters
/// would match almost any two Chinese sentences on the same topic.
fn lcs_len(a: &[char], b: &[char]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    let mut best = 0usize;
    for i in 1..=a.len() {
        cur[0] = 0;
        for j in 1..=b.len() {
            if a[i - 1] == b[j - 1] {
                cur[j] = prev[j - 1] + 1;
                if cur[j] > best {
                    best = cur[j];
                }
            } else {
                cur[j] = 0;
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    best
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

    /// R2's exact boundary, measured rather than assumed.
    ///
    /// `match_span` compares `W[len-1-j]` against `W[len-1-unit_len-j]`, so N
    /// consecutive copies of a unit of length L produce a span of (N-1)*L and
    /// therefore `repeats = N-1`. With `max_repeats = 4` the threshold is
    /// consequently "FIVE copies fire, FOUR do not".
    ///
    /// This pins the tolerance the default config actually buys, and it is the
    /// only falsifiable boundary in R2: the failure it exists to catch is a
    /// guard tuned one notch too eager, which is precisely the shape that would
    /// cut legitimate text. (Lowering `min_chars` or `repeat_ratio` instead was
    /// measured NOT to change this outcome -- R4's ratio is exactly 0.75 at four
    /// copies, and R2 still has the last word -- so `max_repeats` is the only
    /// knob this test can and should pin.)
    #[test]
    fn t02b_repeat_threshold_boundary_is_five_copies_not_four() {
        // Distinct characters, so the scan cannot collapse onto unit_len = 1.
        let unit = "第一步要先确认接口返回的字段结构";
        assert_eq!(unit.chars().count(), 16, "the arithmetic below depends on L=16");
        assert_eq!(
            unit.chars().collect::<std::collections::HashSet<_>>().len(),
            16,
            "every character must be distinct or the boundary moves"
        );

        // FOUR copies (64 chars) are inside the tolerance: the client must see
        // them, so a guard that fires here is over-eager and this is where it
        // gets caught.
        let four = unit.repeat(4);
        assert_eq!(four.chars().count(), 64);
        assert!(
            hit(&four).is_none(),
            "4 copies of a 16-char unit must NOT be judged a loop (max_repeats=4 counts 4 repeats, i.e. 5 copies); a hit here is the over-eager-guard bug"
        );

        // FIVE copies (80 chars) exceed it and must fire -- otherwise the test
        // above could be satisfied by a guard that simply never fires.
        let five = unit.repeat(5);
        let h = hit(&five).expect("5 copies must be judged a loop");
        assert_eq!(h.repeats, 4, "5 copies == 4 repeats");
        assert_eq!(h.unit.chars().count(), 16);
    }
    /// The ladder must stay cheap enough to run on EVERY frame.
    ///
    /// `push_tail` calls this per delta, so a wide scan cannot be allowed to
    /// cost milliseconds. The worst case is a tiny period (every candidate
    /// multiple matches), which is what the timing below uses. The budget is
    /// deliberately loose (an order of magnitude above the measured value) so
    /// it flags a real regression rather than CI jitter.
    #[test]
    fn t39_ladder_stays_fast_on_every_frame() {
        use std::time::Instant;
        let cfg = cfg();
        for (name, period) in [("period=4 (worst case)", 4usize), ("period=300", 300)] {
            let unit = distinct_chars(period, 1);
            let text = unit.repeat(cfg.window_chars / period + 8);
            let mut g = LoopGuard::new(&cfg);
            g.push_tail(&text); // fill the window once
            let iters = 50;
            let t = Instant::now();
            for _ in 0..iters {
                let _ = g.judge_tail();
            }
            let per_call = t.elapsed().as_secs_f64() * 1e6 / iters as f64;
            println!("{name}: {per_call:.1} us per frame (budget 1000)");
            assert!(per_call < 1_000.0, "{name}: {per_call:.0} us per frame is over the 1 ms budget");
        }
    }

    /// The REAL reported case: one answer restating the same plan six times
    /// in slightly different words. Every R1-R6 rule misses it by construction
    /// (no two paragraphs are identical), which is exactly why this test exists.
    const REPORTED_PARAPHRASE_LOOP: &str = concat!(
        "好，提醒间隔最小设为 5 分钟，默认 10 分钟，保留现有约每 5 分钟检查一次的节奏。接下来按 ts-standards 和 TDD 补配置、提醒计数与回归测试，设置界面沿用现有样式。",
        "\n",
        "明白，提醒间隔最小 5 分钟，默认 10 分钟，保留现有约每 5 分钟的检查节奏。我会在配置输入和任务执行两处都加限制，并补测提醒次数、发送失败及签出后停止的行为。",
        "\n",
        "好，提醒间隔最小设为 5 分钟，默认 10 分钟，保留现有每 5 分钟检查一次的节奏。我会按 ts-standards 接入配置，并用测试覆盖次数、间隔和签出后停止的行为。",
        "\n",
        "好，提醒间隔最小设为 5 分钟，默认仍是 10 分钟，检查频率就保留现有的每 5 分钟一次。我会按 ts-standards 和 TDD 补齐账号配置、设置界面及重复提醒逻辑，并在运行时兜住小于 5 分钟的无效应配置。",
        "\n",
        "好，提醒间隔最小设为 5 分钟，默认 10 分钟，保留现有每 5 分钟检查一次的节奏。我会按 ts-standards 和 TDD 补齐配置与提醒计数，并在设置输入和任务执行两处限制最小间隔。",
        "\n",
        "好的，提醒间隔最小设为 5 分钟，默认仍为 10 分钟，检查节奏保留现有约每 5 分钟一次。接下来按 ts-standards 和 TDD 补配置、设置项及重复提醒逻辑，并验证发送失败不计次、签出后停止。",
        "\n",
    );

    #[test]
    fn t40_paraphrased_restatement_is_caught() {
        let cfg = cfg();
        // Precondition: the verbatim rules MUST miss this text. If this ever
        // starts hitting, the sample changed and the test below proves nothing.
        let mut verbatim = LoopGuard::new(&cfg);
        verbatim.append(REPORTED_PARAPHRASE_LOOP);
        assert!(
            verbatim.detect().is_none(),
            "sample no longer needs the paraphrase rule; pick a new sample"
        );
        // The paraphrase rule must catch it, post-commit (where paragraphs are
        // complete -- the caller feeds the answer in as it streams).
        let mut g = LoopGuard::new(&cfg);
        let h = g
            .push_tail(REPORTED_PARAPHRASE_LOOP)
            .expect("the reported 6-paragraph restatement must be caught");
        assert!(h.real_span > 0, "a hit must say what to cut: {h:?}");
        // The cut must keep the FIRST statement and drop the restatements.
        let kept = g.text_before(h.real_span);
        assert!(
            kept.chars().count() > 40,
            "the first statement must survive the cut, kept {} chars",
            kept.chars().count()
        );
        assert!(
            kept.chars().count() < REPORTED_PARAPHRASE_LOOP.chars().count(),
            "the restatements must be dropped"
        );
    }

    /// The paraphrase rule must fire PRE-commit, not only post-commit.
    ///
    /// Measured on the live gateway: with the rule only in `judge_tail`, a
    /// 6-paragraph restatement was fully relayed before the hit, so the client
    /// kept every copy and the correction merely appended more text. Pre-commit
    /// is where this shape can actually be stopped with zero client bytes.
    #[test]
    fn t43_paraphrase_loop_is_caught_before_commit() {
        let cfg = cfg();
        let mut g = LoopGuard::new(&cfg);
        let hit = g
            .push(REPORTED_PARAPHRASE_LOOP)
            .expect("pre-commit must catch the reported restatement");
        assert!(hit.real_span > 0, "the hit must report what to cut: {hit:?}");
    }

    /// The hit must land EARLY, not merely at the end.
    ///
    /// This is a latency requirement disguised as a correctness one. Judging
    /// only after the whole answer has streamed means the client already has
    /// every copy and the correction merely appends more text -- measured on
    /// the live gateway, that shipped all 6 paragraphs. The rule must fire while
    /// the loop is still forming.
    ///
    /// Pinned at 4 paragraphs: measured 3 for the reported sample. Tightening
    /// this further would force the detector to treat ONE reworded paragraph as
    /// proof, which cuts a normal summary-then-expand answer (`t41`).
    #[test]
    fn t44_paraphrase_hit_lands_before_the_loop_finishes() {
        let cfg = cfg();
        let mut g = LoopGuard::new(&cfg);
        let paras: Vec<&str> = REPORTED_PARAPHRASE_LOOP
            .split('\n')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        assert_eq!(paras.len(), 6, "the sample is six paragraphs");
        let mut fired_after = None;
        for (i, p) in paras.iter().enumerate() {
            if g.push(&format!("{p}\n")).is_some() {
                fired_after = Some(i + 1);
                break;
            }
        }
        let fired_after = fired_after.expect("must fire at all");
        assert!(
            fired_after <= 4,
            "fired only after {fired_after}/6 paragraphs; the client would already have the loop"
        );
    }

    /// Real model answers must NEVER be cut by the paraphrase rule.
    ///
    /// These are 15 genuine answers captured from the live model (933..4287
    /// chars), deliberately including the shapes most likely to false-positive:
    /// markdown tables, code blocks, checklists, and answers that summarise
    /// first and then expand. Fault injection: lowering `PARAPHRASE_LCS_CHARS`
    /// to 12 makes this test fail, which is what keeps the threshold honest.
    const REAL_ANSWERS: &[&str] = &[
        "HTTP 状态码是服务器对客户端请求的响应结果的三位数字代码，由 RFC 9110 等标准定义。它的第一位数字表示响应类别，共分为五类。每一类都对应着不同的语义和处理方式，理解这些类别对于开发、调试和运维 Web 应用都非常重要。下面逐一详细说明。\n\n## 一、1xx：信息性状态码（Informational）\n\n1xx 表示服务器已收到请求，正在继续处理，属于临时性响应。这类状态码不会作为最终响应出现，客户端通常会在收到 1xx 后继续等待最终结果。它们主要用于协议层面的协商和连接管理。\n\n**例子 1：100 Continue**\n客户端在发送较大的请求体之前，可以先发送带有 `Expect: 100-continue` 头的请求。服务器如果愿意接收请求体，就返回 `100 Continue`，客户端随后才真正发送数据。这样可以避免在服务器拒绝请求时白白传输大量数据，是一种优化机制。\n\n**例子 2：101 Switching Protocols**\n当客户端请求升级协议时（例如通过 `Upgrade: websocket` 头请求从 HTTP 升级到 WebSocket），服务器如果同意，就返回 `101 Switching Protocols`，此后连接上的通信将按照新协议进行。这是 WebSocket 握手成功的关键状态码。\n\n需要注意的是，1xx 响应是中间响应，HTTP/1.1 中它们不以空行结束，而是紧接着后续响应；在现代 HTTP/2 和 HTTP/3 中，1xx 的处理方式有所不同，但语义保持一致。\n\n## 二、2xx：成功状态码（Successful）\n\n2xx 表示请求已被服务器成功接收、理解并处理。这是最\"令人愉快\"的一类状态码，具体语义因方法而异：GET 表示资源已获取，POST 表示已处理，等等。\n\n**例子 1：200 OK**\n最通用的成功状态码，表示请求成功，响应体中包含所请求的资源或操作结果。对于 GET 请求，响应体就是资源内容；对于 POST 请求，响应体可能是操作结果。几乎所有正常的网页浏览都会产生大量 200 响应。\n\n**例子 2：201 Created**\n表示请求成功并且导致服务器创建了新资源，通常用于 POST 或 PUT 请求。响应中一般会通过 `Location` 头指出新资源的 URI。例如在 RESTful API 中创建一条新记录后返回 201 并附带新资源的地址，这是符合规范的常见做法。\n\n此外还有 204 No Content（成功但无响应体，常用于 DELETE 或 PUT）、206 Partial Content（范围请求成功，用于断点续传和视频流）等，它们都在特定场景下表达更精确的\"成功\"含义。\n\n## 三、3xx：重定向状态码（Redirection）\n\n3xx 表示客户端需要采取进一步操作才能完成请求，通常意味着资源位于别处，或者需要使用缓存副本。这类状态码分为永久重定向和临时重定向，二者的区别对搜索引擎优化和缓存行为有实际影响。\n\n**例子 1：301 Moved Permanently**\n表示请求的资源已永久移动到新位置，响应中的 `Location` 头给出新 URI。客户端和搜索引擎应当更新书签和索引。常见于网站更换域名或 URL 结构永久调整时。由于浏览器会缓存 301，配置错误可能导致难以撤销的问题，需要谨慎使用。\n\n**例子 2：302 Found**\n表示资源临时位于另一个 URI。与 301 不同，客户端不应更新书签，后续请求仍应使用原 URI。它常用于登录后的临时跳转、A/B 测试分流等场景。历史上 302 的实现曾与规范不符（早期浏览器会自动把 POST 改为 GET），因此后来又引入了 307 和 308 来明确保持方法和请求体的语义。\n\n其他常见成员包括 304 Not Modified（配合缓存验证，表示资源未变化，可继续使用缓存）、307 Temporary Redirect（临时重定向且保持请求方法不变）、308 Permanent Redirect（永久重定向且保持方法不变）。\n\n## 四、4xx：客户端错误状态码（Client Error）\n\n4xx 表示请求本身存在问题，错误的责任在客户端一侧，例如请求语法错误、缺少认证、资源不存在、方法不被允许等。服务器理解了请求但无法或拒绝处理。客户端通常不应原样重试（除非问题被修正）。\n\n**例子 1：404 Not Found**\n表示服务器找不到请求的资源。这是最广为人知的错误码，常见于输入了错误的 URL 或资源已被删除。需要注意的是，出于安全考虑，有些服务器会对本应返回 403 的请求也返回 404，以避免泄露资源是否存在的信息。\n\n**例子 2：401 Unauthorized**\n表示请求缺少有效的身份认证凭据。虽然名字叫\"未授权\"，实际含义是\"未认证\"——客户端需要提供凭证（如通过 `WWW-Authenticate` 头指示的认证方式）。与之相对的是 403 Forbidden，表示身份已确认但无权访问。\n\n其他重要成员包括 400 Bad Request（请求语法错误）、403 Forbidden（拒绝访问）、405 Method Not Allowed（方法不被支持）、409 Conflict（资源状态冲突）、429 Too Many Requests（触发限流）等。合理使用 4xx 能让 API 的错误语义更清晰。\n\n## 五、5xx：服务器错误状态码（Server Error）\n\n5xx 表示服务器在处理一个看似有效的请求时遇到了错误，责任在服务器一侧。这类错误通常意味着服务端存在缺陷、过载或依赖故障。客户端可以稍后重试（尤其是 503），但根本修复需要服务端介入。\n\n**例子 1：500 Internal Server Error**\n最通用的服务器错误，表示服务器遇到意外情况，无法完成请求。它往往源于未捕获的异常、代码缺陷或配置错误。对客户端而言，500 通常不透露具体原因（出于安全考虑），详细错误应记录在服务端日志中。\n\n**例子 2：503 Service Unavailable**\n表示服务器暂时无法处理请求，通常由于过载或维护。响应中可以通过 `Retry-After` 头告知客户端何时可以重试。在负载均衡和限流场景中，503 常被用来表达\"服务暂时不可用，请稍后再试\"，是熔断和降级策略中常见的返回码。\n\n其他成员包括 502 Bad Gateway（网关从上游收到无效响应）、504 Gateway Timeout（网关等待上游超时）等，这些在反向代理和微服务架构中尤其常见，能帮助快速定位故障发生在哪一层。\n\n## 总结\n\n五类状态码通过首位数字形成清晰的语义分层：1xx 是过程性的中间信息，2xx 是成功，3xx 是重定向与缓存协商，4xx 是客户端侧问题，5xx 是服务端侧问题。实际开发中，不仅要正确选择状态码，还要理解同类内部各码之间的细微差别（如 301 与 302、401 与 403、502 与 504），这样才能让接口行为符合标准、便于调试，也利于缓存、代理、搜索引擎等中间环节正确工作。",
        "| 名称 | 地址 | 超时 | 重试 |\n|------|------|------|------|\n| 华东支付网关 | https://api-east.pay.example.com/v2 | 5s | 3 |\n| 华南支付网关 | https://api-south.pay.example.com/v2 | 6s | 2 |\n| 华北消息队列 | amqp://mq-north.example.com:5672 | 10s | 5 |\n| 用户中心主库 | jdbc:mysql://db-user-primary.example.com:3306 | 30s | 1 |\n| 用户中心从库 | jdbc:mysql://db-user-replica.example.com:3307 | 30s | 2 |\n| 风控引擎 | http://risk-engine.example.com:8080/check | 3s | 4 |\n| 短信服务商A | https://sms-a.example.com/send | 8s | 3 |\n| 短信服务商B | https://sms-b.example.com/send | 8s | 2 |\n| 对象存储主节点 | https://oss-primary.example.com | 15s | 2 |\n| 对象存储备节点 | https://oss-backup.example.com | 20s | 3 |\n| 日志采集服务 | udp://log-collector.example.com:5140 | 2s | 1 |\n| 缓存集群 | redis://cache-cluster.example.com:6379 | 1s | 5 |\n| 第三方物流接口 | https://logistics.example.com/api/v1 | 12s | 3 |\n| 发票开具服务 | https://invoice.example.com/open | 25s | 2 |\n| 报表导出服务 | https://report.example.com/export | 60s | 1 |",
        "# SSE 流式输出详解\n\n## 一、什么是 SSE\n\n**SSE（Server-Sent Events，服务器发送事件）** 是一种基于 HTTP 的**单向、服务器到客户端**的推送技术。服务器可以持续向客户端发送数据流，客户端只需建立一个连接就能不断接收消息。\n\n浏览器端通过 `EventSource` API 使用：\n\n```js\nconst es = new EventSource('/stream');\nes.onmessage = (e) => console.log(e.data);\n```\n\n> 注：LLM 应用（如 ChatGPT）里的\"流式输出\"，前端常用 `fetch` + `ReadableStream` 读取，后端按 SSE 的 `data: ...\\n\\n` 格式发送，本质仍是 SSE 协议。\n\n---\n\n## 二、SSE 与普通 HTTP 响应的区别\n\n| 维度 | 普通 HTTP 响应 | SSE 流式响应 |\n|------|---------------|-------------|\n| 响应方式 | 一次性返回完整 body | 分块持续发送，连接保持打开 |\n| 连接生命周期 | 响应结束即关闭 | 长连接，直到主动断开 |\n| 传输头 | `Content-Length` 或分块 | `Content-Type: text/event-stream`、`Transfer-Encoding: chunked` |\n| 数据单位 | 完整 JSON/HTML | `data: ...\\n\\n` 事件块 |\n| 方向 | 请求-响应一次交互 | 服务器→客户端单向持续推送 |\n| 适用场景 | 常规接口 | 打字机效果、日志实时输出、进度通知 |\n| 客户端处理 | `res.json()` 等 | 读取 stream、逐块解析 |\n\n**核心差异：** 普通响应是\"一问一答的完整包裹\"；SSE 是\"一问之后，服务器把答案一小块一小块地递过来\"。\n\n---\n\n## 三、SSE 的消息格式\n\n每行一个字段，事件之间用**空行（`\\n\\n`）**分隔：\n\n```\ndata: 第一段内容\\n\\n\ndata: 第二段内容\\n\\n\n\nevent: done\\n\ndata: [DONE]\\n\\n\n```\n\n常用字段：\n- `data:` 消息内容（可多行，多行会拼接）\n- `event:` 自定义事件名（客户端用 `addEventListener` 监听）\n- `id:` 断线重连时的游标\n- `retry:` 重连间隔毫秒\n- 以 `:` 开头的行是注释（可用作心跳）\n\n---\n\n## 四、实现要点\n\n### 1. 服务端关键 Header\n\n```http\nContent-Type: text/event-stream; charset=utf-8\nCache-Control: no-cache\nConnection: keep-alive\nX-Accel-Buffering: no   # 关键：禁用 Nginx 缓冲，否则不会实时\n```\n\n### 2. 后端伪代码（Node/Express）\n\n```js\napp.get('/stream', (req, res) => {\n  res.setHeader('Content-Type', 'text/event-stream');\n  res.setHeader('Cache-Control', 'no-cache');\n  res.setHeader('Connection', 'keep-alive');\n  res.flushHeaders();          // 立即发送头部，避开默认缓冲\n\n  const timer = setInterval(() => {\n    res.write(`data: ${JSON.stringify({ t: Date.now() })}\\n\\n`);\n  }, 1000);\n\n  req.on('close', () => clearInterval(timer)); // 客户端断开要清理资源\n});\n```\n\n### 3. 必须\"立即 flush\"\n\n- 框架（Express、Flask、Django、Spring）默认会缓冲响应，**第一块数据可能卡住不发出**。\n- 措施：调用 `flushHeaders()` / `res.flush()` / 关闭中间件缓冲，或用 `transfer-encoding: chunked`。\n- 反向代理（Nginx）要 `proxy_buffering off;`，否则流被整段缓存，前端看到的还是\"一次性返回\"。\n\n### 4. 前端容错解析\n\n```js\nconst resp = await fetch('/stream');\nconst reader = resp.body.getReader();\nconst decoder = new TextDecoder();\nlet buf = '';\nwhile (true) {\n  const { done, value } = await reader.read();\n  if (done) break;\n  buf += decoder.decode(value, { stream: true }); // 用 stream 模式避免乱码\n  let idx;\n  while ((idx = buf.indexOf('\\n\\n')) >= 0) {\n    const chunk = buf.slice(0, idx);\n    buf = buf.slice(idx + 2);\n    // 解析 \"data: ...\"\n  }\n}\n```\n\n要点：\n- 用 `TextDecoder(..., { stream: true })` 处理**跨块的多字节字符被截断**问题。\n- 按 `\\n\\n` 切分事件，不要按单次 `read()` 回调切分。\n- 处理 `data:` 可能跨多个 chunk 到达的情况。\n- 遇到 `[DONE]` 或流结束要停止读取并更新 UI。\n\n### 5. 断线重连与心跳\n\n- `EventSource` 默认会**自动重连**（普通 fetch 不会，需自己实现）。\n- 长时间无数据时，中间代理可能判定超时断连；周期性发送 `: ping\\n\\n`（注释行）作为心跳保活。\n- 生产环境注意 `retry` 和 `id` 的使用，配合 `Last-Event-ID` 实现断点续传。\n\n### 6. 生命周期与资源清理\n\n- 监听 `close` / `aborted` 事件，停止定时器、关闭数据库游标、中止上游 LLM 请求。\n- 客户端主动断开（关页面、取消）不要让服务端泄漏连接。\n\n### 7. 与 WebSocket 的选择\n\n- SSE：**单向**（服务器→客户端）、纯 HTTP、自动重连、实现简单，适合日志/LLM 流式输出。\n- WebSocket：**双向**、全双工，适合需要频繁双向交互的场景。\n- 需要客户端中途\"取消/发消息\"时，SSE 方案常用\"另一个 HTTP 请求发起取消\"来弥补。\n\n### 8. 常见坑总结\n\n| 坑 | 解决 |\n|----|------|\n| 前端收不到数据，最后一起收到 | 绕过 Nginx 缓冲、服务端 flush |\n| 中文乱码/截断 | `TextDecoder({stream:true})` + 按 `\\n\\n` 切 |\n| 连接自动断 | 心跳、设置代理超时 |\n| 内存泄漏 | 监听 close，清理定时器/上游请求 |\n| 代理或 CDN 不支持 | 检查 `text/event-stream` 透传 |\n\n---\n\n**一句话总结：** SSE 流式输出就是把普通 HTTP 的\"完整响应\"改造成\"长连接 + 分块文本事件\"，核心在于**服务端及时 flush、禁用中间层缓冲、客户端流式解析并按事件边界切分**，同时处理好断线重连、心跳和资源清理。",
        "# Rust 后端项目目录结构示例\n\n下面以典型的 **Web 后端服务**(基于 `axum` / `actix-web` 等框架) 为例,给出一个清晰、可扩展的目录结构,并逐项说明职责。\n\n---\n\n## 整体目录树\n\n```\nmy-backend/\n├── Cargo.toml\n├── Cargo.lock\n├── .env\n├── .env.example\n├── .gitignore\n├── Dockerfile\n├── docker-compose.yml\n├── README.md\n│\n├── migrations/               # 数据库迁移文件(SQL)\n│   ├── 20240101_init.sql\n│   └── ...\n│\n├── config/                   # 环境相关配置文件\n│   ├── default.toml\n│   ├── development.toml\n│   └── production.toml\n│\n├── tests/                    # 集成测试\n│   ├── api_user.rs\n│   └── common/\n│       └── mod.rs            # 测试工具/初始化\n│\n└── src/\n    ├── main.rs               # 程序入口\n    ├── lib.rs                # 库入口(可选,便于测试复用)\n    ├── config.rs             # 配置加载\n    ├── state.rs              # 全局共享状态(AppState)\n    ├── error.rs              # 统一错误类型 & 错误响应\n    ├── telemetry.rs          # 日志 / tracing / metrics 初始化\n    │\n    ├── routes/               # 路由定义层\n    │   ├── mod.rs\n    │   ├── user.rs\n    │   └── health.rs\n    │\n    ├── handlers/             # 请求处理层(Controller)\n    │   ├── mod.rs\n    │   ├── user.rs\n    │   └── health.rs\n    │\n    ├── services/             # 业务逻辑层\n    │   ├── mod.rs\n    │   └── user.rs\n    │\n    ├── repositories/         # 数据访问层(DAO)\n    │   ├── mod.rs\n    │   └── user.rs\n    │\n    ├── models/               # 数据模型\n    │   ├── mod.rs\n    │   ├── entity/           # 数据库实体\n    │   │   └── user.rs\n    │   ├── dto/              # 请求/响应 DTO\n    │   │   └── user.rs\n    │   └── error.rs          # 模型层错误(可选)\n    │\n    ├── middleware/           # 中间件\n    │   ├── mod.rs\n    │   ├── auth.rs\n    │   └── request_id.rs\n    │\n    ├── db/                   # 数据库连接 & 连接池\n    │   ├── mod.rs\n    │   └── pool.rs\n    │\n    └── utils/                # 通用工具\n        ├── mod.rs\n        ├── crypto.rs\n        └── pagination.rs\n```\n\n---\n\n## 各模块职责说明\n\n### 顶层文件\n\n| 文件/目录 | 职责 |\n|---|---|\n| `Cargo.toml` / `Cargo.lock` | 依赖声明与锁定 |\n| `.env` / `.env.example` | 本地敏感配置(密钥、DB URL),不提交真实 `.env` |\n| `Dockerfile` / `docker-compose.yml` | 容器化构建与本地依赖编排(DB、Redis 等) |\n| `README.md` | 项目说明 |\n\n### `migrations/`\n数据库 schema 迁移脚本(常用 `sqlx migrate` 或 `diesel migration`)。**只增不改**,保证历史可追溯。\n\n### `config/`\n不同环境的配置文件(TOML/YAML),与 `.env` 互补:`.env` 放密钥,`config/` 放结构化参数(超时、分页大小、连接池大小等)。\n\n### `tests/`\nRust 的集成测试目录。所有 `.rs` 文件被当作独立 crate 编译,用来做 API 端到端测试。`common/` 放测试公共工具(启动应用、准备 DB)。\n\n---\n\n### `src/` 内部\n\n#### `main.rs`\n程序入口。职责:\n- 加载配置、初始化 `tracing`\n- 建立数据库连接池\n- 构建 `AppState`\n- 组装路由 + 中间件\n- 启动 HTTP 服务器\n\n#### `lib.rs`(可选)\n把核心逻辑作为一个库暴露,便于 `tests/` 复用、也便于以后拆成多 crate(workspace)。\n\n#### `config.rs`\n定义 `Config` 结构体,从 `.env` + `config/*.toml` 读取并校验,提供全局强类型配置。\n\n#### `state.rs`\n定义 `AppState`,集中持有所需的共享依赖(数据库池、Redis 客户端、配置等),通过 `axum::State` 注入到 handler。**避免使用全局单例**,依赖显式传递。\n\n#### `error.rs`\n定义统一的 `AppError` 枚举,实现:\n- `IntoResponse`(axum 需要)\n- `From<sqlx::Error>`、`From<ValidationError>` 等转换\n保证所有 handler 用同一个错误出口,统一返回 JSON 错误格式。\n\n#### `telemetry.rs`\n初始化日志与可观测性:`tracing-subscriber`、`tracing-opentelemetry`、请求 ID 注入、metrics 导出。\n\n#### `routes/`\n**路由层**:只声明 URI、HTTP 方法、绑定 handler 与中间件。不写业务逻辑。\n```\n/health        -> health_handler\n/api/v1/users  -> user_handler::*\n```\n\n#### `handlers/`\n**控制层(Controller)**:接收请求,解析 `Path/Query/Json` → 调用 `services` → 构造响应。**不直接访问数据库**。主要负责:入参反序列化、鉴权上下文提取、调用 service、组装 DTO。\n\n#### `services/`\n**业务逻辑层**:事务边界、领域规则、跨 repository 编排。例如\"注册用户\"= 校验邮箱唯一 + hash 密码 + 写库 + 发事件。**不关心 HTTP**。\n\n#### `repositories/`\n**数据访问层(DAO)**:封装 SQL 或 ORM 调用,把 `sqlx::Error` 转成领域错误。service 通过 trait 依赖它,便于 mock 测试。\n\n#### `models/`\n- `entity/`:与数据库表一一对应(或 ORM 生成)\n- `dto/`:API 的请求体 / 响应体,字段名与前端约定\n- `error.rs`:领域错误类型(可选,或合并进 `src/error.rs`)\n\n**关键原则**:entity 与 dto **分离**,避免 DB 字段泄露到 API。\n\n#### `middleware/`\n自定义中间件:`auth`(JWT 校验并注入用户到请求扩展)、`request_id`、限流、CORS 配置等。\n\n#### `db/`\n连接池创建与调优(`sqlx::PgPool::connect_with`、max_connections、timeout),以及事务辅助函数。\n\n#### `utils/`\n无状态纯函数工具:crypto、分页(query 参数 → offset/limit + 总数封装)、时间格式化等。\n\n---\n\n## 设计要点总结\n\n1. **分层清晰**:`routes → handlers → services → repositories`,单向依赖,上层不越界调用 DB。\n2. **依赖注入**:通过 `AppState` 显式传递依赖,而非全局变量。\n3. **错误统一**:一个 `AppError`,一个响应格式。\n4. **entity / dto 分离**:数据库结构与 API 契约解耦。\n5. **可测试**:service 依赖 repository trait;`tests/` 目录做集成测试。\n6. **可选演进**:当项目变大时,把 `src/` 拆成 workspace(`crates/domain`、`crates/api`、`crates/infra`)。\n\n按需裁剪即可:小项目可以省掉 `services/` 直接 handler 调 repository;大型项目则可以升级成多 crate workspace。",
        "排查间歇性失败的网络请求，核心是**先定位失败发生在哪一层（DNS/TCP/TLS/HTTP/应用）**，再用证据收敛范围。以下是完整步骤。\n\n## 一、先量化问题：定义\"失败\"\n\n1. **明确失败特征**\n   - 错误类型：超时（connect/read）、连接重置（RST）、DNS 解析失败、HTTP 5xx/4xx、TLS 握手失败、响应内容错误。\n   - 频率与时间分布：每天几次？集中在某时段？与流量峰值相关？\n   - 影响范围：只影响特定客户端/地域/运营商？还是服务端全局？\n\n2. **建立可重复的测量方法**\n   - 写脚本持续探测（如每 10s 一次），记录时间戳、错误码、耗时分解（DNS、connect、TTFB、总时长）。\n   - 保留失败样本的完整日志，避免\"偶发\"变成无据可查。\n\n## 二、分层定位：沿请求路径逐跳验证\n\n按 客户端 → DNS → 网络 → 负载均衡 → 服务端 → 下游依赖 的顺序排查。\n\n### 1. DNS 层\n- 检查是否间歇性解析失败或解析到不同 IP（多 A 记录轮询、GSLB）。\n- 方法：`dig @resolver domain` 多次执行，对比结果；客户端用 `getent hosts` 或抓包看 DNS 响应。\n- 关注：TTL 过短导致频繁解析、本地 DNS 缓存污染、解析超时。\n\n### 2. TCP 层\n- 现象：connect timeout、连接被拒绝、RST。\n- 方法：\n  - `curl -v --connect-timeout 5` 观察连接建立。\n  - 抓包（见第三步）看 SYN 是否重传、是否有 RST、三次握手耗时。\n- 关注：SYN 队列溢出（服务端 `netstat -s | grep -i listen`）、连接数打满、NAT/防火墙状态表超时、客户端源端口耗尽。\n\n### 3. TLS 层\n- 现象：握手超时、证书错误、协议版本/加密套件不兼容。\n- 方法：`openssl s_client -connect host:443 -servername host`，重复执行；抓包看握手是否卡在某一步（如 ClientHello 后无响应）。\n- 关注：证书链偶尔拉取失败（OCSP/CRL）、中间设备干扰、会话票据问题。\n\n### 4. HTTP/应用层\n- 现象：5xx、响应慢、超时、空响应。\n- 方法：\n  - 服务端日志按请求 ID 关联，看失败请求是否到达服务端（没到 → 网络/网关问题；到了 → 服务端问题）。\n  - 检查超时配置：上游超时、连接池等待超时、keep-alive 空闲超时导致复用失效（经典\"间歇性失败\"来源）。\n  - 检查负载均衡健康检查是否误摘节点、是否有节点偶发 5xx。\n\n### 5. 下游依赖\n- 数据库慢查询/连接池耗尽、缓存雪崩、第三方 API 限流（429）等都可能表现为上游请求间歇失败。\n- 用链路追踪（trace）确认失败请求卡在哪一跳。\n\n## 三、抓包与日志取证\n\n1. **客户端/边缘抓包**\n   - `tcpdump -i any -w cap.pcap 'host x.x.x.x and port 443'`，失败时段的包重点看：重传、RST、零窗口、握手耗时尖峰。\n   - 用 Wireshark 的 `tcp.analysis.flags`、`http.response.code >= 500` 过滤。\n2. **服务端抓包/日志**\n   - 对比客户端发出时间与服务端收到时间，判断延迟发生在网络还是处理。\n   - 打开 access log 记录上游耗时、连接复用状态、请求 ID。\n3. **关联分析**\n   - 把失败时间点与系统指标（CPU、连接数、GC、带宽、限流计数）对齐，找相关性。\n\n## 四、复现与假设验证\n\n- 构造压力场景：并发连接数、长连接空闲后复用、特定地域/运营商路径。\n- 逐项排除：改 DNS、绕负载均衡直连后端、关闭 keep-alive、调大超时/连接池，看失败率是否变化。\n- 每次只改一个变量，记录前后失败率。\n\n## 五、常见根因清单（对照检查）\n\n| 现象 | 常见原因 |\n|---|---|\n| 间歇 connect timeout | SYN 队列满、防火墙状态表满、NAT 超时 |\n| 间歇 RST | 空闲连接被中间设备回收、服务端 keep-alive 超时短于客户端 |\n| 间歇 5xx | 某后端节点故障、健康检查滞后、限流 |\n| 间歇 DNS 失败 | 解析超时、TTL 过短、本地缓存 |\n| 高峰失败 | 连接池/线程池耗尽、带宽打满 |\n| 特定运营商失败 | 路由问题、MTU/分片、中间设备干扰 |\n\n## 六、修复与验证\n\n- 针对根因修复（如调大 keep-alive 一致配置、增加连接池、修复健康检查、加 DNS 缓存）。\n- 上线后持续用第一步的探测脚本监控失败率，设定告警阈值，确认收敛。\n\n**关键原则**：间歇性问题必须靠**持续探测 + 失败样本留存 + 分层对比**来定位，避免靠猜测。优先确认\"请求是否到达服务端\"，这一步能直接把问题空间砍成两半。",
        "一句话总结：限制提醒间隔的最小值，是为了防止通知刷屏、保护用户体验与系统稳定性，同时避免误配置带来的资源浪费。\n\n第一，从用户体验看，提醒间隔过小会导致高频打扰。用户设置提醒的初衷是“在合适的时间被提示”，而不是被连续轰炸。若最小间隔不受限制，用户可能误设成几秒一次，触发大量通知，造成注意力打断、反感甚至卸载应用，反而背离提醒的价值。\n\n第二，从系统稳定性与资源消耗看，过小的间隔会放大后端压力。每次提醒都涉及定时调度、消息推送、日志写入等开销，间隔越短，单位时间内请求与计算量越大，可能引发队列堆积、推送通道限流、数据库写入飙升，严重时影响其他用户。设置下限相当于给系统留出缓冲。\n\n第三，从业务与配置合理性看，最小值是一种防护性约束。它能拦住误操作、脚本化滥用和异常配置，避免“提醒”退化成骚扰或刷量工具；同时也为产品定义清晰的语义边界，让用户理解提醒是低频、有意义的触达，而非无成本的高频广播。",
        "好的，先复述您的要求：\n\n- 提醒间隔有最小值限制：**最小 5 分钟**（不能设置得比 5 分钟更短）；\n- 提醒间隔有默认值：**默认 10 分钟**（用户没有指定时使用 10 分钟）。\n\n下面说明我会怎么实现：\n\n1. **定义常量**\n   - `MIN_INTERVAL_MINUTES = 5`\n   - `DEFAULT_INTERVAL_MINUTES = 10`\n\n2. **解析与校验输入**\n   - 如果用户没有提供间隔值（为空/未设置），取默认值 10 分钟。\n   - 如果提供了值，先做类型与数值校验（必须是数字、为正数）。\n   - 再与最小值比较：若小于 5 分钟，则拒绝或回退——具体有两种策略，需要和您确认：\n     - **策略 A（报错）**：直接提示\"间隔不能小于 5 分钟\"，要求重新输入；\n     - **策略 B（纠正）**：自动提升到 5 分钟，并给出提示。\n   - 我默认倾向 **策略 A**（显式报错更符合\"最小限制\"的语义），如需纠正式可切换。\n\n3. **统一出口**\n   - 所有设置提醒的入口都走同一个校验函数，保证默认值与最小值规则只在一处生效，避免各调用点行为不一致。\n\n4. **测试覆盖**\n   - 未设置 → 10；\n   - 设置为 3 → 报错（或纠正为 5，取决于策略）；\n   - 设置为 5 → 通过；\n   - 设置为 10 → 通过；\n   - 边界值与非数字输入。\n\n需要您确认一点：低于 5 分钟时，您希望**报错拒绝**还是**自动纠正为 5 分钟**？确认后我按对应策略实现。",
        "从数学运算的角度看，幂等性指的是对某个对象反复施加同一种运算，结果与只施加一次相同。最直观的例子是绝对值函数：对任意实数先取一次绝对值得到非负数，再取绝对值仍然是同一个数，即 ||x|| = |x|。类似地，集合运算中的并集也满足幂等性：A ∪ A = A。这个角度的核心是“f(f(x)) = f(x)”，强调运算的重复不产生额外变化。\n\n从编程与函数设计的角度看，幂等性描述的是一个操作被执行一次或多次，对系统状态产生的影响相同。例如 HTTP 的 PUT 请求通常被设计为幂等的：把某个资源设置为指定内容，执行一次和执行十次，最终资源状态一致。与之相对，POST 创建订单往往不是幂等的，重复提交可能生成多笔订单。这个角度关注的是“副作用是否可重复而无害”，是接口设计和重试机制的基础。\n\n从分布式系统与工程实践的角度看，幂等性是保证消息重复投递、网络超时重试等场景下数据正确性的关键属性。系统通常通过唯一请求 ID、去重表、乐观锁或状态机等手段实现幂等，使得同一条消息即使被消费多次，也只产生一次业务效果。这个角度强调的是“在不可靠环境中实现可靠结果”，幂等性成为容错与一致性的重要保障。",
        "# 新人上手说明\n\n## 一、背景\n\n欢迎加入团队。本说明面向刚入职的新同学，帮助你快速了解工作环境、熟悉基本流程，并在最短时间内独立开展日常工作。\n\n我们所在的团队主要负责[填写团队职责，例如：产品研发/客户支持/内容运营]相关工作，日常协作以[填写协作方式，例如：项目制/需求驱动]为主，沟通渠道包括[填写工具，例如：企业微信、飞书、邮件]。团队现有成员分工如下：项目负责人负责整体规划与资源协调，资深同事负责技术/业务指导，你作为新人将先从辅助性任务入手，逐步过渡到独立承担模块。\n\n在正式上手之前，你需要知道三件事：第一，团队重视流程规范，所有任务都要有记录、有跟踪；第二，遇到问题先自己查资料，查不到再问人，提问时要说清楚背景和你已尝试的方法；第三，新人期一般为[填写周期，例如：1个月]，期间会有导师带教和定期反馈。\n\n本说明分为三部分：先讲背景（即本节），再讲步骤（第二部分），最后讲注意事项（第三部分），请完整阅读后再开始操作。\n\n## 二、步骤\n\n**第1步：完成入职基础配置（第1天）**\n- 领取办公设备，开通企业邮箱、即时通讯工具、项目管理平台账号。\n- 在人事系统完成个人信息登记，确认考勤方式和请假流程。\n- 加入团队群组，修改群昵称为「姓名-岗位」。\n\n**第2步：熟悉环境与资料（第1-2天）**\n- 阅读团队共享文档库中的《团队介绍》《项目清单》《常见问题FAQ》。\n- 向导师索取近期的项目周报和会议纪要，了解当前工作重点。\n- 走一遍办公区，认识对接的行政、IT、财务联系人。\n\n**第3步：明确岗位职责与考核要求（第2-3天）**\n- 与直属上级进行一次15-30分钟的面谈，确认试用期目标、汇报节奏和考核标准。\n- 把面谈要点整理成文档，发给上级确认，避免理解偏差。\n\n**第4步：跟随导师完成首个实操任务（第1周内）**\n- 由导师分配一个低风险的小任务（如整理数据、撰写初稿、跟进一个简单需求）。\n- 按照「接收任务→确认需求→执行→自检→提交→复盘」的流程走一遍，导师全程指导。\n- 任务完成后请导师给出书面或口头反馈，记录改进点。\n\n**第5步：独立承接任务并纳入日常节奏（第2周起）**\n- 在项目管理平台认领任务，按团队约定的格式更新进度。\n- 参加每日/每周例会，学会用「进展-问题-计划」三段式汇报。\n- 每周五向导师提交一份简短周报，内容包括完成事项、遇到的问题、下周计划。\n\n**第6步：阶段性复盘（新人期末）**\n- 对照第3步确认的目标逐项自查，形成总结。\n- 与上级进行一次正式复盘面谈，明确转正或后续培养方向。\n\n## 三、注意事项\n\n1. **权限与保密**：只申请岗位必需的权限，离职或调岗时及时交还；客户资料、内部数据不得外传，发送外部邮件前确认收件人和附件。\n2. **提问方式**：优先查阅文档和搜索历史记录；提问时提供背景、已尝试的方法和期望结果，避免只问「这个怎么做」。\n3. **任务确认**：接到任务先复述一遍需求和截止时间，得到确认再动手，防止方向性返工。\n4. **记录留痕**：重要沟通尽量用文字或邮件确认，会议要有纪要，任务变更要同步到平台，方便追溯。\n5. **时间管理**：新人期任务杂，建议每天上班前花10分钟列当日清单，下班前检查完成情况；遇到多任务冲突及时向上级求助排序，不要自行硬扛。\n6. **反馈与求助**：导师和上级的反馈要记录并落实；遇到卡点超过半天没有进展，要主动求助，不要等到截止日期才暴露问题。\n7. **合规与安全**：遵守公司考勤、报销、设备使用等制度；不安装未经批准的软件，不点击可疑链接，发现异常及时报告IT。\n8. **心态调整**：新人期出错是正常的，关键是及时上报、复盘原因、避免重复；不要因为怕犯错而隐瞒问题。\n\n以上三部分请完整阅读并保存，后续如有流程更新，以团队最新通知为准。祝顺利上手。",
        "结论：流式响应必须做首字节延迟控制，因为流式接口的“响应已开始”是不可回退的状态，首字节一旦发出，状态码、头部和错误处理空间就被锁死；延迟控制是在发出首字节前保留决策、缓冲和失败切换的窗口。\n\n理由：\n1. 不可回退性：HTTP 响应头/状态码只能发一次。首字节发出后若上游失败、超时或返回错误，无法再改成 502/503 或 JSON 错误体，只能中途断流，客户端体验和可观测性都差。\n2. 错误与降级窗口：延迟首字节可等待上游首个有效数据块、鉴权/配额/路由检查完成，失败时走非流式错误响应或重试/切换后端。\n3. 连接与资源保护：避免“假流式”——连接已建立但长时间无数据，客户端/网关超时、连接堆积；同时防止上游慢启动时过早占用流式通道。\n4. 缓存与压缩：部分网关/CDN 需要一定缓冲才能压缩、合并头部或判断内容类型；过早首字节会绕过或破坏这些机制。\n5. 用户体验与可观测性：控制首字节延迟可在“快速失败”和“尽快出字”间取平衡；过早起流会把上游抖动直接暴露给用户，且难以区分“慢”与“挂”。\n6. 背压与顺序：首字节前可完成会话初始化、排序/去重、权限校验，确保流内容一致；否则中途发现无权限只能断流。\n\n因此首字节延迟控制本质是用少量延迟换错误处理、降级和协议正确性的空间。",
    ];

    /// A shared phrase across UNRELATED sections is not a restatement.
    ///
    /// Real answers repeat technical tokens (`Content-Type: text/event-stream`,
    /// a URL, an import line) in far-apart sections. Those pairs do share a long
    /// run, so without the neighbour limit two such mentions in a row would read
    /// as the same paragraph re-said. This pins `PARAPHRASE_NEIGHBOUR_PARAS`:
    /// removing the limit makes this test fail.
    ///
    /// The two later mentions are CONSECUTIVE (that satisfies the run rule) and
    /// sit more than the neighbour limit away from the original.
    #[test]
    fn t42_far_apart_shared_phrase_is_not_a_paraphrase_loop() {
        let cfg = cfg();
        let mut lines: Vec<String> = vec![
            "传输头里最常见的写法是 `Content-Type: text/event-stream`，它告诉浏览器这是一个事件流。"
                .to_string(),
        ];
        for i in 0..7 {
            lines.push(format!(
                "第 {} 条注意事项：这里说明一个与流式传输无关的实现细节，内容各不相同。",
                i + 1
            ));
        }
        lines.push(
            "另外要记得，服务端必须返回 `Content-Type: text/event-stream`，否则浏览器不会当作流处理。"
                .to_string(),
        );
        lines.push(
            "还有一点，反向代理也要透传 `Content-Type: text/event-stream`，不能改写成普通 JSON。"
                .to_string(),
        );
        let text = lines.join("\n");
        let mut g = LoopGuard::new(&cfg);
        assert!(
            g.push_tail(&text).is_none(),
            "far-apart mentions of one MIME type are not a restatement loop"
        );
    }

    #[test]
    fn t41_real_answers_are_never_cut_by_the_paraphrase_rule() {
        let cfg = cfg();
        for (i, text) in REAL_ANSWERS.iter().enumerate() {
            let mut g = LoopGuard::new(&cfg);
            if let Some(h) = g.push_tail(text) {
                panic!(
                    "real answer #{i} ({} chars) was cut as a paraphrase loop: unit {:?} repeats {}",
                    text.chars().count(),
                    h.unit.chars().take(40).collect::<String>(),
                    h.repeats
                );
            }
        }
    }

    #[test]
    fn t03_pure_newline_repeat_does_not_hit()
 {
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

        // ABOVE the tier-2 bound. This used to be the end of the story: a unit
        // longer than `bound2` was undetectable post-commit and leaked in full,
        // however many times it repeated. `LADDER` extends post-commit coverage
        // to ~314 chars at the defaults, so the assertion is now the opposite:
        // the unit must be CAUGHT, and by a tier wider than tier 2.
        for u in [bound2 + 1, bound2 + 8, 100, 128] {
            let long = distinct_chars(u, 1);
            let mut g2 = LoopGuard::new(&cfg);
            let h = g2
                .push_tail(&long.repeat(40))
                .unwrap_or_else(|| panic!("unit {u} must be caught post-commit by the ladder"));
            assert!(
                h.repeats >= cfg.max_repeats,
                "a hit must clear the base evidence bar: {h:?}"
            );
        }

        // The same long unit is caught pre-commit too, where the whole window is
        // judged. Both paths must agree that a long unit is a loop.
        let mut g3 = LoopGuard::new(&cfg);
        assert!(g3.push(&distinct_chars(bound2 + 8, 1).repeat(40)).is_some()) ;
    }

    /// The ladder's unit-length ceilings, tier by tier, pinned as numbers.
    ///
    /// `t32` proves the wide tiers can SEE a long unit. This proves each tier's
    /// reach is where `LADDER`'s arithmetic says it is, so a future edit that
    /// silently narrows coverage turns this red instead of quietly regressing.
    #[test]
    fn t35_ladder_unit_length_ceilings_are_pinned() {
        let cfg = cfg();
        let tail = cfg.tail_chars as usize;
        let reps0 = cfg.max_repeats as usize;
        let ceilings: Vec<usize> = LADDER
            .iter()
            .map(|(m, e)| (tail * m) / (reps0 + *e as usize + 1))
            .collect();
        assert_eq!(ceilings, vec![25, 54, 128, 314], "LADDER coverage moved");

        // Every ceiling must be reachable: a unit exactly at it is caught.
        for (i, ceil) in ceilings.iter().enumerate() {
            if *ceil > 300 {
                continue; // needs more copies than a realistic stream would send
            }
            let unit = distinct_chars(*ceil, 1);
            let mut g = LoopGuard::new(&cfg);
            assert!(
                g.push_tail(&unit.repeat(40)).is_some(),
                "tier {i} ceiling {ceil} must be catchable; otherwise the ceiling is a lie"
            );
        }
    }

    /// EVERY unit length the ladder claims to cover must actually be covered.
    ///
    /// `t35` only checks the ceilings. The ladder also SKIPS the bands each
    /// narrower tier already scanned, and a bug in that skip silently drops a
    /// slice of unit lengths while every ceiling still looks right -- which is
    /// exactly what fault injection found (skipping 9 instead of 0 extra left
    /// the whole suite green). This sweeps every length, so a hole cannot hide
    /// between two pinned numbers.
    #[test]
    fn t37_every_covered_unit_length_is_caught() {
        let cfg = cfg();
        let mut missed = Vec::new();
        for u in 24usize..=130 {
            let unit = distinct_chars(u, 1);
            let mut g = LoopGuard::new(&cfg);
            if g.push_tail(&unit.repeat(40)).is_none() {
                missed.push(u);
            }
        }
        assert!(missed.is_empty(), "the ladder claims 24..=130 is covered, but {missed:?} leaked");
    }

    /// The evidence ratchet: a wider span must not lower the repeat bar.
    ///
    /// R4 ("the repetition must dominate the judged span") is trivially
    /// satisfied when the whole window IS the repetition, so on a short window
    /// R2 alone decides -- and the ladder's higher tiers are the only thing
    /// demanding more copies. Fault injection confirms the point: lowering the
    /// wide tiers' bar to `max_repeats` cuts this text, which is why it is
    /// pinned. The count below is the widest tier's requirement minus one
    /// copy, so it sits just under the ratified bar on purpose.
    #[test]
    fn t38_wide_tiers_keep_their_higher_repeat_bar() {
        let cfg = cfg();
        let unit = distinct_chars(120, 1);
        // Tier 3 demands `max_repeats + 4` repeats (9 copies). One copy short
        // must NOT be cut: R4 holds (the window is the repetition), so only the
        // raised bar saves it.
        let mut g = LoopGuard::new(&cfg);
        assert!(
            g.push_tail(&unit.repeat(8)).is_none(),
            "8 copies of a 120-char unit is one short of tier 3's bar and must survive"
        );
        // At the bar it must fire, so the test is a threshold and not a wall.
        let mut g2 = LoopGuard::new(&cfg);
        assert!(
            g2.push_tail(&unit.repeat(9)).is_some(),
            "9 copies meets tier 3's bar and must be cut"
        );
    }

    /// The widest tier must not cut text that is NOT a repetition.
    #[test]
    fn t36_wide_tiers_do_not_cut_distinct_long_text() {
        let cfg = cfg();
        // Long-form prose where no unit repeats. The wide tiers scan up to 314
        // characters of unit length, which is exactly where a real document
        // could accidentally look periodic, so this case is the one that
        // matters for the widened net.
        let prose: String = (0..300)
            .map(|i| format!("第 {i} 段落说明编号 {i} 的渠道在 {} 毫秒内完成了第 {i} 次重试。", i * 13))
            .collect::<Vec<_>>()
            .join("\n");
        let mut g = LoopGuard::new(&cfg);
        assert!(g.push_tail(&prose).is_none(), "distinct prose must survive the wide tiers");

        // A long unit repeated only twice is NOT a loop at any tier: R2's bar
        // rises with the span precisely so this stays true.
        let unit = distinct_chars(120, 1);
        let mut g2 = LoopGuard::new(&cfg);
        assert!(g2.push_tail(&unit.repeat(2)).is_none(), "2 copies is not a loop");
        let unit3 = distinct_chars(120, 1);
        let mut g3 = LoopGuard::new(&cfg);
        assert!(g3.push_tail(&unit3.repeat(3)).is_none(), "3 copies must not be cut");
    }
}
