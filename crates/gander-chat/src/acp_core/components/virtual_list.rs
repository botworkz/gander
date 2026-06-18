// SPDX-License-Identifier: GPL-3.0-or-later

//! Pure arithmetic for the message-list virtualiser.
//!
//! Extracted into its own module so the windowing logic is testable on
//! the native target without pulling in `web-sys` or the DOM.  The
//! DOM-side wrappers (scroll listener, intersection observer, height
//! cache writers) live in `crate::acp_core::components::message_list`.
//!
//! # Model
//!
//! We model the list as a vertical stack of messages indexed `0..n`.
//! For each index `i` we know an *estimated* height in CSS pixels
//! (real height if it's been measured, fallback average otherwise).
//! Given the current `scroll_top` and `client_height` we compute:
//!
//! - `first` — index of the first message that should be mounted
//! - `last`  — one past the index of the last message that should be
//!             mounted (range is `first..last`)
//! - `leading_spacer` — cumulative height of indices `0..first`
//! - `trailing_spacer` — cumulative height of indices `last..n`
//!
//! `<MessageList>` then renders
//!
//! ```text
//!   <div style="height: leading_spacer"/>
//!   <For each=visible_slice .../>
//!   <div style="height: trailing_spacer"/>
//! ```
//!
//! and the keyed-`<For>` over the *visible slice* mounts at most
//! `(visible_count + 2 * VIRTUAL_OVERSCAN)` messages regardless of
//! transcript length.
//!
//! # Overscan
//!
//! `VIRTUAL_OVERSCAN` is the number of messages mounted above and
//! below the visible range.  This is the "Netflix carousel" trick
//! every real virtualiser uses: it means moderate scroll-back doesn't
//! trigger remount, doesn't flash skeleton placeholders, and doesn't
//! lose component-local state for messages just-out-of-frame.
//!
//! Tracked deliberately as **items, not pixels** — see #124 for the
//! rationale.  Failure mode of items-based overscan is "one really
//! tall card sometimes unmounts a bit eagerly", which doesn't break
//! anything.
//!
//! # Why we always include the tail
//!
//! For a live chat session the bottom of the list is always visible
//! (anchor-to-now, gander#119), so the last `VIRTUAL_OVERSCAN`
//! messages are always inside the window.  This guarantees the
//! streaming bubble + the recent tool cards + their iframes are never
//! unmounted by virtualisation, which keeps live behaviour unchanged
//! and side-steps the iframe-state-loss problem for the
//! conversational hot path.

/// Number of messages mounted above and below the visible range as an
/// overscan buffer.  See module docs.
///
/// At a typical viewport with ~120 px message heights that's roughly
/// 2–3 viewports of buffer on each side — wide enough that routine
/// "scroll up a screen or two to re-read" reads zero unmounts.
///
/// If QA shows this is wrong, just change this number.  Do **not**
/// make it user-configurable: it's an implementation knob, not a
/// preference, and committing to a config commits us to a contract
/// almost nobody should ever touch.
pub const VIRTUAL_OVERSCAN: usize = 20;

/// Pixel height to assume for a message before it has been measured.
///
/// Tuned by eye against the long-session mock fixture; a typical
/// assistant bubble lands around 120 px once markdown renders, with
/// huge variance (one-word "ok" → 30 px, a wrapped code block →
/// 800+ px).  The running mean updates as messages settle; this is
/// only the initial guess.
pub const INITIAL_AVERAGE_HEIGHT_PX: f64 = 120.0;

/// Computed mount window plus spacer sizes.
///
/// Bundled into a single struct so `<MessageList>` can write the
/// result to one signal and downstream effects observe one read
/// rather than four.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisibleWindow {
    /// Index of the first message that should be mounted (inclusive).
    pub first: usize,
    /// Index one past the last message that should be mounted
    /// (exclusive).  `first..last` is the slice the keyed `<For>`
    /// iterates over.
    pub last: usize,
    /// Cumulative pixel height of indices `0..first` — paints as the
    /// leading spacer `<div>`.
    pub leading_spacer: f64,
    /// Cumulative pixel height of indices `last..total` — paints as
    /// the trailing spacer `<div>`.  Always `>= 0`.
    pub trailing_spacer: f64,
}

