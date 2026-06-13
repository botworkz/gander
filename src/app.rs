// SPDX-License-Identifier: GPL-3.0-or-later

//! `gander`'s top-level COSMIC [`Application`](cosmic::Application).
//!
//! Holds the open-tab list (one [`Tab`] per profile name), wires the tab-bar
//! to a `segmented_button` model, and persists tab state to disk via
//! [`state::Storage`].
//!
//! ## Pages and drawers
//!
//! The main content area shows one of three things at any given time:
//!
//! - [`Page::Empty`] — no tab open, nudge the user to open one
//! - [`Page::Picker`] — in-window picker for opening/creating a profile tab
//! - [`Page::Tab`] — the active tab's body (currently a placeholder; will
//!   eventually host the embedded goose UI, see DESIGN.md)
//!
//! Two context drawers (mutually exclusive — libcosmic only renders one at a
//! time) overlay the right edge:
//!
//! - [`ContextDrawer::About`] — about-the-app card
//! - [`ContextDrawer::ProfileConfig`] — per-active-tab profile metadata,
//!   "Launch goose" lives here rather than in the tab body
//!
//! ## Flow for a typical user gesture
//!
//! 1. user clicks "+ New tab" → [`Message::ShowNewTabPicker`] → `view()`
//!    swaps to the picker page
//! 2. user picks a profile → [`Message::OpenTab`] → tab is appended and
//!    activated, state is persisted, page returns to [`Page::Tab`]
//! 3. user clicks the gear in the header → [`Message::ToggleProfileConfig`]
//!    → profile-config drawer slides in for the active tab
//! 4. user clicks "Launch goose" in the drawer → [`Message::LaunchGoose`]
//!    → goose is spawned via `geese::Profile::command` with `GOOSE_PATH_ROOT`
//!
//! Launching goose builds a `Command` via `geese::Profile::command` (which
//! sets `GOOSE_PATH_ROOT`) and spawns it detached. Per-tab errors are
//! surfaced in the tab body rather than as a dialog. When embedded goose
//! lands this whole spawn path is replaced.

use std::{collections::HashMap, process::Stdio, time::Duration};

use cosmic::app::context_drawer;
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::prelude::*;
use cosmic::widget::{self, about::About, menu, nav_bar, segmented_button};
use geese::{ProfileMeta, Storage};

use crate::config::Config;
use crate::fl;
use crate::state::{self, State, TabState};
use crate::tab::Tab;
#[cfg(target_os = "linux")]
use crate::webview;
#[cfg(target_os = "linux")]
use cosmic::iced::Rectangle as IcedRectangle;
#[cfg(target_os = "linux")]
use cosmic::widget::rectangle_tracker::{
    RectangleTracker, RectangleUpdate, rectangle_tracker_subscription,
};
#[cfg(target_os = "linux")]
use wry::Rect as WryRect;
#[cfg(target_os = "linux")]
use wry::dpi::{LogicalPosition, LogicalSize};

/// Stable id we hand to libcosmic's [`rectangle_tracker_subscription`] so the
/// app and the widget tree end up talking through the same channel.
///
/// Only one slot is tracked (the active tab's body), so a constant is fine.
#[cfg(target_os = "linux")]
const TAB_BODY_TRACKER_ID: u8 = 0;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Flags constructed up-front in `main` and passed into the cosmic runtime.
///
/// Failures here are fatal (we can't reasonably open a window without a
/// place to put state), so [`Flags::load`] returns `Result`.
#[derive(Clone, Debug)]
pub struct Flags {
    pub geese_storage: Storage,
    pub state_storage: state::Storage,
    pub initial_state: State,
}

impl Flags {
    pub fn load() -> anyhow::Result<Self> {
        let geese_storage = Storage::from_env()?;
        let state_storage = state::Storage::from_env()?;
        let initial_state = state_storage.load()?;
        Ok(Self {
            geese_storage,
            state_storage,
            initial_state,
        })
    }
}

/// Top-level UI state.
pub struct AppModel {
    core: cosmic::Core,
    geese_storage: Storage,
    state_storage: state::Storage,
    config: Config,
    #[allow(dead_code)]
    config_handler: Option<cosmic_config::Config>,

    /// Tab strip backing model. Each entity is one open profile tab.
    tabs: segmented_button::SingleSelectModel,
    /// Tab state by entity. We could stash it in `segmented_button`'s `data`
    /// slot, but a side `HashMap` keeps the borrow-checker happy when we want
    /// to mutate tab state while still rendering the tab strip.
    tab_data: HashMap<segmented_button::Entity, Tab>,

    /// What page is currently shown in the content area.
    page: Page,
    /// Which context drawer (if any) is currently open. libcosmic only allows
    /// one drawer at a time, so this is a sum type, not a set.
    drawer: Option<ContextDrawer>,

    /// Latest snapshot of profile metadata, refreshed on demand. Used by the
    /// new-tab picker.
    known_profiles: Vec<ProfileMeta>,
    /// Input box state for "create profile" on the picker page.
    new_profile_name: String,
    /// Last error from a picker action, surfaced inline.
    picker_error: Option<String>,

    about: About,
    key_binds: HashMap<menu::KeyBind, MenuAction>,

    /// The iced window ID of the main application window. Set on first
    /// `Message::GotMainWindowId`; used by webview creation tasks.
    main_window_id: Option<cosmic::iced::window::Id>,

