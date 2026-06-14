// SPDX-License-Identifier: GPL-3.0-or-later

mod acp;
mod app;
mod config;
mod ext;
mod i18n;
mod state;
mod tab;
mod transport;
#[cfg(target_os = "linux")]
mod webview;

use std::process;

fn main() -> cosmic::iced::Result {
    install_panic_hook();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("gander=info,warn")),
        )
        .with_target(false)
        .init();

    // GTK must be initialised on the main thread before any wry/WebKitGTK
    // operations.  cosmic/winit does not use GTK, so this has no side-effects
    // on the iced window stack; it just opens the X11/Wayland display
    // connection that GTK and WebKitGTK need internally.
    #[cfg(target_os = "linux")]
    if gtk::init().is_err() {
        eprintln!("gander: failed to initialise GTK (required for the wry webview backend)");
        process::exit(1);
    }

    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();
    i18n::init(&requested_languages);

    let flags = match app::Flags::load() {
        Ok(flags) => flags,
        Err(error) => {
            eprintln!("gander: failed to initialise: {error:?}");
            process::exit(1);
        }
    };

    let settings = cosmic::app::Settings::default().size_limits(
        cosmic::iced::Limits::NONE
            .min_width(640.0)
            .min_height(400.0),
    );

    cosmic::app::run::<app::AppModel>(settings, flags)
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let message = panic_message(panic_info.payload());
        if matches!(
            message,
            Some(msg) if is_winit_xim_glx_bad_window_panic_message(msg)
        ) {
            eprintln!("gander: GLXBadWindow on shutdown (winit/xim bug, ignored)");
            return;
        }

        default_hook(panic_info);
    }));
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return Some(message);
    }
    payload.downcast_ref::<String>().map(String::as_str)
}

fn is_winit_xim_glx_bad_window_panic_message(message: &str) -> bool {
    message.contains("Failed to unfocus input context")
        && message.contains("description: \"GLXBadWindow\"")
}

#[cfg(test)]
mod tests {
    use super::is_winit_xim_glx_bad_window_panic_message;

    #[test]
    fn matches_winit_xim_glx_bad_window_panic_message() {
        let message = "Failed to unfocus input context: XError { description: \"GLXBadWindow\", error_code: 168, request_code: 149, minor_code: 32 }";
        assert!(is_winit_xim_glx_bad_window_panic_message(message));
    }

    #[test]
    fn does_not_match_other_panics() {
        assert!(!is_winit_xim_glx_bad_window_panic_message(
            "Failed to unfocus input context: XError { description: \"BadWindow\" }"
        ));
        assert!(!is_winit_xim_glx_bad_window_panic_message(
            "thread 'main' panicked at something else"
        ));
    }
}
