// SPDX-License-Identifier: GPL-3.0-or-later

//! Scrollable, virtualised chat message list.
//!
//! # Anchor-to-now (gander#119)
//!
//! Chat is bottom-anchored.  These pieces stay from PR 1:
//!
//! 1. **`replaying` snap.**  When `replaying` transitions from `true` to
//!    `false` we snap the viewport to the bottom on next animation
//!    frame.  Opening a session means "you're at now".
//! 2. **`at_bottom` tracking.**  A scroll listener updates an
//!    `at_bottom: RwSignal<bool>` whenever the user scrolls.  The pure
//!    arithmetic lives in [`crate::acp_core::components::scroll`].
//! 3. **Live snap + "↓ N new" pill.**  An effect watches
//!    `messages.len()`: if at bottom, snap; else increment the pill
//!    counter.
//! 4. **ResizeObserver on the viewport** so token streaming into a
//!    settled bubble — which doesn't change `messages.len()` but does
//!    change content height — still snaps when at-bottom.
//!
//! # Virtualisation (gander#124)
//!
//! Adds an overscan windowing layer on top of PR 1.  Instead of
//! mounting every message, we only render messages inside
//! `[visible_first - OVERSCAN, visible_last + OVERSCAN]` and replace
//! the unmounted prefix and suffix with single spacer `<div>`s sized
//! by the running-mean height estimate.
//!
//! ## Heights are estimate-only in this PR
//!
//! Every message contributes `INITIAL_AVERAGE_HEIGHT_PX` to the
//! virtualiser's spacer math (see `virtual_list.rs`).  That means:
//!
//! - **Correctness:** the mount window is approximately right even
//!   when individual messages differ from the estimate by 4–5×, so
//!   long-session rendering stays within the frame budget.  Overscan
//!   absorbs the slop.
//! - **Trade-off:** `scrollHeight` is `total * estimate` rather than
//!   `sum(real heights)`.  The scrollbar thumb size and position
//!   shift slightly as the user scrolls and the *mounted* portion's
//!   real heights interact with the spacers.  In practice this reads
//!   as "the scrollbar isn't perfectly accurate" rather than "things
//!   are broken".
//!
//! Real-height measurement is a follow-up (see #124 acceptance note
//! — promotes to a sibling PR if QA shows the jitter matters).
//!
//! The keyed `<For>` still keys on `msg.id`, so messages
//! re-entering the window from above or below re-mount cleanly with
//! preserved per-message state (`expanded`, `ui_html`, etc. all
//! live on `ChatMessage`).
//!
//! Pure arithmetic for the window math lives in
//! [`crate::acp_core::components::virtual_list`]; this module wires
//! the DOM side together.

use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use leptos_icons::Icon;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::acp_core::components::message_view::MessageView;
use crate::acp_core::components::scroll::at_bottom_from_metrics;
use crate::acp_core::components::virtual_list::{
    visible_window, VisibleWindow, INITIAL_AVERAGE_HEIGHT_PX, VIRTUAL_OVERSCAN,
};
use crate::acp_core::types::ChatMessage;