    /// Last-known logical size of the main window (width, height in logical
    /// pixels). Initialised to the declared minimum size; updated on every
    /// `Message::WindowResized`. Used to position and size WebViews.
    window_size: (f32, f32),

    /// Per-tab WebView store (Linux only). On other platforms this field does
    /// not exist; all `#[cfg(target_os = "linux")]` guards below reference it.
    #[cfg(target_os = "linux")]
    webview_store: webview::WebviewStore,

    /// Tracker handle from libcosmic's `rectangle_tracker` subscription. Set
    /// once on the first `RectangleUpdate::Init` event, then used to wrap the
    /// active tab body so iced's draw pass reports its real on-screen bounds.
    /// `None` until the subscription has produced its init message; in that
    /// window we fall back to the rough `TAB_STRIP_HEIGHT` constant.
    #[cfg(target_os = "linux")]
    rect_tracker: Option<RectangleTracker<u8>>,

    /// Last reported on-screen bounds of the tab body, in logical pixels.
    /// Updated by `RectangleUpdate::Rectangle` events. The webview is
    /// reparented to this rectangle on every change. `None` until the first
    /// draw lands.
    #[cfg(target_os = "linux")]
    tab_body_bounds: Option<IcedRectangle>,

    /// Tabs that asked for a webview before the rectangle_tracker had fired.
    /// Drained by `Message::TabBodyRect` once the first `Rectangle` update
    /// arrives.
    ///
    /// This exists because of the wry 0.55 X11 `set_bounds` move-is-a-no-op
    /// bug: the position passed to `WebViewBuilder` at creation time is the
    /// *only* position the webview will ever have. So we must not build the
    /// `WebView` until we know the real on-screen rectangle of the tab body.
    #[cfg(target_os = "linux")]
    pending_webviews: Vec<(segmented_button::Entity, String)>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Page {
    /// No tab is open; show a "click + to begin" placeholder.
    #[default]
    Empty,
    /// A profile tab is active; render its body.
    Tab,
    /// The new-tab picker is showing.
    Picker,
}

/// Which overlay drawer is open. Mutually exclusive — libcosmic only renders
/// one drawer at a time, so opening one closes the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextDrawer {
    About,
    ProfileConfig,
}

#[derive(Clone, Debug)]
pub enum Message {
    /// User opened the new-tab picker.
    ShowNewTabPicker,
    /// User dismissed the picker.
    HideNewTabPicker,
    /// User picked an existing profile to open as a new tab.
    OpenTab(String),
    /// Tab strip activation by entity.
    ActivateTab(segmented_button::Entity),
    /// Tab strip close-button.
    CloseTab(segmented_button::Entity),
    /// Picker → input box updates.
    NewProfileNameChanged(String),
    /// Picker → "create" pressed.
    CreateProfile,
    /// Picker → refresh button or implicit refresh after mutation.
    RefreshProfiles,
    /// User pressed "Launch goose" on the profile-config drawer.
    LaunchGoose(String),
    /// `Config` watcher tick.
    UpdateConfig(Config),
    /// About-page link click.
    LaunchUrl(String),
    /// About drawer toggle (menu/keybind).
    ToggleAbout,
    /// Profile-config drawer toggle (gear button in header).
    ToggleProfileConfig,
    /// libcosmic-side drawer close (the drawer's own × button). Closes
    /// whichever drawer is currently open.
    CloseContextDrawer,

    // -----------------------------------------------------------------------
    // Webview / window messages
    // -----------------------------------------------------------------------
    /// Delivered once, shortly after `init()`, with the main window's iced
    /// `Id`. Used by webview creation tasks that need `run_with_handle`.
    GotMainWindowId(cosmic::iced::window::Id),

    /// Signals that a wry `WebView` has been stored in the thread-local
    /// `webview::PENDING` map for `entity` and is ready to be claimed.
    ///
    /// Sent as the return value of `iced::window::run_with_handle` closures.
    /// The value is emitted regardless of whether the webview was actually
    /// created (e.g. it is also emitted on Wayland where `build_as_child`
    /// fails, in which case `claim_pending` finds nothing and is a no-op).
    WebviewReady(segmented_button::Entity),

    /// 60 fps timer tick — pumps the GTK event loop so WebKitGTK can paint
    /// and handle input. Linux-only; on other platforms this message is never
    /// sent.
    PumpGtk,

    /// Window resize event forwarded from the iced subscription.
    WindowResized(cosmic::iced::window::Id, cosmic::iced::Size),

