// SPDX-License-Identifier: GPL-3.0-or-later

//! Time-ago formatting helper.

use wasm_bindgen::JsValue;

/// Format an ISO 8601 timestamp as a short "time ago" string.
///
/// Uses `js_sys::Date` to parse the timestamp and compute the elapsed days
/// accurately, including correct handling of variable-length months.
pub fn time_ago(iso: &str) -> String {
    let then = js_sys::Date::new(&JsValue::from_str(iso));
    // `Date::new` with an unparseable string produces NaN for `getTime()`.
    let then_ms = then.get_time();
    if then_ms.is_nan() {
        return "(unknown)".to_string();
    }

    let now_ms = js_sys::Date::now();
    let diff_ms = now_ms - then_ms;
    if diff_ms < 0.0 {
        return "just now".to_string();
    }

    let diff_mins = (diff_ms / 60_000.0) as u64;
    let diff_hours = diff_mins / 60;
    let diff_days = diff_hours / 24;

    match diff_mins {
        0..=1 => "just now".to_string(),
        2..=59 => format!("{diff_mins}m ago"),
        60..=119 => "1h ago".to_string(),
        _ if diff_hours < 24 => format!("{diff_hours}h ago"),
        _ if diff_days == 1 => "yesterday".to_string(),
        _ if diff_days < 7 => format!("{diff_days}d ago"),
        _ if diff_days < 14 => "1w ago".to_string(),
        _ if diff_days < 30 => format!("{}w ago", diff_days / 7),
        _ if diff_days < 365 => format!("{}mo ago", diff_days / 30),
        _ => format!("{}y ago", diff_days / 365),
    }
}
