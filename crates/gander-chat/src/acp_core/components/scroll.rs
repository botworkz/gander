// SPDX-License-Identifier: GPL-3.0-or-later

//! Scroll-position helpers for the message list.
//!
//! Extracted into their own module so the pure arithmetic
//! ([`at_bottom_from_metrics`]) is testable on the native target without
//! pulling in `web-sys`.  The DOM-aware wrappers live in
//! [`crate::acp_core::components::message_list`].

/// Distance from the bottom (in CSS pixels) below which we still consider
/// the viewport "anchored to now" and snap on new messages.
///
/// 60 px lines up with roughly one bubble of breathing room — small enough
/// that a deliberate scroll-up of even a single message-height defeats the
/// snap, large enough that the inevitable few-pixel jitter from font
/// metrics or scrollbar gutters does not.  Tuned by eye; if a future bubble
/// design changes the typical message height we should revisit.
pub const AT_BOTTOM_EPSILON_PX: f64 = 60.0;

/// Returns `true` when the viewport is close enough to the bottom that
/// new content should auto-scroll.
///
/// The three metrics are the raw `Element.scrollTop` / `Element.scrollHeight`
/// / `Element.clientHeight` values — kept as `f64` so callers can pass the
/// `web-sys` getters directly without rounding-trip artefacts.
///
/// Treats degenerate inputs (`client_height >= scroll_height`, i.e. the
/// content does not overflow yet) as "at bottom".  This is the right
/// answer for an empty chat or a tiny one: the user is implicitly already
/// looking at every message.
#[inline]
pub fn at_bottom_from_metrics(scroll_top: f64, scroll_height: f64, client_height: f64) -> bool {
    if client_height >= scroll_height {
        return true;
    }
    (scroll_height - scroll_top - client_height) < AT_BOTTOM_EPSILON_PX
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_short_content_is_at_bottom() {
        // Content shorter than the viewport: nothing to scroll, anchor implicit.
        assert!(at_bottom_from_metrics(0.0, 100.0, 600.0));
        // Exactly viewport-sized: same answer.
        assert!(at_bottom_from_metrics(0.0, 600.0, 600.0));
    }

    #[test]
    fn pinned_to_bottom_is_at_bottom() {
        // 2000 px of content in a 600 px viewport, scrolled all the way down.
        assert!(at_bottom_from_metrics(1400.0, 2000.0, 600.0));
    }

    #[test]
    fn within_epsilon_is_at_bottom() {
        // 30 px above the floor — still counted as anchored.
        assert!(at_bottom_from_metrics(1370.0, 2000.0, 600.0));
    }

    #[test]
    fn beyond_epsilon_is_not_at_bottom() {
        // 120 px above the floor — user has deliberately scrolled up.
        assert!(!at_bottom_from_metrics(1280.0, 2000.0, 600.0));
    }

    #[test]
    fn scrolled_to_top_is_not_at_bottom() {
        assert!(!at_bottom_from_metrics(0.0, 2000.0, 600.0));
    }

    #[test]
    fn fractional_pixel_jitter_does_not_unanchor() {
        // 0.5 px short of perfect — within epsilon, must stay anchored.
        // Guards against the scrollbar-gutter jitter described in the
        // module-level comment.
        assert!(at_bottom_from_metrics(
            1399.5_f64,
            2000.0_f64,
            600.0_f64
        ));
    }
}