    /// Update from libcosmic's `rectangle_tracker` subscription. We use it to
    /// observe the on-screen bounds of the active tab body widget so the
    /// child wry WebView can be positioned to exactly match — without
    /// hard-coding offsets for the COSMIC header and tab strip.
    #[cfg(target_os = "linux")]
    TabBodyRect(RectangleUpdate<u8>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuAction {
    About,
}

impl menu::action::MenuAction for MenuAction {
    type Message = Message;

    fn message(&self) -> Self::Message {
        match self {
            MenuAction::About => Message::ToggleAbout,
        }
    }
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = Flags;
    type Message = Message;

    const APP_ID: &'static str = "phlax.Gander";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(core: cosmic::Core, flags: Self::Flags) -> (Self, Task<cosmic::Action<Self::Message>>) {
        let Flags {
            geese_storage,
            state_storage,
            initial_state,
        } = flags;

        // Load cosmic-config; fall back to defaults on any error.
        let (config_handler, config) =
            match cosmic_config::Config::new(Self::APP_ID, Config::VERSION) {
                Ok(handler) => {
                    let config = match Config::get_entry(&handler) {
                        Ok(config) => config,
                        Err((_errs, fallback)) => fallback,
                    };
                    (Some(handler), config)
                }
                Err(_) => (None, Config::default()),
            };

        let about = About::default()
            .name(fl!("app-title"))
            .version(env!("CARGO_PKG_VERSION"))
            .links([(fl!("about"), REPOSITORY)])
            .license(env!("CARGO_PKG_LICENSE"));

        let mut app = AppModel {
            core,
            geese_storage,
            state_storage,
            config,
            config_handler,
            tabs: segmented_button::ModelBuilder::default().build(),
            tab_data: HashMap::new(),
            page: Page::Empty,
            drawer: None,
            known_profiles: Vec::new(),
            new_profile_name: String::new(),
            picker_error: None,
            about,
            key_binds: HashMap::new(),
            main_window_id: None,
            // Default to the declared minimum size; updated when the first
            // WindowResized event arrives.
            window_size: (640.0, 400.0),
            #[cfg(target_os = "linux")]
            webview_store: webview::WebviewStore::new(),
            #[cfg(target_os = "linux")]
            rect_tracker: None,
            #[cfg(target_os = "linux")]
            tab_body_bounds: None,
            #[cfg(target_os = "linux")]
            pending_webviews: Vec::new(),
        };

        // Replay persisted tabs against the *current* set of profiles —
        // entries whose profiles have vanished since last run are silently
        // dropped, matching the contract in DESIGN.md.
        app.refresh_known_profiles_inline();
        app.restore_state(&initial_state);

        let title_task = app.update_title();

        // Fire a one-shot task to learn the main window's iced Id. We chain
        // it so that any error (no window yet) is swallowed gracefully; the
        // webview creation path below handles `None`.
        let get_window_id = cosmic::iced::window::oldest().map(|opt_id| {
            cosmic::Action::App(match opt_id {
                Some(id) => Message::GotMainWindowId(id),
                None => {
                    // Should not happen in practice: cosmic creates the main
                    // window before init() returns. Log if it ever does.
                    tracing::warn!(
                        "oldest() returned None during init; \
                         falling back to Id::RESERVED for webview creation"
                    );
                    Message::GotMainWindowId(cosmic::iced::window::Id::RESERVED)
                }
            })
        });

        (app, Task::batch([title_task, get_window_id]))
    }

    fn header_start(&self) -> Vec<Element<'_, Self::Message>> {
        let menu_bar = menu::bar(vec![menu::Tree::with_children(
            menu::root(fl!("view")).apply(Element::from),
            menu::items(
                &self.key_binds,
                vec![menu::Item::Button(fl!("about"), None, MenuAction::About)],
            ),
        )]);
        vec![menu_bar.into()]
    }

    fn header_end(&self) -> Vec<Element<'_, Self::Message>> {
        // Gear is enabled only when there's an active tab whose profile
        // exists. Disabled-state buttons are rendered via the absence of
        // `on_press` — same pattern as the picker.
        let mut config_button =
            widget::button::icon(widget::icon::from_name("preferences-system-symbolic"));
        if self.active_profile_name().is_some() {
            config_button = config_button.on_press(Message::ToggleProfileConfig);
        }
        let config_tooltip = widget::tooltip(
            config_button,
            widget::text::body(fl!("profile-config-tooltip")),
            widget::tooltip::Position::Bottom,
        );

