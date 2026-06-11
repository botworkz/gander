// SPDX-License-Identifier: GPL-3.0-or-later

//! Markdown → HTML rendering via [`pulldown_cmark`].
//!
//! The output is injected as raw `innerHTML`.  Content arrives from the LLM
//! via the local gander bridge (same machine, no network attacker), so we
//! treat it as trusted and do not apply an additional HTML sanitiser.  If
//! you expose this client to untrusted content in the future, add a sanitiser
//! (e.g. ammonia compiled to WASM) before this injection point.

use pulldown_cmark::{html, Options, Parser};

/// Render `markdown` to an HTML string.
///
/// Enables the most commonly needed CommonMark extensions:
/// tables, strikethrough, and task lists.
pub fn render(markdown: &str) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;

    let parser = Parser::new_ext(markdown, opts);
    let mut out = String::with_capacity(markdown.len() * 2);
    html::push_html(&mut out, parser);
    out
}