impl VisibleWindow {
    /// Default for an empty list: no messages mounted, no spacers.
    pub const EMPTY: Self = Self {
        first: 0,
        last: 0,
        leading_spacer: 0.0,
        trailing_spacer: 0.0,
    };
}

/// Compute the visible window for a list of messages.
///
/// Pure function: takes scroll metrics, total count, and a closure
/// returning the per-message estimated height.  Returns the mount
/// range and spacer sizes (see [`VisibleWindow`]).
///
/// `height_of(i)` is called O(`total`) times in the worst case
/// (computing the leading spacer walks indices `0..first`).  Callers
/// should resolve heights from a fast in-memory map; allocating on
/// each call is a footgun.
///
/// `overscan` is the symmetric mount buffer.  In production this is
/// always [`VIRTUAL_OVERSCAN`]; the parameter exists only so unit
/// tests can drive corner cases without globally tweaking the
/// constant.
///
/// # Semantics
///
/// - Empty list (`total == 0`) → [`VisibleWindow::EMPTY`].
/// - Visible region of the list is the half-open pixel range
///   `[scroll_top, scroll_top + client_height)`.  The "raw" visible
///   index range is the smallest `[vf, vl)` whose cumulative heights
///   cover that pixel range.
/// - The mount range expands the raw range by `overscan` on each
///   side, then clamps to `0..total`.
/// - Spacers are the cumulative heights of the unmounted prefix and
///   suffix.  Spacer heights use the *same* `height_of` function as
///   the mount math, so the total list height the user scrolls
///   through is internally consistent regardless of which messages
///   are currently mounted.
#[inline]
pub fn visible_window<H>(
    scroll_top: f64,
    client_height: f64,
    total: usize,
    overscan: usize,
    height_of: H,
) -> VisibleWindow
where
    H: Fn(usize) -> f64,
{
    if total == 0 {
        return VisibleWindow::EMPTY;
    }

    // Clamp degenerate inputs.  A negative scrollTop / clientHeight
    // shouldn't happen but a real browser has surprised us before.
    let scroll_top = scroll_top.max(0.0);
    let client_height = client_height.max(0.0);

    // ── Raw visible window: walk indices summing heights ──────────
    //
    // We do this in two scans rather than one because (a) it keeps
    // each loop trivially correct and (b) the spacer math wants
    // cumulative-up-to-first anyway, which we get for free.
    let mut cumulative = 0.0_f64;
    let mut vf = 0usize;
    for i in 0..total {
        let h = height_of(i).max(0.0);
        if cumulative + h > scroll_top {
            vf = i;
            break;
        }
        cumulative += h;
        // Past the end means everything is above the visible region
        // (user has scrolled past the bottom — shouldn't happen with
        // an `overflow:auto` container but be defensive).
        if i + 1 == total {
            vf = total.saturating_sub(1);
        }
    }

    // `cumulative` is now the height of indices `0..vf`; that's
    // exactly the leading spacer for the raw window.  We still need
    // to walk forward to find `vl`.
    let visible_top = cumulative;
    let mut vl = vf;
    let mut running = visible_top;
    for i in vf..total {
        if running >= scroll_top + client_height {
            vl = i;
            break;
        }
        running += height_of(i).max(0.0);
        vl = i + 1;
    }

    // ── Apply overscan and clamp ──────────────────────────────────
    let first = vf.saturating_sub(overscan);
    let last = (vl + overscan).min(total);

    // ── Spacer math ───────────────────────────────────────────────
    //
    // Recompute leading from scratch because overscan may have
    // shifted `first` below `vf`.  Trailing is the suffix sum from
    // `last..total`.
    let leading_spacer = (0..first).map(&height_of).map(|h| h.max(0.0)).sum::<f64>();
    let trailing_spacer = (last..total)
        .map(&height_of)
        .map(|h| h.max(0.0))
        .sum::<f64>();

    VisibleWindow {
        first,
        last,
        leading_spacer,
        trailing_spacer,
    }
}