        vec![
            config_tooltip.into(),
            widget::button::standard(fl!("new-tab"))
                .on_press(Message::ShowNewTabPicker)
                .into(),
        ]
    }

    fn nav_model(&self) -> Option<&nav_bar::Model> {
        None
    }

    fn context_drawer(&self) -> Option<context_drawer::ContextDrawer<'_, Self::Message>> {
        if !self.core.window.show_context {
            return None;
        }
        match self.drawer {
            None => None,
            Some(ContextDrawer::About) => Some(context_drawer::about(
                &self.about,
                |url| Message::LaunchUrl(url.to_string()),
                Message::CloseContextDrawer,
            )),
            Some(ContextDrawer::ProfileConfig) => self
                .active_profile_name()
                .map(|name| self.view_profile_config(name)),
        }
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let space = cosmic::theme::spacing();

        let tab_strip: Element<_> = if self.tabs.iter().next().is_some() {
            widget::tab_bar::horizontal(&self.tabs)
                .button_height(32)
                .button_spacing(space.space_xxs)
                .on_activate(Message::ActivateTab)
                .on_close(Message::CloseTab)
                .into()
        } else {
            widget::space::vertical().height(0).into()
        };

        let body_inner: Element<_> = match self.page {
            Page::Empty => self.view_empty(),
            Page::Picker => self.view_picker(),
            Page::Tab => {
                let entity = self.tabs.active();
                match self.tab_data.get(&entity) {
                    Some(tab) => tab.view(&self.geese_storage),
                    None => self.view_empty(),
                }
            }
        };
        #[cfg(target_os = "linux")]
        let body: Element<_> = if let Some(tracker) = self.rect_tracker.as_ref() {
            tracker
                .container(TAB_BODY_TRACKER_ID, body_inner)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            // Tracker subscription hasn't sent its init message yet; render
            // plain so the user sees content immediately. The webview falls
            // back to `TAB_STRIP_HEIGHT` for this first frame.
            body_inner
        };
        #[cfg(not(target_os = "linux"))]
        let body = body_inner;

        widget::column::with_capacity(2)
            .push(tab_strip)
            .push(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::ShowNewTabPicker => {
                self.refresh_known_profiles_inline();
                self.picker_error = None;
                self.page = Page::Picker;
                // Hide any visible webview so it doesn't float over the picker.
                #[cfg(target_os = "linux")]
                self.webview_store.hide_all();
            }
            Message::HideNewTabPicker => {
                self.page = self.default_page();
                // Re-show the active tab's webview when returning to Tab page.
                #[cfg(target_os = "linux")]
                if self.page == Page::Tab {
                    let entity = self.tabs.active();
                    let (x, y, w, h) = self.webview_bounds();
                    self.webview_store.show_only(entity);
                    self.webview_store.set_bounds(entity, x, y, w, h);
                }
            }
            Message::OpenTab(name) => {
                // Track whether this is a brand-new tab (needs a webview) or
                // an existing one being re-activated (webview already exists).
                let already_open = self.tab_data.values().any(|tab| tab.profile == name);

                self.open_tab(&name);
                self.persist_state();

                let title_task = self.update_title();

                #[cfg(target_os = "linux")]
                {
                    let entity = self.tabs.active();
                    let (x, y, w, h) = self.webview_bounds();

                    if already_open {
                        // Switch to the existing webview.
                        self.webview_store.show_only(entity);
                        self.webview_store.set_bounds(entity, x, y, w, h);
                        return title_task;
                    }

                    // New tab. Two preconditions must hold before we can
                    // build the wry WebView:
                    //
                    //   1. The iced window Id is known (set on init via
                    //      `GotMainWindowId`).
                    //   2. `tab_body_bounds` is populated, i.e. the
                    //      rectangle_tracker subscription has fired at least
                    //      one Rectangle event so we know where to put it.
                    //
                    // If either is missing, queue the request; the first
                    // Rectangle event drains it. See `pending_webviews`.
                    match (self.main_window_id, self.tab_body_bounds) {
                        (Some(window_id), Some(_)) => {
                            let wv_task = build_webview_task(entity, name, window_id, x, y, w, h);
                            return Task::batch([title_task, wv_task]);
                        }
                        _ => {
                            tracing::info!(
                                profile = %name,
                                "deferring webview creation until tab body bounds are known"
                            );
                            self.pending_webviews.push((entity, name));
                        }
                    }
                }

                return title_task;
            }
            Message::ActivateTab(entity) => {
                self.tabs.activate(entity);
                self.page = Page::Tab;
                self.persist_state();

                #[cfg(target_os = "linux")]
                {
                    let (x, y, w, h) = self.webview_bounds();
                    self.webview_store.show_only(entity);
                    self.webview_store.set_bounds(entity, x, y, w, h);
                }

                return self.update_title();
            }
            Message::CloseTab(entity) => {
                // Destroy the WebView before removing the tab so the X11
                // child window is torn down in an orderly fashion.
                #[cfg(target_os = "linux")]
                self.webview_store.destroy(entity);

                self.close_tab(entity);
                self.persist_state();

                // Show the newly-active tab's webview (if any).
                #[cfg(target_os = "linux")]
                if self.page == Page::Tab {
                    let active = self.tabs.active();
                    let (x, y, w, h) = self.webview_bounds();
                    self.webview_store.show_only(active);
                    self.webview_store.set_bounds(active, x, y, w, h);
                }

                return self.update_title();
            }
            Message::NewProfileNameChanged(value) => {
                self.new_profile_name = value;
            }
            Message::CreateProfile => {
                let name = std::mem::take(&mut self.new_profile_name);
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    return Task::none();
                }
                match self.geese_storage.create(trimmed) {
                    Ok(profile) => {
                        let profile_name = profile.name().to_owned();
                        self.refresh_known_profiles_inline();
                        self.open_tab(&profile_name);
                        self.picker_error = None;
                        self.persist_state();

                        let title_task = self.update_title();

                        #[cfg(target_os = "linux")]
                        {
                            let entity = self.tabs.active();
                            match (self.main_window_id, self.tab_body_bounds) {
                                (Some(window_id), Some(_)) => {
                                    let (x, y, w, h) = self.webview_bounds();
                                    let wv_task = build_webview_task(
                                        entity,
                                        profile_name,
                                        window_id,
                                        x,
                                        y,
                                        w,
                                        h,
                                    );
                                    return Task::batch([title_task, wv_task]);
                                }
                                _ => {
                                    tracing::info!(
                                        profile = %profile_name,
                                        "deferring webview creation until tab body bounds are known"
                                    );
                                    self.pending_webviews.push((entity, profile_name));
                                }
                            }
                        }

                        return title_task;
                    }
                    Err(error) => {
                        self.new_profile_name = trimmed.to_owned();
                        self.picker_error = Some(error.to_string());
                    }
                }
            }
            Message::RefreshProfiles => {
                self.refresh_known_profiles_inline();
            }
            Message::LaunchGoose(name) => {
                // Build the command via `geese::Profile::command` so the
                // resolved profile path is set as `GOOSE_PATH_ROOT` (this is
                // the same plumbing `geese launch` does internally — we just
                // skip the binary).
                //
                // Pre-alpha decision: stdin is closed (no terminal for goose
                // to read from) but stdout/stderr are inherited so any output
                // from goose surfaces in gander's terminal. Makes "the button
                // does nothing" actually debuggable. When we wire embedded
                // goose this whole branch goes away.
                tracing::info!(
                    profile = %name,
                    bin = %self.config.goose_bin,
                    "attempting to spawn goose"
                );
                let error = match self.geese_storage.get(&name) {
                    Ok(profile) => match profile
                        .command(&self.config.goose_bin)
                        .stdin(Stdio::null())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .spawn()
                    {
                        Ok(child) => {
                            tracing::info!(
                                profile = %name,
                                pid = child.id(),
                                path = %profile.path().display(),
                                "spawned goose"
                            );
                            None
                        }
                        Err(error) => Some(error.to_string()),
                    },
                    Err(err) => Some(err.to_string()),
                };
                if let Some(error) = &error {
                    tracing::error!(profile = %name, %error, "failed to launch goose");
                }
                if let Some((_, tab)) = self
                    .tab_data
                    .iter_mut()
                    .find(|(_, tab)| tab.profile == name)
                {
                    tab.last_launch_error = error;
                }
            }
            Message::UpdateConfig(config) => {
                self.config = config;
            }
            Message::LaunchUrl(url) => {
                if let Err(error) = open::that_detached(&url) {
                    tracing::warn!(%url, %error, "failed to open url");
                }
            }
            Message::ToggleAbout => {
                self.toggle_drawer(ContextDrawer::About);
            }
            Message::ToggleProfileConfig => {
                // Opening profile-config only makes sense when there's an
                // active profile tab — the header button is disabled in that
                // case, but guard here too in case a future keybind fires it.
                if self.active_profile_name().is_some() {
                    self.toggle_drawer(ContextDrawer::ProfileConfig);
                }
            }
            Message::CloseContextDrawer => {
                self.drawer = None;
                self.core.window.show_context = false;
            }

            // -------------------------------------------------------------------
            // Webview / window messages
            // -------------------------------------------------------------------
            Message::GotMainWindowId(id) => {
                self.main_window_id = Some(id);

                // Create webviews for any tabs that were restored from
                // persisted state before the window ID was known. As with
                // the OpenTab path we still need a real on-screen bounds
                // before we can build the WebView; if the tracker hasn't
                // fired yet we queue and let TabBodyRect drain it.
                #[cfg(target_os = "linux")]
                {
                    let tab_snapshots: Vec<(segmented_button::Entity, String)> = self
                        .tab_data
                        .iter()
                        .map(|(&e, tab)| (e, tab.profile.clone()))
                        .collect();

                    if tab_snapshots.is_empty() {
                        return Task::none();
                    }

                    if self.tab_body_bounds.is_none() {
                        tracing::info!(
                            count = tab_snapshots.len(),
                            "deferring restored-tab webviews until tab body bounds are known"
                        );
                        self.pending_webviews.extend(tab_snapshots);
                        return Task::none();
                    }

                    let (x, y, w, h) = self.webview_bounds();
                    let tasks: Vec<Task<cosmic::Action<Message>>> = tab_snapshots
                        .into_iter()
                        .map(|(entity, profile)| {
                            build_webview_task(entity, profile, id, x, y, w, h)
                        })
                        .collect();

                    // Each webview starts hidden; the active tab's webview is
                    // shown when its WebviewReady message arrives.
                    return Task::batch(tasks);
                }
            }

            Message::WebviewReady(entity) => {
                // Claim the WebView stored in the thread-local PENDING map by
                // the run_with_handle closure. No-op if build_as_child failed
                // (e.g., on Wayland).
                #[cfg(target_os = "linux")]
                {
                    self.webview_store.claim_pending(entity);

                    // If this is the currently active tab, show its webview
                    // and set initial bounds.
                    if self.tabs.active() == entity && self.page == Page::Tab {
                        let (x, y, w, h) = self.webview_bounds();
                        self.webview_store.show_only(entity);
                        self.webview_store.set_bounds(entity, x, y, w, h);
                    }
                }
            }

            Message::PumpGtk => {
                // Drive the GTK event loop on the main thread so the
                // WebKitGTK surface repaints and processes input. Called from
                // the 60 fps `time::every` subscription. Linux-only — on
                // other platforms this message is never sent.
                #[cfg(target_os = "linux")]
                while gtk::events_pending() {
                    gtk::main_iteration();
                }
            }

            Message::WindowResized(_id, size) => {
                self.window_size = (size.width, size.height);

                // Pre-position hidden tabs' webviews so they're roughly in
                // the right place if/when the user switches to them. The
                // active tab will be refined to pixel-perfect bounds by the
                // next rectangle_tracker draw event.
                #[cfg(target_os = "linux")]
                {
                    let (x, y, w, h) = self.webview_bounds();
                    self.webview_store.set_bounds_all(x, y, w, h);
                }
            }

            // libcosmic's rectangle_tracker subscription has two payload
            // shapes: `Init(tracker)` once (we keep the handle so view()
            // can wrap the active tab body in it), and `Rectangle((id, r))`
            // on every draw of the tracked widget (we forward `r` to the
            // active tab's WebView). Linux-only.
            #[cfg(target_os = "linux")]
            Message::TabBodyRect(update) => match update {
                RectangleUpdate::Init(tracker) => {
                    self.rect_tracker = Some(tracker);
                }
                RectangleUpdate::Rectangle((_, rect)) => {
                    self.tab_body_bounds = Some(rect);
                    if self.page == Page::Tab {
                        let entity = self.tabs.active();
                        // Go through `webview_bounds()` so the
                        // libcosmic-chrome offset is applied. Calling
                        // `set_bounds` with the raw iced rect would put
                        // the webview 40px too high, overlapping the
                        // tab strip.
                        let (x, y, w, h) = self.webview_bounds();
                        self.webview_store.set_bounds(entity, x, y, w, h);
                    }

                    // Drain any webview creations that were waiting on us.
                    // Their initial position is the only position they will
                    // ever have (wry 0.55 X11 move-is-a-no-op), so this has
                    // to happen after the rectangle is known.
                    if !self.pending_webviews.is_empty() {
                        if let Some(window_id) = self.main_window_id {
                            let pending: Vec<(segmented_button::Entity, String)> =
                                std::mem::take(&mut self.pending_webviews);
                            let (x, y, w, h) = self.webview_bounds();
                            tracing::info!(
                                count = pending.len(),
                                x,
                                y,
                                w,
                                h,
                                "draining pending webviews"
                            );
                            let tasks: Vec<Task<cosmic::Action<Message>>> = pending
                                .into_iter()
                                .map(|(entity, profile)| {
                                    build_webview_task(entity, profile, window_id, x, y, w, h)
                                })
                                .collect();
                            return Task::batch(tasks);
                        }
                    }
                }
            },
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let config_sub = self
            .core
            .watch_config::<Config>(Self::APP_ID)
            .map(|update| Message::UpdateConfig(update.config));

        // Window resize — keep `window_size` in sync so webview bounds stay
        // correct after the user drags the window edge.
        let resize_sub = cosmic::iced::window::resize_events()
            .map(|(id, size)| Message::WindowResized(id, size));

        // Pump the GTK event loop at ~60 fps so the WebKitGTK surface can
        // paint, process input, and run animations. Without this the webview
        // renders once on creation and then freezes. Linux-only.
        #[cfg(target_os = "linux")]
        let gtk_pump =
            cosmic::iced::time::every(Duration::from_millis(16)).map(|_| Message::PumpGtk);

        // rectangle_tracker_subscription only sends on changes (debounced
        // internally by libcosmic), so this is cheap to leave running for
        // the life of the application — it's how `update` learns about the
        // active tab body's bounds.
        #[cfg(target_os = "linux")]
        let rect_sub = rectangle_tracker_subscription(TAB_BODY_TRACKER_ID)
            .map(|(_, u)| Message::TabBodyRect(u));

        #[cfg(target_os = "linux")]
        return Subscription::batch([config_sub, resize_sub, gtk_pump, rect_sub]);

        #[cfg(not(target_os = "linux"))]
        Subscription::batch([config_sub, resize_sub])
    }

    fn on_nav_select(&mut self, _id: nav_bar::Id) -> Task<cosmic::Action<Self::Message>> {
        Task::none()
    }
}