/// Scrollable list of all chat messages, virtualised.
///
/// `replaying` is observed so the list can snap to the bottom exactly
/// once per session-load transition.  See module docs.
#[component]
pub fn MessageList(
    messages: RwSignal<Vec<ChatMessage>>,
    replaying: RwSignal<bool>,
) -> impl IntoView {
    let is_empty = move || messages.get().is_empty();

    // ── DOM ref ─────────────────────────────────────────────────────
    let scroll_ref: NodeRef<html::Div> = NodeRef::new();

    // ── PR 1 signals (anchor + pill) ────────────────────────────────
    let at_bottom: RwSignal<bool> = RwSignal::new(true);
    let pending_below: RwSignal<u32> = RwSignal::new(0);
    let prev_len: RwSignal<usize> = RwSignal::new(0);
    let prev_replaying: RwSignal<bool> = RwSignal::new(false);

    // ── PR 2 signals (virtualisation) ───────────────────────────────
    //
    // `scroll_top` / `client_height` are written by the DOM listeners
    // and read by the window recompute.  `window` is the derived
    // mount range that the view reads.
    let scroll_top: RwSignal<f64> = RwSignal::new(0.0);
    let client_height: RwSignal<f64> = RwSignal::new(0.0);
    let window: RwSignal<VisibleWindow> = RwSignal::new(VisibleWindow::EMPTY);

    // ── snap helpers ────────────────────────────────────────────────
    let snap_to_bottom = move || {
        let Some(el) = scroll_ref.get_untracked() else {
            return;
        };
        request_animation_frame(move || {
            el.set_scroll_top(el.scroll_height());
        });
    };

    let snap_and_reset = move || {
        pending_below.set(0);
        at_bottom.set(true);
        snap_to_bottom();
    };

    // ── recompute the window ────────────────────────────────────────
    //
    // Reads `messages.len()`, `scroll_top`, `client_height`; writes
    // the new `VisibleWindow` to `window` and flips per-message
    // `visible` signals.
    //
    // Heights are estimate-only in this PR (see module docs).
    let recompute_window = move || {
        let total = messages.with_untracked(|v| v.len());
        let st = scroll_top.get_untracked();
        let ch = client_height.get_untracked();
        let new_window = visible_window(
            st,
            ch,
            total,
            VIRTUAL_OVERSCAN,
            // All-estimate height lookup.  Cheap (one constant), and
            // the spacer math is consistent: every message contributes
            // the same height to both spacers and to the mount window
            // calculation.
            |_| INITIAL_AVERAGE_HEIGHT_PX,
        );
        if window.with_untracked(|w| *w != new_window) {
            window.set(new_window);
        }
        // Flip per-message `visible` to match — gates per-message
        // work in MessageView, ToolCallCard, McpAppIframe.  Borrows
        // the messages vec rather than cloning; this fires on every
        // scroll tick so the clone-per-call cost adds up fast.
        messages.with_untracked(|v| sync_visibility(v, new_window));
    };

    // ── one-shot listener install ───────────────────────────────────
    let recompute_for_install = recompute_window.clone();
    Effect::new(move |installed: Option<bool>| {
        if installed.unwrap_or(false) {
            return true;
        }
        let Some(view_el) = scroll_ref.get() else {
            return false;
        };

        install_scroll_listener(
            &view_el,
            at_bottom,
            pending_below,
            scroll_top,
            recompute_for_install.clone(),
        );
        install_viewport_resize_observer(
            &view_el,
            at_bottom,
            client_height,
            recompute_for_install.clone(),
        );

        // First-paint window is computed against (scrollTop=0, clientHeight=0)
        // which gives nothing visible.  Re-run now that we've read
        // real dimensions from the DOM.
        client_height.set(view_el.client_height() as f64);
        scroll_top.set(view_el.scroll_top() as f64);
        recompute_for_install();
        true
    });

    // ── messages-watch effect: snap, count, recompute ───────────────
    //
    // Three jobs.  Order matters:
    //   1. If we're about to snap, pre-set `scroll_top` to the target
    //      so the recompute mounts the bottom window — without this
    //      the recompute mounts [0..N) and the snap then lands the
    //      viewport on the *trailing spacer*, flashing empty for one
    //      frame before the scroll event triggers a follow-up
    //      recompute.
    //   2. Recompute the window (new total, new scroll_top).
    //   3. Schedule the actual snap on the next RAF.
    let recompute_for_messages = recompute_window.clone();
    Effect::new(move |_| {
        let len = messages.with(|v| v.len());
        let prev = prev_len.get_untracked();
        prev_len.set(len);

        let grew = len > prev;
        let anchored = at_bottom.get_untracked();

        if grew && !anchored {
            let delta = (len - prev) as u32;
            pending_below.update(|n| *n = n.saturating_add(delta));
        }

        if grew && anchored {
            scroll_top.set(target_bottom_scroll(len, client_height.get_untracked()));
        }

        recompute_for_messages();

        if grew && anchored {
            snap_to_bottom();
        }
    });

    // ── replaying-edge effect: "you just opened a session" ──────────
    //
    // Fires once on the true → false transition, always snaps,
    // always clears the pill.  Documented load-bearing — do not
    // optimise away.  Same pre-snap ordering as the messages effect
    // for the same reason.
    let recompute_for_replay = recompute_window.clone();
    Effect::new(move |_| {
        let now = replaying.get();
        let was = prev_replaying.get_untracked();
        prev_replaying.set(now);
        if was && !now {
            pending_below.set(0);
            at_bottom.set(true);
            let len = messages.with_untracked(|v| v.len());
            scroll_top.set(target_bottom_scroll(len, client_height.get_untracked()));
            recompute_for_replay();
            snap_to_bottom();
        }
    });

    view! {
        <div class="message-list-shell">
            <div class="message-list" node_ref=scroll_ref>
                {move || {
                    is_empty()
                        .then(|| {
                            view! {
                                <div class="empty-state">
                                    <img
                                        src="/assets/gander.png"
                                        alt="gander"
                                        class="empty-state-logo"
                                    />
                                </div>
                            }
                        })
                }}
                // Leading spacer — replaces unmounted prefix.
                <div
                    class="message-list-spacer message-list-spacer--leading"
                    style=move || {
                        format!("height: {}px", window.with(|w| w.leading_spacer))
                    }
                />
                <For
                    each=move || {
                        // Iterate only the visible slice — `messages.with`
                        // borrows rather than cloning the whole vec, and
                        // we copy just the window into a fresh Vec.  Per
                        // scroll tick that's at most
                        // `(visible + 2 * VIRTUAL_OVERSCAN)` `Copy`
                        // `ChatMessage` entries (~40), not the entire
                        // transcript.  Key on `msg.id` so messages
                        // re-entering the window from either direction
                        // re-mount cleanly with preserved per-message
                        // state.
                        let w = window.get();
                        messages.with(|v| {
                            let lo = w.first.min(v.len());
                            let hi = w.last.min(v.len()).max(lo);
                            v[lo..hi].to_vec()
                        })
                    }
                    key=|msg| msg.id
                    children=|msg| view! { <MessageView message=msg /> }
                />
                // Trailing spacer — replaces unmounted suffix.
                <div
                    class="message-list-spacer message-list-spacer--trailing"
                    style=move || {
                        format!("height: {}px", window.with(|w| w.trailing_spacer))
                    }
                />
            </div>
            {move || {
                let n = pending_below.get();
                (n > 0).then(|| {
                    let label = if n == 1 {
                        "1 new message".to_string()
                    } else {
                        format!("{n} new messages")
                    };
                    let title_label = label.clone();
                    view! {
                        <button
                            class="scroll-pill"
                            title=title_label
                            on:click=move |_: ev::MouseEvent| snap_and_reset()
                        >
                            <Icon icon=icondata::LuChevronDown width="14px" height="14px" />
                            <span class="scroll-pill-label">{label}</span>
                        </button>
                    }
                })
            }}
        </div>
    }
}

