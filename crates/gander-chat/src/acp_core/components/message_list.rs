// SPDX-License-Identifier: GPL-3.0-or-later

//! Scrollable chat message list.
//!
//! # Anchor-to-now (gander#119)
//!
//! Chat is bottom-anchored.  Three pieces of behaviour live here:
//!
//! 1. **`replaying` snap.**  When `replaying` transitions from `true` to
//!    `false` (i.e. `session_load_end` just fired and `App()` flushed the
//!    replay buffer into `messages`), the viewport snaps to the bottom
//!    on the next animation frame, regardless of where the user was
//!    looking.  Opening a session means "you're at now".
//!
//! 2. **`at_bottom` tracking.**  A scroll listener updates an
//!    `at_bottom: RwSignal<bool>` whenever the user scrolls.  The pure
//!    arithmetic lives in [`crate::acp_core::components::scroll`] so the
//!    Web-free fallback is testable on the native target.
//!
//! 3. **Live snap + "↓ N new" pill.**  An effect watches `messages.len()`:
//!    if the user is at the bottom, snap; otherwise increment a
//!    `pending_below` counter.  A small absolute-positioned pill inside
//!    the list shell shows the counter and snaps + resets on click.
//!
//!    A `ResizeObserver` on the scroll container handles the
//!    stay-glued-to-the-bottom case where tokens flow into a settled
//!    bubble: `scrollHeight` grows but `messages.len()` doesn't.

use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use leptos_icons::Icon;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::acp_core::components::message_view::MessageView;
use crate::acp_core::components::scroll::at_bottom_from_metrics;
use crate::acp_core::types::ChatMessage;