// ---------------------------------------------------------------------------
// Webview task helper (Linux only)
// ---------------------------------------------------------------------------

/// Build a `Task` that calls `iced::window::run_with_handle` to create a wry
/// `WebView` for `profile` as a child of the iced window identified by
/// `window_id`.
///
/// The WebView is stored in the thread-local `webview::PENDING` map (since
/// `WebView` is `!Send` and cannot be returned from the Task directly).  The
/// task returns `Message::WebviewReady(entity)` which triggers
/// `webview_store.claim_pending(entity)` in `update()`.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn build_webview_task(
    entity: segmented_button::Entity,
    profile: String,
    window_id: cosmic::iced::window::Id,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Task<cosmic::Action<Message>> {
    let s = webview::display_scale();
    let initial_bounds = WryRect {
        position: LogicalPosition::new(x * s, y * s).into(),
        size: LogicalSize::new((width * s).max(1.0), (height * s).max(1.0)).into(),
    };
    cosmic::iced::window::run_with_handle(window_id, move |handle| {
        webview::create_child_webview(entity, &profile, &handle, initial_bounds);
        cosmic::Action::App(Message::WebviewReady(entity))
    })
}

// ---------------------------------------------------------------------------

impl AppModel {
    /// What page to land on when no tab is being actively shown.
    fn default_page(&self) -> Page {
        if self.tabs.iter().next().is_some() {
            Page::Tab
        } else {
            Page::Empty
        }
    }