// ─── DOM-side helpers ────────────────────────────────────────────────────────
//
// All closures are leaked into the DOM (`.forget()` / `std::mem::forget`)
// because the WASM module is a single-page mount with no Drop point;
// they live for the document's lifetime.

/// Flip each message's `visible` signal based on whether it falls
/// inside the mount window.  Only writes when the value actually
/// changes — cheaper than entering the reactive machinery for a
/// no-op.
fn sync_visibility(messages: &[ChatMessage], w: VisibleWindow) {
    for (i, msg) in messages.iter().enumerate() {
        let should_be_visible = i >= w.first && i < w.last;
        if msg.visible.get_untracked() != should_be_visible {
            msg.visible.set(should_be_visible);
        }
    }
}

/// Wire up the `scroll` listener.  Updates `at_bottom`, collapses
/// the pill on deliberate-scroll-to-bottom, writes the new
/// `scroll_top`, and triggers a window recompute.
fn install_scroll_listener<R>(
    el: &web_sys::HtmlDivElement,
    at_bottom: RwSignal<bool>,
    pending_below: RwSignal<u32>,
    scroll_top: RwSignal<f64>,
    mut recompute: R,
) where
    R: FnMut() + 'static,
{
    let el_for_read = el.clone();
    let cb = Closure::wrap(Box::new(move |_: web_sys::Event| {
        let st = el_for_read.scroll_top() as f64;
        let sh = el_for_read.scroll_height() as f64;
        let ch = el_for_read.client_height() as f64;
        let now_at_bottom = at_bottom_from_metrics(st, sh, ch);

        if at_bottom.get_untracked() != now_at_bottom {
            at_bottom.set(now_at_bottom);
        }
        if now_at_bottom && pending_below.get_untracked() > 0 {
            pending_below.set(0);
        }
        // 0.5 px hysteresis avoids redundant signal writes on
        // sub-pixel scroll deltas.
        if (scroll_top.get_untracked() - st).abs() > 0.5 {
            scroll_top.set(st);
        }
        recompute();
    }) as Box<dyn FnMut(web_sys::Event)>);
    let _ = el.add_event_listener_with_callback("scroll", cb.as_ref().unchecked_ref());
    cb.forget();
}

