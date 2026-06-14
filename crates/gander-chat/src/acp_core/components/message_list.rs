// SPDX-License-Identifier: GPL-3.0-or-later

//! Scrollable chat message list.

use leptos::prelude::*;

use crate::acp_core::components::message_view::MessageView;
use crate::acp_core::types::ChatMessage;

/// Scrollable list of all chat messages.
#[component]
pub fn MessageList(messages: RwSignal<Vec<ChatMessage>>) -> impl IntoView {
    let is_empty = move || messages.get().is_empty();
    view! {
        <div class="message-list">
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
    }
}