    /// Best current estimate of the active tab body's on-screen bounds, in
    /// logical pixels, as `(x, y, width, height)`.
    ///
    /// Prefers the rectangle reported by libcosmic's rectangle_tracker
    /// (pixel-perfect, drawer/header/theme-aware); falls back to a coarse
    /// constant-offset rectangle derived from the window size when no
    /// tracker update has fired yet.
    ///
    /// The fallback only matters for the very first frame between a tab's
    /// creation and the next iced draw; once the tracker fires, the webview
    /// snaps to the true bounds.
    ///
    /// ## Coordinate spaces
    ///
    /// iced reports the tab body rectangle in *logical* pixels relative to
    /// the iced viewport. `wry::WebView::set_bounds` on X11 forwards those
    /// numbers to a gtk child window — see `webview::display_scale` for
    /// the device-pixel adjustment applied on the way to wry.
    ///
    /// `GANDER_WEBVIEW_X_OFFSET` / `GANDER_WEBVIEW_Y_OFFSET` env vars
    /// remain as a runtime escape hatch for builds where the rectangle
    /// returned by the tracker still doesn't line up.
    #[cfg(target_os = "linux")]
    fn webview_bounds(&self) -> (f64, f64, f64, f64) {
        let x_off: f64 = std::env::var("GANDER_WEBVIEW_X_OFFSET")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let y_off: f64 = std::env::var("GANDER_WEBVIEW_Y_OFFSET")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        if let Some(rect) = self.tab_body_bounds {
            (
                f64::from(rect.x) + x_off,
                f64::from(rect.y) + y_off,
                f64::from(rect.width).max(1.0),
                f64::from(rect.height).max(1.0),
            )
        } else {
            let (w, h) = self.window_size;
            let y = webview::TAB_STRIP_HEIGHT;
            (x_off, y + y_off, f64::from(w), (f64::from(h) - y).max(1.0))
        }
    }