/// Running-mean estimate of message heights.
///
/// A separate type because callers update it from inside a closure
/// that doesn't have access to the full `HeightCache` struct (which
/// holds RwSignals).  Pure arithmetic, native-target testable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RunningMean {
    /// Running mean of all observed heights, in CSS pixels.
    pub mean: f64,
    /// Count of observations folded into the mean.  Used to weight
    /// new observations.
    pub n: u64,
}

impl RunningMean {
    /// Bootstrap with a sensible default before any observations.
    pub const fn new() -> Self {
        Self {
            mean: INITIAL_AVERAGE_HEIGHT_PX,
            n: 0,
        }
    }

    /// Fold a new observation into the mean.
    ///
    /// Standard online mean: `mean_n = mean_{n-1} + (x - mean_{n-1}) / n`.
    /// Cheap (one division, one subtraction, two additions) and
    /// numerically well-behaved for the height ranges we care about.
    ///
    /// Negative observations are clamped to zero — we never want a
    /// negative estimate to shrink the running mean.
    pub fn observe(&mut self, sample: f64) {
        let sample = sample.max(0.0);
        self.n = self.n.saturating_add(1);
        // n is at least 1 here.
        let delta = sample - self.mean;
        self.mean += delta / (self.n as f64);
    }
}

impl Default for RunningMean {
    fn default() -> Self {
        Self::new()
    }
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Constant-height helper for tests: every message is `h` px tall.
    fn flat(h: f64) -> impl Fn(usize) -> f64 {
        move |_| h
    }

    /// Heights from a slice; out-of-bounds is 0 (mirrors clamp).
    fn slice_heights(heights: &'static [f64]) -> impl Fn(usize) -> f64 {
        move |i| heights.get(i).copied().unwrap_or(0.0)
    }

    #[test]
    fn empty_list_yields_empty_window() {
        let w = visible_window(0.0, 600.0, 0, VIRTUAL_OVERSCAN, flat(100.0));
        assert_eq!(w, VisibleWindow::EMPTY);
    }

    #[test]
    fn whole_short_list_fits_inside_overscan() {
        // 10 messages × 100 px = 1000 px total.  Viewport is 600 px.
        // Overscan of 20 trivially encompasses everything.
        let w = visible_window(0.0, 600.0, 10, VIRTUAL_OVERSCAN, flat(100.0));
        assert_eq!(w.first, 0);
        assert_eq!(w.last, 10);
        assert_eq!(w.leading_spacer, 0.0);
        assert_eq!(w.trailing_spacer, 0.0);
    }

    #[test]
    fn anchored_to_bottom_long_list_mounts_only_tail_plus_overscan() {
        // 500 messages × 100 px = 50 000 px total.  Viewport is 600 px,
        // scrolled to the floor (anchor-to-now behaviour).
        //
        // Raw visible: [494, 500) (6 messages of 100 px each fill 600 px).
        // With overscan 20: first = 474, last = 500.
        let w = visible_window(49_400.0, 600.0, 500, VIRTUAL_OVERSCAN, flat(100.0));
        assert_eq!(w.first, 474);
        assert_eq!(w.last, 500);
        // Leading = 474 × 100 = 47 400; trailing = 0.
        assert!((w.leading_spacer - 47_400.0).abs() < 1.0);
        assert_eq!(w.trailing_spacer, 0.0);
    }

    #[test]
    fn scrolled_to_top_long_list_mounts_head_plus_overscan() {
        // Same 500-msg corpus, scrolled to top.  Raw visible: [0, 6).
        // Overscan extends to last = 26.
        let w = visible_window(0.0, 600.0, 500, VIRTUAL_OVERSCAN, flat(100.0));
        assert_eq!(w.first, 0);
        assert_eq!(w.last, 26);
        assert_eq!(w.leading_spacer, 0.0);
        // Trailing = (500 - 26) × 100 = 47 400.
        assert!((w.trailing_spacer - 47_400.0).abs() < 1.0);
    }

