// SPDX-License-Identifier: GPL-3.0-or-later

//! Footer bar showing session metadata.

use leptos::prelude::*;
use leptos_icons::Icon;

/// Footer bar showing session metadata below the input row.
///
/// Each field is driven by its own signal so only the changed span re-renders.
/// Fields that have not yet been populated show `—` (em-dash).
///
/// ## Layout
///
/// ```text
/// 📁 /home/…  📎  |  — / —  |  model  |  —  |  N tools  |  ⚙
///   cwd      attach  tokens    model   mode   tools   settings
/// ```
#[component]
pub fn Footer(
    cwd: RwSignal<Option<String>>,
    model: RwSignal<Option<String>>,
    tool_count: RwSignal<Option<u32>>,
) -> impl IntoView {
    // Repeated string literals extracted to locals so changes stay in one place.
    const SEP: &str = "|";
    const PLACEHOLDER: &str = "—";

    view! {
        <div class="input-footer">
            // ── cwd ────────────────────────────────────────────────────────
            <span class="footer-cwd" title=move || cwd.get().unwrap_or_default()>
                <Icon icon=icondata::LuFolder width="14px" height="14px" />
                {move || cwd.get().unwrap_or_else(|| PLACEHOLDER.to_string())}
            </span>

            // ── attach (no-op placeholder) ─────────────────────────────────
            <button
                class="footer-btn"
                title="Attach file (not implemented)"
                on:click=|_| {
                    web_sys::console::log_1(
                        &wasm_bindgen::JsValue::from_str("attach not implemented"),
                    );
                }
            >
                <Icon icon=icondata::LuPaperclip width="14px" height="14px" />
            </button>

            <span class="footer-sep">{SEP}</span>

            // ── token usage (placeholder) ──────────────────────────────────
            <span class="footer-tokens">"— / —"</span>

            <span class="footer-sep">{SEP}</span>

            // ── model ──────────────────────────────────────────────────────
            <span class="footer-model">
                {move || model.get().unwrap_or_else(|| PLACEHOLDER.to_string())}
            </span>

            <span class="footer-sep">{SEP}</span>

            // ── mode (placeholder) ─────────────────────────────────────────
            <span class="footer-mode">{PLACEHOLDER}</span>

            <span class="footer-sep">{SEP}</span>

            // ── tool count ─────────────────────────────────────────────────
            <span class="footer-tools">
                {move || {
                    tool_count
                        .get()
                        .map(|n| format!("{n} tools"))
                        .unwrap_or_else(|| PLACEHOLDER.to_string())
                }}
            </span>

            <span class="footer-sep">{SEP}</span>

            // ── settings (no-op placeholder) ──────────────────────────────
            <button
                class="footer-btn"
                title="Settings (not implemented)"
                on:click=|_| {
                    web_sys::console::log_1(
                        &wasm_bindgen::JsValue::from_str("settings not implemented"),
                    );
                }
            >
                <Icon icon=icondata::LuSettings2 width="14px" height="14px" />
            </button>
        </div>
    }
}