    /// Name of the profile bound to the currently-active tab, if any.
    fn active_profile_name(&self) -> Option<&str> {
        self.tab_data
            .get(&self.tabs.active())
            .map(|tab| tab.profile.as_str())
    }

    /// Open `kind`, or close it if it's already open. Closing any drawer also
    /// hides the cosmic context-drawer surface; opening one shows it.
    fn toggle_drawer(&mut self, kind: ContextDrawer) {
        if self.drawer == Some(kind) {
            self.drawer = None;
            self.core.window.show_context = false;
        } else {
            self.drawer = Some(kind);
            self.core.window.show_context = true;
        }
    }

    /// Refresh the cached list of profiles from disk. Errors are logged and
    /// swallowed; the picker just shows the previous list.
    fn refresh_known_profiles_inline(&mut self) {
        match self.geese_storage.list() {
            Ok(list) => self.known_profiles = list,
            Err(error) => {
                tracing::error!(%error, "failed to list geese profiles");
            }
        }
    }

    /// Replay `state` onto an empty tab model. Profiles that have disappeared
    /// since last run are dropped.
    fn restore_state(&mut self, state: &State) {
        // Snapshot the known names into owned strings so we don't hold an
        // immutable borrow of `self.known_profiles` across the subsequent
        // mutable calls to `insert_tab` / `tabs.activate`.
        let known: std::collections::HashSet<String> =
            self.known_profiles.iter().map(|p| p.name.clone()).collect();

        let mut active_entity = None;
        for tab in &state.tabs {
            if !known.contains(&tab.name) {
                tracing::info!(name = %tab.name, "dropping stale tab on restore");
                continue;
            }
            let entity = self.insert_tab(&tab.name);
            if state.active.as_deref() == Some(tab.name.as_str()) {
                active_entity = Some(entity);
            }
        }
        if let Some(entity) = active_entity {
            self.tabs.activate(entity);
            self.page = Page::Tab;
        } else {
            // Activate the first tab if any, releasing the iterator borrow
            // before we touch `self.tabs` mutably.
            let first = self.tabs.iter().next();
            if let Some(first) = first {
                self.tabs.activate(first);
                self.page = Page::Tab;
            }
        }
    }

    /// Open `name` as a tab, activating an existing tab if one already shows
    /// that profile. Does not persist — callers do that.
    fn open_tab(&mut self, name: &str) {
        if let Some(entity) = self
            .tab_data
            .iter()
            .find(|(_, tab)| tab.profile == name)
            .map(|(entity, _)| *entity)
        {
            self.tabs.activate(entity);
        } else {
            let entity = self.insert_tab(name);
            self.tabs.activate(entity);
        }
        self.page = Page::Tab;
    }

    fn insert_tab(&mut self, name: &str) -> segmented_button::Entity {
        let entity = self.tabs.insert().text(name.to_owned()).closable().id();
        self.tab_data.insert(entity, Tab::new(name.to_owned()));
        entity
    }

    fn close_tab(&mut self, entity: segmented_button::Entity) {
        // If we're closing the active tab, activate its neighbour first to
        // avoid a flash of "empty" mid-frame.
        if self.tabs.is_active(entity) {
            if let Some(position) = self.tabs.position(entity) {
                let len = self.tabs.iter().count() as u16;
                let target = if position + 1 < len {
                    self.tabs.iter().nth((position + 1) as usize)
                } else if position > 0 {
                    self.tabs.iter().nth((position - 1) as usize)
                } else {
                    None
                };
                if let Some(target) = target {
                    self.tabs.activate(target);
                }
            }
        }
        self.tab_data.remove(&entity);
        self.tabs.remove(entity);
        self.page = self.default_page();
        // If the profile-config drawer was open for the now-gone active tab,
        // close it. About is profile-agnostic and stays open.
        if self.drawer == Some(ContextDrawer::ProfileConfig) && self.active_profile_name().is_none()
        {
            self.drawer = None;
            self.core.window.show_context = false;
        }
    }

    fn persist_state(&self) {
        let state = self.snapshot_state();
        if let Err(error) = self.state_storage.save(&state) {
            tracing::error!(%error, "failed to persist gander state");
        }
    }

    fn snapshot_state(&self) -> State {
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .filter_map(|entity| {
                self.tab_data.get(&entity).map(|tab| TabState {
                    name: tab.profile.clone(),
                })
            })
            .collect();
        let active = self
            .tab_data
            .get(&self.tabs.active())
            .map(|tab| tab.profile.clone());
        State {
            version: state::VERSION,
            active,
            tabs,
        }
    }