/// Watch the viewport's own size.  When the window or pane resizes,
/// `clientHeight` changes and so does the visible-row count.
///
/// Also handles the "stay glued to the bottom while the *viewport*
/// shrinks" case (e.g. opening DevTools).  Without this, a resize
/// that pushes the bottom of the content below the new visible
/// region would silently un-anchor.
fn install_viewport_resize_observer<R>(
    el: &web_sys::HtmlDivElement,
    at_bottom: RwSignal<bool>,
    client_height: RwSignal<f64>,
    mut recompute: R,
) where
    R: FnMut() + 'static,
{
    let el_for_callback = el.clone();
    let cb = Closure::wrap(Box::new(
        move |_entries: js_sys::Array, _observer: web_sys::ResizeObserver| {
            let ch = el_for_callback.client_height() as f64;
            if (client_height.get_untracked() - ch).abs() > 0.5 {
                client_height.set(ch);
            }
            // Content grew inside the viewport too — keep us glued
            // to the bottom if we were already there.
            if at_bottom.get_untracked() {
                let el2 = el_for_callback.clone();
                request_animation_frame(move || {
                    el2.set_scroll_top(el2.scroll_height());
                });
            }
            recompute();
        },
    )
        as Box<dyn FnMut(js_sys::Array, web_sys::ResizeObserver)>);

    let observer = match web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
        Ok(o) => o,
        Err(err) => {
            web_sys::console::warn_1(&err);
            cb.forget();
            return;
        }
    };
    observer.observe(el);
    cb.forget();
    std::mem::forget(observer);
}

/// Estimated scrollTop value that puts the bottom of the (estimated)
/// content at the bottom of the viewport.  Used to pre-bias
/// `scroll_top` before the window recompute so the messages-watch
/// effect can mount the tail rather than the head when it's about
/// to snap.
///
/// Total content height is `total * INITIAL_AVERAGE_HEIGHT_PX` in
/// this PR (estimate-only — see module docs).  The clamp at zero
/// covers the empty-list and small-list cases without a special
/// branch.
fn target_bottom_scroll(total: usize, client_height: f64) -> f64 {
    let total_h = (total as f64) * INITIAL_AVERAGE_HEIGHT_PX;
    (total_h - client_height).max(0.0)
}

/// Schedule `f` to run on the next browser animation frame.
fn request_animation_frame<F: FnOnce() + 'static>(f: F) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let cb = Closure::once_into_js(f);
    let _ = window.request_animation_frame(cb.unchecked_ref());
}