/// Scrollable list of all chat messages.
///
/// `replaying` is observed so the list can snap to the bottom exactly
/// once per session-load transition.  See module docs.
#[component]
pub fn MessageList(
    messages: RwSignal<Vec<ChatMessage>>,
    replaying: RwSignal<bool>,
) -> impl IntoView {
    let is_empty = move || messages.get().is_empty();

    // NodeRef to the scrollable viewport so we can drive scrollTop /
    // scrollHeight directly.  Populated by Leptos after the first render.
    let scroll_ref: NodeRef<html::Div> = NodeRef::new();

    // Whether the viewport is currently within the "anchored to now"
    // epsilon (see `scroll::AT_BOTTOM_EPSILON_PX`).  Starts true: an
    // empty list is trivially at the bottom, and we want the first
    // settled turn to snap rather than light up the pill.
    let at_bottom: RwSignal<bool> = RwSignal::new(true);

    // How many new messages have arrived since the user last looked at
    // the bottom.  Drives the "↓ N new" pill.  Reset by a snap.
    let pending_below: RwSignal<u32> = RwSignal::new(0);

    // Previous len, so the messages-watch effect can compute a delta.
    // `untracked` write so we don't create a self-dependency.
    let prev_len: RwSignal<usize> = RwSignal::new(0);

    // Previous `replaying` state, used to fire the "session just opened"
    // snap on the true→false edge.  Same untracked-write rule.
    let prev_replaying: RwSignal<bool> = RwSignal::new(false);

    // ── snap helper ─────────────────────────────────────────────────
    //
    // Schedules the actual scroll write on the next animation frame so
    // the keyed `<For>` has committed the new DOM (and therefore
    // `scrollHeight`) before we read it.
    let snap_to_bottom = move || {
        let Some(el) = scroll_ref.get_untracked() else {
            return;
        };
        request_animation_frame(move || {
            // scrollTop = scrollHeight clamps to the maximum scroll, so
            // even if scrollHeight grows again before the next frame
            // we still end up pinned to the floor.
            el.set_scroll_top(el.scroll_height());
        });
    };

    let snap_and_reset = move || {
        pending_below.set(0);
        at_bottom.set(true);
        snap_to_bottom();
    };

    // ── one-shot listener install ───────────────────────────────────
    //
    // Effect tracks `scroll_ref`; it fires once the ref becomes Some
    // (the first time Leptos mounts the div).  Returns a `bool` it
    // carries forward to subsequent runs so the "already installed"
    // gate is per-effect-instance rather than per-component.
    Effect::new(move |installed: Option<bool>| {
        if installed.unwrap_or(false) {
            return true;
        }
        let Some(el) = scroll_ref.get() else {
            return false;
        };

        install_scroll_listener(&el, at_bottom, pending_below);
        install_resize_observer(&el, at_bottom);
        true
    });

    // ── messages-watch effect: snap or count ────────────────────────
    //
    // Fires on every change to `messages` — which during normal
    // operation is the addition of a new bubble or tool card.  Splits
    // on `at_bottom`:
    //
    //   - anchored → snap
    //   - scrolled up → bump pill counter by the delta
    Effect::new(move |_| {
        let len = messages.with(|v| v.len());
        let prev = prev_len.get_untracked();
        prev_len.set(len);

        // Net additions only.  A shrink (e.g. session reload clearing
        // the list) is handled by the `replaying` edge below; the
        // counter should not climb when messages vanish.
        if len > prev {
            if at_bottom.get_untracked() {
                snap_to_bottom();
            } else {
                let delta = (len - prev) as u32;
                pending_below.update(|n| *n = n.saturating_add(delta));
            }
        }
    });

    // ── replaying-edge effect: "you just opened a session" ──────────
    //
    // On the true → false transition (which is `session_load_end`'s
    // tail) we always snap, regardless of `at_bottom`.  This is the
    // load-bearing piece of the anchor-to-now behaviour: opening a
    // session must land the user at "now", not at the start of
    // history.  Don't optimise this gate away.
    Effect::new(move |_| {
        let now = replaying.get();
        let was = prev_replaying.get_untracked();
        prev_replaying.set(now);
        if was && !now {
            // Replay just finished; `messages` has been swapped in by
            // `session_load_end`.  Snap + clear the pill.
            pending_below.set(0);
            at_bottom.set(true);
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
                                    // Same asset used by .message-avatar — copied to dist/assets/ by Trunk.
                                    <img
                                        src="/assets/gander.png"
                                        alt="gander"
                                        class="empty-state-logo"
                                    />
                                </div>
                            }
                        })
                }}
                <For
                    each=move || messages.get()
                    key=|msg| msg.id
                    children=|msg| view! { <MessageView message=msg /> }
                />
            </div>
            // Pill is a sibling of .message-list inside .message-list-shell
            // so it can position relative to the shell (not the scrolling
            // viewport).  Without the shell the pill would scroll with
            // the content, which is the opposite of what we want.
            {move || {
                let n = pending_below.get();
                (n > 0).then(|| {
                    let label = if n == 1 {
                        "1 new message".to_string()
                    } else {
                        format!("{n} new messages")
                    };
                    // The view! macro evaluates `{label}` (which moves) before
                    // `title=label.clone()` reads it, so cloning at the call
                    // site triggers borrow-after-move.  Bind the title copy
                    // up-front and let the span consume the original.
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
// Split out as free functions so the `Effect::new` body stays readable.
// All closures are leaked into the DOM (`.forget()` / `std::mem::forget`)
// because the WASM module is a single-page mount with no Drop point;
// they live for the document's lifetime.

/// Wire up the `scroll` listener that keeps `at_bottom` in sync with the
/// user's actual position in the viewport.
///
/// Also collapses the pill counter on a deliberate scroll-to-bottom, so
/// pressing End / dragging the scrollbar to the floor counts as
/// "I'm caught up".
fn install_scroll_listener(
    el: &web_sys::HtmlDivElement,
    at_bottom: RwSignal<bool>,
    pending_below: RwSignal<u32>,
) {
    let el_for_read = el.clone();
    let cb = Closure::wrap(Box::new(move |_: web_sys::Event| {
        let st = el_for_read.scroll_top() as f64;
        let sh = el_for_read.scroll_height() as f64;
        let ch = el_for_read.client_height() as f64;
        let now_at_bottom = at_bottom_from_metrics(st, sh, ch);
        // Only write when the value actually changes; signal equality
        // would short-circuit the effect re-run but it's cheaper still
        // not to enter the reactive machinery at all.
        if at_bottom.get_untracked() != now_at_bottom {
            at_bottom.set(now_at_bottom);
        }
        // A deliberate scroll back to the bottom (e.g. End key, scrollbar
        // drag) is the user saying "I'm caught up" — reset the pill.
        if now_at_bottom && pending_below.get_untracked() > 0 {
            pending_below.set(0);
        }
    }) as Box<dyn FnMut(web_sys::Event)>);
    let _ = el.add_event_listener_with_callback("scroll", cb.as_ref().unchecked_ref());
    cb.forget();
}

/// Wire up the `ResizeObserver` that keeps the viewport glued to the
/// bottom while tokens stream into a settled bubble.
///
/// Pill counter is *not* bumped here: in-place growth from streaming is
/// part of the current turn, not a new arrival.  Without this observer
/// we'd un-glue from the bottom as soon as gander started talking,
/// because token chunks mutate a leaf signal — `messages.len()` doesn't
/// change, so the messages-watch effect doesn't fire.
fn install_resize_observer(el: &web_sys::HtmlDivElement, at_bottom: RwSignal<bool>) {
    let el_for_callback = el.clone();
    let cb = Closure::wrap(Box::new(
        move |_entries: js_sys::Array, _observer: web_sys::ResizeObserver| {
            if at_bottom.get_untracked() {
                // Same RAF-deferred write as snap_to_bottom; can't reuse
                // the closure because we don't have access to it here
                // (and the `scroll_ref` it captured would be redundant
                // — the element is right in front of us).
                let el2 = el_for_callback.clone();
                request_animation_frame(move || {
                    el2.set_scroll_top(el2.scroll_height());
                });
            }
        },
    )
        as Box<dyn FnMut(js_sys::Array, web_sys::ResizeObserver)>);

    let observer = match web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
        Ok(o) => o,
        Err(err) => {
            web_sys::console::warn_1(&err);
            // Keep the closure alive even on the failure path so it
            // doesn't get freed under any retained reference.
            cb.forget();
            return;
        }
    };
    observer.observe(el);
    // Leak the closure and observer so they live for the document's
    // lifetime; no Drop point exists for the chat root in a single-page
    // WASM mount.
    cb.forget();
    std::mem::forget(observer);
}

/// Schedule `f` to run on the next browser animation frame.
///
/// Used to defer scroll writes until after Leptos has committed the
/// keyed-`<For>` diff, so `scrollHeight` reflects the just-mounted
/// content.  Without the RAF defer the snap happens against the *old*
/// scrollHeight and ends up one bubble short.
fn request_animation_frame<F: FnOnce() + 'static>(f: F) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let cb = Closure::once_into_js(f);
    let _ = window.request_animation_frame(cb.unchecked_ref());
}