    fn update_title(&mut self) -> Task<cosmic::Action<Message>> {
        let mut title = fl!("app-title");
        if let Some(name) = self.active_profile_name() {
            title.push_str(" — ");
            title.push_str(name);
        }
        if self.core.main_window_id().is_some() {
            self.set_window_title(title)
        } else {
            Task::none()
        }
    }

    fn view_empty(&self) -> Element<'_, Message> {
        let space = cosmic::theme::spacing();
        widget::container(
            widget::column::with_children(vec![
                widget::text::title2(fl!("app-title")).into(),
                widget::text::body(fl!("placeholder-no-tab")).into(),
                widget::button::suggested(fl!("new-tab"))
                    .on_press(Message::ShowNewTabPicker)
                    .into(),
            ])
            .spacing(space.space_s)
            .align_x(Alignment::Center),
        )
        .padding(space.space_xl)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
    }

    fn view_picker(&self) -> Element<'_, Message> {
        let space = cosmic::theme::spacing();

        let already_open: std::collections::HashSet<String> = self
            .tab_data
            .values()
            .map(|tab| tab.profile.clone())
            .collect();

        let mut existing = widget::settings::section().title(fl!("new-tab-page-title"));

        if self.known_profiles.is_empty() {
            existing = existing.add(widget::text::body(fl!("new-tab-page-empty")));
        } else {
            for profile in &self.known_profiles {
                let is_open = already_open.contains(&profile.name);
                let mut button = widget::button::standard(if is_open {
                    format!("{} {}", profile.name, fl!("new-tab-page-already-open"))
                } else {
                    profile.name.clone()
                });
                if !is_open {
                    button = button.on_press(Message::OpenTab(profile.name.clone()));
                }

                let mut control_row = widget::row::with_capacity(2)
                    .spacing(space.space_xs)
                    .align_y(Alignment::Center);
                if profile.locked {
                    control_row =
                        control_row.push(widget::text::caption(fl!("new-tab-page-locked")));
                }
                control_row = control_row.push(button);

                existing = existing.add(
                    widget::settings::item::builder(profile.name.clone()).control(control_row),
                );
            }
        }

        let create_input = widget::text_input(
            fl!("new-tab-page-create-placeholder"),
            &self.new_profile_name,
        )
        .on_input(Message::NewProfileNameChanged);

        let create_button = widget::button::standard(fl!("new-tab-page-create-submit"))
            .on_press(Message::CreateProfile);

        let create_section = widget::settings::section()
            .title(fl!("new-tab-page-create-section"))
            .add(
                widget::row::with_capacity(2)
                    .push(create_input)
                    .push(create_button)
                    .spacing(space.space_xs)
                    .align_y(Alignment::Center),
            );

        let mut column = widget::column::with_capacity(4)
            .push(existing)
            .push(create_section);

        if let Some(error) = &self.picker_error {
            column = column.push(widget::text::body(error.clone()));
        }

        let actions = widget::row::with_capacity(3)
            .spacing(space.space_xs)
            .align_y(Alignment::Center)
            .push(widget::space::horizontal())
            .push(
                widget::button::standard(fl!("new-tab-page-refresh"))
                    .on_press(Message::RefreshProfiles),
            )
            .push(
                widget::button::standard(fl!("new-tab-page-close"))
                    .on_press(Message::HideNewTabPicker),
            );
        column = column.push(actions);

        widget::container(column.spacing(space.space_m))
            .padding(space.space_m)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// Build the profile-config context drawer for `profile_name`.
    ///
    /// Reads the profile from `geese_storage` on every render — we don't
    /// cache, so the drawer reflects current disk state. If the profile has
    /// vanished while the drawer was open, render a "no longer exists" note
    /// (the surrounding tab will also be showing its missing-placeholder).
    fn view_profile_config<'a>(
        &'a self,
        profile_name: &'a str,
    ) -> context_drawer::ContextDrawer<'a, Message> {
        let space = cosmic::theme::spacing();

        let body: Element<'_, Message> = match self.geese_storage.get(profile_name) {
            Err(_) => widget::text::body(fl!("tab-placeholder-missing")).into(),
            Ok(profile) => {
                let meta = profile.meta();

                let mut section = widget::settings::section();
                section = section.add(
                    widget::settings::item::builder(fl!("profile-config-path"))
                        .control(widget::text::body(profile.path().display().to_string())),
                );
                let status = if meta.locked {
                    fl!("profile-config-status-locked")
                } else {
                    fl!("profile-config-status-unlocked")
                };
                section = section.add(
                    widget::settings::item::builder(fl!("profile-config-status"))
                        .control(widget::text::body(status)),
                );
                if let Some(parent) = &meta.parent {
                    section = section.add(
                        widget::settings::item::builder(fl!("profile-config-parent"))
                            .control(widget::text::body(parent.clone())),
                    );
                }

                let launch = widget::button::suggested(fl!("profile-config-launch"))
                    .on_press(Message::LaunchGoose(profile_name.to_owned()));

                let mut column = widget::column::with_capacity(2)
                    .spacing(space.space_m)
                    .push(section)
                    .push(launch);

                if let Some(error) = self
                    .tab_data
                    .values()
                    .find(|tab| tab.profile == profile_name)
                    .and_then(|tab| tab.last_launch_error.as_ref())
                {
                    column = column.push(widget::text::body(fl!(
                        "profile-config-launch-failed",
                        error = error.as_str()
                    )));
                }

                column.into()
            }
        };

        context_drawer::context_drawer(body, Message::CloseContextDrawer)
            .title(profile_name.to_owned())
    }
}
