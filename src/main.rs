// SPDX-License-Identifier: GPL-3.0-or-later

mod acp;
mod app;
mod config;
mod i18n;
mod state;
mod supervisor;
mod tab;
#[cfg(target_os = "linux")]
mod webview;

use std::process;

fn main() -> cosmic::iced::Result {
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