    #[test]
    fn scrolled_to_middle_mounts_window_around_visible() {
        // Scrolled so messages [200, 206) are visible.
        let w = visible_window(20_000.0, 600.0, 500, VIRTUAL_OVERSCAN, flat(100.0));
        assert_eq!(w.first, 180);
        assert_eq!(w.last, 226);
        assert!((w.leading_spacer - 18_000.0).abs() < 1.0);
        assert!((w.trailing_spacer - 27_400.0).abs() < 1.0);
    }

    #[test]
    fn variable_heights_compute_correct_spacers() {
        // Mixed heights: short bubble, tall code block, short, …
        const HEIGHTS: &[f64] = &[50.0, 800.0, 50.0, 50.0, 50.0, 800.0, 50.0, 50.0, 50.0, 50.0];
        // Visible viewport from 100 px (mid first tall block) for 600 px.
        let w = visible_window(100.0, 600.0, HEIGHTS.len(), 0, slice_heights(HEIGHTS));
        // The first visible message is the 800 px one at index 1
        // (sum 0..1 = 50 ≤ 100; sum 0..2 = 850 > 100).
        assert_eq!(w.first, 1);
        // last walks until running >= 100+600 = 700.
        // After mounting index 1 (800 px), running = 50 + 800 = 850 ≥ 700,
        // so vl = 2.  No overscan, so last = 2.
        assert_eq!(w.last, 2);
        assert_eq!(w.leading_spacer, 50.0);
        // Trailing = sum of indices 2..10 with the listed heights.
        let trailing: f64 = HEIGHTS[2..].iter().sum();
        assert!((w.trailing_spacer - trailing).abs() < 1.0);
    }

    #[test]
    fn overscan_clamps_at_list_bounds() {
        // 4 messages, scrolled past the bottom (defensive case).  All
        // four should mount, no negative spacers.
        let w = visible_window(99_999.0, 600.0, 4, 10, flat(100.0));
        assert_eq!(w.first, 0);
        assert_eq!(w.last, 4);
        assert_eq!(w.leading_spacer, 0.0);
        assert_eq!(w.trailing_spacer, 0.0);
    }

    #[test]
    fn negative_metrics_are_clamped_to_zero() {
        // A real browser shouldn't give us this, but we've seen it.
        let w = visible_window(-100.0, -600.0, 50, VIRTUAL_OVERSCAN, flat(100.0));
        // scroll_top clamped to 0, client_height clamped to 0.
        // Raw window degenerates: the find-vf loop picks vf=0 (height
        // of index 0 > 0 = scroll_top); the find-vl loop sees
        // running (0) >= scroll_top+client_height (0) on first
        // iteration and breaks immediately with vl=0.
        // So raw window is the empty range [0..0); overscan extends
        // last to min(0+20, 50) = 20.
        assert_eq!(w.first, 0);
        assert_eq!(w.last, 20);
    }

    #[test]
    fn running_mean_bootstraps_to_initial_average() {
        let m = RunningMean::new();
        assert_eq!(m.mean, INITIAL_AVERAGE_HEIGHT_PX);
        assert_eq!(m.n, 0);
    }

    #[test]
    fn running_mean_first_observation_replaces_bootstrap() {
        let mut m = RunningMean::new();
        m.observe(200.0);
        // mean_1 = bootstrap + (200 - bootstrap) / 1 = 200.
        assert!((m.mean - 200.0).abs() < 1e-9);
        assert_eq!(m.n, 1);
    }

    #[test]
    fn running_mean_converges_to_steady_state() {
        // Feed 1000 samples around a steady 150 px mean.
        let mut m = RunningMean::new();
        for i in 0..1000 {
            // Alternate 140 / 160 to give an exact mean of 150.
            m.observe(if i % 2 == 0 { 140.0 } else { 160.0 });
        }
        assert!(
            (m.mean - 150.0).abs() < 0.5,
            "running mean should track to 150, got {}",
            m.mean
        );
    }

    #[test]
    fn running_mean_clamps_negative_observations() {
        let mut m = RunningMean::new();
        m.observe(-50.0);
        // -50 clamped to 0; mean_1 = bootstrap + (0 - bootstrap) / 1 = 0.
        assert!((m.mean - 0.0).abs() < 1e-9);
        assert_eq!(m.n, 1);
    }
}
