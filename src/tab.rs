// SPDX-License-Identifier: GPL-3.0-or-later

//! Tab content rendering.
//!
//! Each open tab is bound to a `geese` profile by name. The body is currently
//! a minimal placeholder — once embedded goose UI lands (see
//! [`DESIGN.md`](../DESIGN.md)), the body becomes a webview hosting the React
//! UI for `goosed` running with this tab's `GOOSE_PATH_ROOT`.
//!
//! Per-profile *configuration* (path, status, parent, launch) lives in the
//! app's context drawer, not in the tab body — see
//! `AppModel::view_profile_config`. The tab body deliberately does not show
//! that information so the space reads as "this is where goose goes" rather
//! than "this is a settings page".
//!
//! The view returned here is intentionally a single `Element` so swapping the
//! placeholder for an embedded webview later is a localized change.

use cosmic::iced::{Alignment, Length, Task as IcedTask};
use cosmic::prelude::*;
use cosmic::widget;
use cosmic::widget::segmented_button;
use geese::Storage;
use iced_webview::{Action as WebViewAction, PageType, WebView};

use crate::app::Message;
use crate::fl;

/// One open tab.
///
/// Holds only what's needed for the *body* — the profile name (so we know
/// which profile to bind to) and the most recent launch error (so we can
/// surface failures inline without a separate dialog). Profile metadata is
/// resolved against `Storage` at render time; nothing is cached here.
pub struct Tab {
    pub profile: String,
    pub last_launch_error: Option<String>,
    webview: WebView<iced_webview::Servo, Message>,
    webview_live: bool,
}

impl Tab {
    pub fn new(
        profile: impl Into<String>,
        entity: segmented_button::Entity,
    ) -> Self {
        Self {
            profile: profile.into(),
            last_launch_error: None,
            webview: WebView::new()
                .on_create_view(Message::TabWebViewCreated(entity))
                .on_action(move |action| Message::TabWebView(entity, action)),
            webview_live: false,
        }
    }

    pub fn create_webview(&mut self) -> IcedTask<Message> {
        self.webview
            .update(WebViewAction::CreateView(PageType::Url(self.data_url())))
    }

    pub fn finish_webview_creation(&mut self) -> IcedTask<Message> {
        self.webview_live = true;
        self.webview.update(WebViewAction::ChangeView(0))
    }

    pub fn update_webview(&mut self, action: WebViewAction) -> IcedTask<Message> {
        self.webview.update(action)
    }

    pub fn tick_webview(&mut self) -> IcedTask<Message> {
        if self.webview_live {
            self.webview.update(WebViewAction::Update)
        } else {
            IcedTask::none()
        }
    }

    pub fn destroy(&mut self) {
        if self.webview_live {
            let _ = self.webview.update(WebViewAction::CloseView(0));
            self.webview_live = false;
        }
    }

    /// Render the tab body. Checks that the profile still exists on disk; if
    /// it doesn't, surfaces a friendly "no longer exists" message rather than
    /// panicking.
    pub fn view<'a>(
        &'a self,
        entity: segmented_button::Entity,
        storage: &Storage,
    ) -> Element<'a, Message> {
        let space = cosmic::theme::spacing();

        if storage.get(&self.profile).is_err() {
            return widget::container(
                widget::column::with_children(vec![
                    widget::text::title3(self.profile.clone()).into(),
                    widget::text::body(fl!("tab-placeholder-missing")).into(),
                ])
                .spacing(space.space_s)
                .align_x(Alignment::Center),
            )
            .padding(space.space_xl)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
        }

        let webview: Element<'_, Message> = if self.webview_live {
            self.webview.view().map(move |action| Message::TabWebView(entity, action))
        } else {
            widget::container(widget::text::body(format!("goose: {}", self.profile)))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
        };

        let mut column = widget::column::with_capacity(2);
        if let Some(error) = &self.last_launch_error {
            column = column.spacing(space.space_xs).push(widget::text::body(fl!(
                "profile-config-launch-failed",
                error = error.as_str()
            )));
        }

        widget::container(column.push(webview))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn data_url(&self) -> String {
        data_url_for_profile(&self.profile)
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn data_url_for_profile(profile: &str) -> String {
    let html = format!("<h1>goose: {}</h1>", html_escape(profile));
    format!(
        "data:text/html;charset=utf-8,{}",
        percent_encode(html.as_bytes())
    )
}

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len());
    for byte in bytes {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~' => encoded.push(char::from(*byte)),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{data_url_for_profile, html_escape};

    #[test]
    fn escapes_profile_name_for_html() {
        assert_eq!(
            html_escape("<work>&\"'"),
            "&lt;work&gt;&amp;&quot;&#39;"
        );
    }

    #[test]
    fn builds_data_url_from_escaped_html() {
        let url = data_url_for_profile("<tab>");
        assert_eq!(
            url,
            "data:text/html;charset=utf-8,%3Ch1%3Egoose%3A%20%26lt%3Btab%26gt%3B%3C%2Fh1%3E"
        );
    }
}
