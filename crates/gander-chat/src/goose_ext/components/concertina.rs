// SPDX-License-Identifier: GPL-3.0-or-later

//! Collapsible accordion menu for the goose-specific sidebar sections.
//!
//! Both sections ("Extensions" and "Settings") manage goose-private state,
//! so the entire component lives in `goose_ext`.

use leptos::prelude::*;
use leptos_icons::Icon;

/// A single entry in the concertina menu.
struct ConcertinaSection {
    label: &'static str,
    icon: &'static icondata_core::IconData,
}

/// The concertina sections shown below the session list.
// goose-ext: Extensions and Settings are both goose surfaces
const CONCERTINA_SECTIONS: &[ConcertinaSection] = &[
    ConcertinaSection {
        label: "Extensions",
        icon: icondata::LuPuzzle,
    },
    ConcertinaSection {
        label: "Settings",
        icon: icondata::LuSettings2,
    },
];

/// Collapsible accordion menu placed below the sessions list in the sidebar.
///
/// One section open at a time; all sections start collapsed.  Clicking an
/// open section collapses it; clicking a closed section opens it and closes
/// whichever was previously open.
// goose-ext: Extensions + Settings manage goose-private state
#[component]
pub fn Concertina() -> impl IntoView {
    // Index of the currently open section, or `None` when all are collapsed.
    let open: RwSignal<Option<usize>> = RwSignal::new(None);

    view! {
        <div class="concertina">
            {CONCERTINA_SECTIONS
                .iter()
                .enumerate()
                .map(|(idx, section)| {
                    let label = section.label;
                    let icon = section.icon;
                    let is_open = move || open.get() == Some(idx);
                    let on_click = move |_| {
                        open.update(|o| {
                            *o = if *o == Some(idx) { None } else { Some(idx) };
                        });
                    };
                    view! {
                        <div class="concertina-section">
                            <button
                                class=move || {
                                    if is_open() {
                                        "concertina-row concertina-row--open"
                                    } else {
                                        "concertina-row"
                                    }
                                }
                                on:click=on_click
                            >
                                <span class="concertina-icon">
                                    <Icon icon=icon width="15px" height="15px" />
                                </span>
                                <span class="concertina-label">{label}</span>
                                <span class=move || {
                                    if is_open() {
                                        "concertina-chevron concertina-chevron--open"
                                    } else {
                                        "concertina-chevron"
                                    }
                                }>
                                    <Icon
                                        icon=icondata::LuChevronRight
                                        width="14px"
                                        height="14px"
                                    />
                                </span>
                            </button>
                            {move || {
                                is_open()
                                    .then(|| {
                                        view! {
                                            <div class="concertina-content">
                                                <span class="concertina-placeholder">
                                                    "Not yet implemented"
                                                </span>
                                            </div>
                                        }
                                    })
                            }}
                        </div>
                    }
                })
                .collect::<Vec<_>>()}
        </div>
    }
}
