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
//! - [`Page::Tab`] — the active tab's body
//!
//! Two context drawers (mutually exclusive — libcosmic only renders one at a
//! time) overlay the right edge:
//!
//! - [`ContextDrawer::About`] — about-the-app card
//! - [`ContextDrawer::ProfileConfig`] — per-active-tab profile metadata
//!
//! ## Flow for a typical user gesture
//!
//! 1. user clicks "+ New tab" → [`Message::ShowNewTabPicker`] → `view()`
//!    swaps to the picker page
//! 2. user picks a profile → [`Message::OpenTab`] → tab is appended and
//!    activated, an ACP connection to geesed is started, state is persisted,
//!    page returns to [`Page::Tab`]
//! 3. user types a prompt → IPC handler → [`AcpCommand::Prompt`] → ACP task
//! 4. tokens stream back → [`AcpEvent::AgentText`] → `evaluate_script`

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use cosmic::app::context_drawer;
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::keyboard::{Key, Modifiers, key::Named};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::prelude::*;
use cosmic::widget::{self, about::About, menu, nav_bar, segmented_button};
use geese_client::{GeesedClient, ProfileEntry};

use crate::acp::{AcpCommand, AcpConnection, AcpEvent, ConnectError};
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

/// Stable id we hand to libcosmic's [`rectangle_tracker_subscription`].
#[cfg(target_os = "linux")]
const TAB_BODY_TRACKER_ID: u8 = 0;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Shared inbox for ACP connections completed by background tasks.
///
/// `AcpConnection` is not `Clone` (it owns a `JoinHandle` and an `mpsc::Receiver`),
/// so we can't embed it directly in a `#[derive(Clone)]` message variant.
/// Instead, the task writes the completed connection here and sends only the
/// entity key + error status in the message.
#[cfg(target_os = "linux")]
type AcpInbox = Arc<Mutex<HashMap<segmented_button::Entity, AcpConnection>>>;

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

/// Flags constructed up-front in `main` and passed into the cosmic runtime.
#[derive(Clone, Debug)]
pub struct Flags {
    pub state_storage: state::Storage,
    pub initial_state: State,
}

impl Flags {
    pub fn load() -> anyhow::Result<Self> {
        let state_storage = state::Storage::from_env()?;
        let initial_state = state_storage.load()?;
        Ok(Self {
            state_storage,
            initial_state,
        })
    }
}

// ---------------------------------------------------------------------------
// AppModel
// ---------------------------------------------------------------------------

/// Top-level UI state.
pub struct AppModel {
    core: cosmic::Core,
    /// Geesed CRUD client. Populated after geesed starts.
    geesed: Option<GeesedClient>,
    state_storage: state::Storage,
    config: Config,
    #[allow(dead_code)]
    config_handler: Option<cosmic_config::Config>,

    tabs: segmented_button::SingleSelectModel,
    tab_data: HashMap<segmented_button::Entity, Tab>,

    /// Active ACP connections, one per open tab (Linux only; stubbed on other
    /// platforms). Keyed by the same entity as `tab_data`.
    #[cfg(target_os = "linux")]
    tab_sessions: HashMap<segmented_button::Entity, AcpConnection>,

    /// Inbox for ACP connections completed by `Task::perform` workers.
    /// Written by the task; drained in `update(TabAcpReady)`.
    #[cfg(target_os = "linux")]
    acp_inbox: AcpInbox,

    page: Page,
    drawer: Option<ContextDrawer>,

    /// Latest snapshot of profile metadata, refreshed on demand.
    known_profiles: Vec<ProfileEntry>,
    new_profile_name: String,
    picker_error: Option<String>,

    about: About,
    key_binds: HashMap<menu::KeyBind, MenuAction>,

    main_window_id: Option<cosmic::iced::window::Id>,
    window_size: (f32, f32),

    #[cfg(target_os = "linux")]
    webview_store: webview::WebviewStore,
    #[cfg(target_os = "linux")]
    rect_tracker: Option<RectangleTracker<u8>>,
    #[cfg(target_os = "linux")]
    tab_body_bounds: Option<IcedRectangle>,
    /// Tabs whose ACP connection is ready but webview creation is deferred
    /// until both `main_window_id` and `tab_body_bounds` are known.
    #[cfg(target_os = "linux")]
    pending_webviews: Vec<(segmented_button::Entity, String)>,
    /// Sender cloned into each tab's webview IPC handler so that
    /// Ctrl+PageUp/Down keypresses from inside the webview reach the app.
    /// The receiver is drained on every [`Message::PumpGtk`] tick.
    #[cfg(target_os = "linux")]
    webview_nav_tx: tokio::sync::mpsc::Sender<webview::TabNavDir>,
    #[cfg(target_os = "linux")]
    webview_nav_rx: tokio::sync::mpsc::Receiver<webview::TabNavDir>,
}

// ---------------------------------------------------------------------------
// Page / Drawer / Message types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Page {
    #[default]
    Empty,
    Tab,
    Picker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextDrawer {
    About,
    ProfileConfig,
}

#[derive(Clone, Debug)]
pub enum Message {
    ShowNewTabPicker,
    HideNewTabPicker,
    OpenTab(String),
    ActivateTab(segmented_button::Entity),
    CloseTab(segmented_button::Entity),
    NewProfileNameChanged(String),
    CreateProfile,
    RefreshProfiles,
    /// Profiles list returned by geesed.
    ProfilesRefreshed(Result<Vec<ProfileEntry>, String>),
    /// Profile creation result from geesed.
    ProfileCreated(Result<ProfileEntry, String>),
    /// ACP connection established for `entity` (or failed with an error
    /// message). On success, the `AcpConnection` is in `acp_inbox`.
    #[cfg(target_os = "linux")]
    TabAcpReady(segmented_button::Entity, Result<(), String>),
    /// `Config` watcher tick.
    UpdateConfig(Config),
    /// About-page link click.
    LaunchUrl(String),
    ToggleAbout,
    ToggleProfileConfig,
    CloseContextDrawer,

    GotMainWindowId(cosmic::iced::window::Id),
    WebviewReady(segmented_button::Entity),
    PumpGtk,
    WindowResized(cosmic::iced::window::Id, cosmic::iced::Size),

    /// Keyboard shortcut: Ctrl+PageUp — cycle to the previous tab.
    PrevTab,
    /// Keyboard shortcut: Ctrl+PageDown — cycle to the next tab.
    NextTab,

    #[cfg(target_os = "linux")]
    TabBodyRect(RectangleUpdate<u8>),

    /// geesed client ready (or failed to start).
    GeesedReady(Result<Box<GeesedClient>, String>),
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

// ---------------------------------------------------------------------------
// cosmic::Application impl
// ---------------------------------------------------------------------------

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
            state_storage,
            initial_state,
        } = flags;

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

        #[cfg(target_os = "linux")]
        let (webview_nav_tx, webview_nav_rx) = tokio::sync::mpsc::channel::<webview::TabNavDir>(16);

        let about = About::default()
            .name(fl!("app-title"))
            .version(env!("CARGO_PKG_VERSION"))
            .links([(fl!("about"), REPOSITORY)])
            .license(env!("CARGO_PKG_LICENSE"));

        let mut app = AppModel {
            core,
            geesed: None,
            state_storage,
            config,
            config_handler,
            tabs: segmented_button::ModelBuilder::default().build(),
            tab_data: HashMap::new(),
            #[cfg(target_os = "linux")]
            tab_sessions: HashMap::new(),
            #[cfg(target_os = "linux")]
            acp_inbox: Arc::new(Mutex::new(HashMap::new())),
            page: Page::Empty,
            drawer: None,
            known_profiles: Vec::new(),
            new_profile_name: String::new(),
            picker_error: None,
            about,
            key_binds: HashMap::new(),
            main_window_id: None,
            window_size: (640.0, 400.0),
            #[cfg(target_os = "linux")]
            webview_store: webview::WebviewStore::new(),
            #[cfg(target_os = "linux")]
            rect_tracker: None,
            #[cfg(target_os = "linux")]
            tab_body_bounds: None,
            #[cfg(target_os = "linux")]
            pending_webviews: Vec::new(),
            #[cfg(target_os = "linux")]
            webview_nav_tx,
            #[cfg(target_os = "linux")]
            webview_nav_rx,
        };

        // Restore tabs from persisted state. Profiles whose names are in
        // `initial_state` will be opened once geesed confirms they exist.
        // For now, unconditionally restore — `ProfilesRefreshed` will drop
        // stale ones on first refresh.
        app.restore_tabs_unconditional(&initial_state);

        let title_task = app.update_title();

        let get_window_id = cosmic::iced::window::oldest().map(|opt_id| {
            cosmic::Action::App(match opt_id {
                Some(id) => Message::GotMainWindowId(id),
                None => {
                    tracing::warn!(
                        "oldest() returned None during init; \
                         falling back to Id::RESERVED for webview creation"
                    );
                    Message::GotMainWindowId(cosmic::iced::window::Id::RESERVED)
                }
            })
        });

        // Autospawn geesed (or connect to the already-running daemon).
        let geesed_task = Task::perform(
            async {
                match geese_client::ensure_running().await {
                    Ok(client) => Ok(Box::new(client)),
                    Err(err) => Err(err.to_string()),
                }
            },
            Message::GeesedReady,
        )
        .map(cosmic::Action::App);

        (app, Task::batch([title_task, get_window_id, geesed_task]))
    }

    // -----------------------------------------------------------------------
    // Header
    // -----------------------------------------------------------------------

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
                    Some(tab) => tab.view(&self.known_profiles),
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

    // -----------------------------------------------------------------------
    // update
    // -----------------------------------------------------------------------

    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::GeesedReady(result) => match result {
                Ok(client) => {
                    tracing::info!("geesed client ready");
                    self.geesed = Some(*client);
                    // Kick off an initial profile list so the picker and
                    // restore_state can see current profiles.
                    return self.task_refresh_profiles();
                }
                Err(error) => {
                    tracing::error!(%error, "geesed failed to start");
                    self.picker_error = Some(format!("geesed not available: {error}"));
                }
            },

            Message::ProfilesRefreshed(result) => match result {
                Ok(profiles) => {
                    tracing::debug!(count = profiles.len(), "profiles refreshed");
                    self.known_profiles = profiles;
                    // Drop any restored tabs whose profiles no longer exist.
                    self.prune_stale_tabs();
                }
                Err(error) => {
                    tracing::error!(%error, "failed to refresh profiles");
                }
            },

            Message::ProfileCreated(result) => match result {
                Ok(entry) => {
                    let profile_name = entry.name.clone();
                    // Add to cached list so the picker shows it immediately.
                    if !self.known_profiles.iter().any(|p| p.name == profile_name) {
                        self.known_profiles.push(entry);
                    }
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
                                let wv_task = self.build_webview_task(
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
                    self.picker_error = Some(error);
                }
            },

            Message::ShowNewTabPicker => {
                self.picker_error = None;
                self.page = Page::Picker;
                #[cfg(target_os = "linux")]
                self.webview_store.hide_all();
                // Refresh profiles so the picker shows current state.
                return self.task_refresh_profiles();
            }

            Message::HideNewTabPicker => {
                self.page = self.default_page();
                #[cfg(target_os = "linux")]
                if self.page == Page::Tab {
                    let entity = self.tabs.active();
                    let (x, y, w, h) = self.webview_bounds();
                    self.webview_store.show_only(entity);
                    self.webview_store.set_bounds(entity, x, y, w, h);
                }
            }

            Message::OpenTab(name) => {
                let already_open = self.tab_data.values().any(|tab| tab.profile == name);

                self.open_tab(&name);
                self.persist_state();

                let title_task = self.update_title();

                #[cfg(target_os = "linux")]
                {
                    let entity = self.tabs.active();
                    let (x, y, w, h) = self.webview_bounds();

                    if already_open {
                        self.webview_store.show_only(entity);
                        self.webview_store.set_bounds(entity, x, y, w, h);
                        return title_task;
                    }

                    match (self.main_window_id, self.tab_body_bounds) {
                        (Some(window_id), Some(_)) => {
                            let wv_task =
                                self.build_webview_task(entity, name, window_id, x, y, w, h);
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
                #[cfg(target_os = "linux")]
                {
                    self.webview_store.destroy(entity);
                    // Drop the ACP connection — this closes the socket and
                    // geesed stops the goose process for this profile.
                    self.tab_sessions.remove(&entity);
                }

                self.close_tab(entity);
                self.persist_state();

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
                let trimmed = name.trim().to_owned();
                if trimmed.is_empty() {
                    return Task::none();
                }
                let Some(client) = self.geesed.clone() else {
                    self.new_profile_name = trimmed;
                    self.picker_error = Some("geesed not available".into());
                    return Task::none();
                };
                return Task::perform(
                    async move {
                        let mut c = client;
                        c.create_profile(&trimmed).await.map_err(|e| e.to_string())
                    },
                    Message::ProfileCreated,
                )
                .map(cosmic::Action::App);
            }

            Message::RefreshProfiles => {
                return self.task_refresh_profiles();
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
                if self.active_profile_name().is_some() {
                    self.toggle_drawer(ContextDrawer::ProfileConfig);
                }
            }

            Message::CloseContextDrawer => {
                self.drawer = None;
                self.core.window.show_context = false;
            }

            // ---------------------------------------------------------------
            // ACP
            // ---------------------------------------------------------------
            #[cfg(target_os = "linux")]
            Message::TabAcpReady(entity, result) => match result {
                Ok(()) => {
                    tracing::info!(?entity, "ACP connection ready");
                    if let Some(acp) = self.acp_inbox.lock().unwrap().remove(&entity) {
                        self.tab_sessions.insert(entity, acp);
                    }
                }
                Err(error) => {
                    tracing::error!(?entity, %error, "ACP connection failed");
                    if let Some(tab) = self.tab_data.get_mut(&entity) {
                        tab.acp_error = Some(error);
                    }
                }
            },

            // ---------------------------------------------------------------
            // Webview / window
            // ---------------------------------------------------------------
            Message::GotMainWindowId(id) => {
                self.main_window_id = Some(id);

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
                            self.build_webview_task(entity, profile, id, x, y, w, h)
                        })
                        .collect();

                    return Task::batch(tasks);
                }
            }

            Message::WebviewReady(entity) => {
                #[cfg(target_os = "linux")]
                {
                    self.webview_store.claim_pending(entity);

                    if self.tabs.active() == entity && self.page == Page::Tab {
                        let (x, y, w, h) = self.webview_bounds();
                        self.webview_store.show_only(entity);
                        self.webview_store.set_bounds(entity, x, y, w, h);
                    }
                }
            }

            Message::PumpGtk => {
                #[cfg(target_os = "linux")]
                {
                    while gtk::events_pending() {
                        gtk::main_iteration();
                    }
                    // Drain ACP events for each tab and push them to the
                    // corresponding WebView via evaluate_script.
                    for (entity, acp) in &mut self.tab_sessions {
                        while let Ok(event) = acp.recv.try_recv() {
                            let js = acp_event_to_js(&event);
                            self.webview_store.evaluate_script(*entity, &js);
                        }
                    }
                    // Forward tab-navigation keypresses that originated inside
                    // a webview (Ctrl+PageUp / Ctrl+PageDown posted via IPC).
                    if let Ok(dir) = self.webview_nav_rx.try_recv() {
                        let msg = match dir {
                            webview::TabNavDir::Prev => Message::PrevTab,
                            webview::TabNavDir::Next => Message::NextTab,
                        };
                        return self.update(msg);
                    }
                }
            }

            Message::PrevTab => {
                if let Some(entity) = self.adjacent_tab(-1) {
                    return self.update(Message::ActivateTab(entity));
                }
            }
            Message::NextTab => {
                if let Some(entity) = self.adjacent_tab(1) {
                    return self.update(Message::ActivateTab(entity));
                }
            }

            Message::WindowResized(_id, size) => {
                self.window_size = (size.width, size.height);

                #[cfg(target_os = "linux")]
                {
                    let (x, y, w, h) = self.webview_bounds();
                    self.webview_store.set_bounds_all(x, y, w, h);
                }
            }

            #[cfg(target_os = "linux")]
            Message::TabBodyRect(update) => match update {
                RectangleUpdate::Init(tracker) => {
                    self.rect_tracker = Some(tracker);
                }
                RectangleUpdate::Rectangle((_, rect)) => {
                    self.tab_body_bounds = Some(rect);
                    if self.page == Page::Tab {
                        let entity = self.tabs.active();
                        let (x, y, w, h) = self.webview_bounds();
                        self.webview_store.set_bounds(entity, x, y, w, h);
                    }

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
                                    self.build_webview_task(entity, profile, window_id, x, y, w, h)
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

    // -----------------------------------------------------------------------
    // subscription
    // -----------------------------------------------------------------------

    fn subscription(&self) -> Subscription<Self::Message> {
        let config_sub = self
            .core
            .watch_config::<Config>(Self::APP_ID)
            .map(|update| Message::UpdateConfig(update.config));

        let resize_sub = cosmic::iced::window::resize_events()
            .map(|(id, size)| Message::WindowResized(id, size));

        #[cfg(target_os = "linux")]
        let gtk_pump =
            cosmic::iced::time::every(Duration::from_millis(16)).map(|_| Message::PumpGtk);

        #[cfg(target_os = "linux")]
        let rect_sub = rectangle_tracker_subscription(TAB_BODY_TRACKER_ID)
            .map(|(_, u)| Message::TabBodyRect(u));

        // Keyboard shortcut: Ctrl+PageUp / Ctrl+PageDown to cycle tabs.
        // This subscription fires when iced/COSMIC header widgets hold keyboard
        // focus.  When a webview has focus, WebKitGTK captures the keys before
        // iced sees them; the keydown listener injected via BRIDGE_SCRIPT posts
        // them back as IPC messages instead (drained in PumpGtk).
        let key_sub = cosmic::iced::keyboard::listen().filter_map(|event| {
            use cosmic::iced::keyboard::Event as KbEvent;
            match event {
                KbEvent::KeyPressed { key, modifiers, .. } => {
                    if !modifiers.contains(Modifiers::CTRL) {
                        return None;
                    }
                    match key {
                        Key::Named(Named::PageDown) => Some(Message::NextTab),
                        Key::Named(Named::PageUp) => Some(Message::PrevTab),
                        _ => None,
                    }
                }
                _ => None,
            }
        });

        #[cfg(target_os = "linux")]
        return Subscription::batch([config_sub, resize_sub, gtk_pump, rect_sub, key_sub]);

        #[cfg(not(target_os = "linux"))]
        Subscription::batch([config_sub, resize_sub, key_sub])
    }

    fn on_nav_select(&mut self, _id: nav_bar::Id) -> Task<cosmic::Action<Self::Message>> {
        Task::none()
    }
}

// ---------------------------------------------------------------------------
// Webview task helper (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn build_webview_task_inner(
    entity: segmented_button::Entity,
    profile: String,
    window_id: cosmic::iced::window::Id,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    cmd_tx: tokio::sync::mpsc::Sender<AcpCommand>,
    nav_tx: tokio::sync::mpsc::Sender<webview::TabNavDir>,
) -> Task<cosmic::Action<Message>> {
    let s = webview::display_scale();
    let initial_bounds = WryRect {
        position: LogicalPosition::new(x * s, y * s).into(),
        size: LogicalSize::new((width * s).max(1.0), (height * s).max(1.0)).into(),
    };
    cosmic::iced::window::run_with_handle(window_id, move |handle| {
        webview::create_child_webview(entity, &profile, &handle, initial_bounds, cmd_tx, nav_tx);
        cosmic::Action::App(Message::WebviewReady(entity))
    })
}

// ---------------------------------------------------------------------------
// ACP event → JS
// ---------------------------------------------------------------------------

/// Serialize a string as a JSON string literal, falling back to `""` on error.
#[cfg(target_os = "linux")]
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

#[cfg(target_os = "linux")]
fn acp_event_to_js(event: &AcpEvent) -> String {
    match event {
        AcpEvent::AgentText(text) => {
            format!(
                "window.gander._publish({{type:'agent_text',content:{}}})",
                json_str(text)
            )
        }
        AcpEvent::UserText(text) => {
            format!(
                "window.gander._publish({{type:'user_text',content:{}}})",
                json_str(text)
            )
        }
        AcpEvent::ToolUse { name, input } => {
            format!(
                "window.gander._publish({{type:'tool_use',name:{},input:{}}})",
                json_str(name),
                json_str(input)
            )
        }
        AcpEvent::ToolResult { name, output } => {
            format!(
                "window.gander._publish({{type:'tool_result',name:{},output:{}}})",
                json_str(name),
                json_str(output)
            )
        }
        AcpEvent::SessionLoadStart => {
            "window.gander._publish({type:'session_load_start'})".to_owned()
        }
        AcpEvent::SessionLoadEnd => "window.gander._publish({type:'session_load_end'})".to_owned(),
        AcpEvent::Complete(_) => "window.gander._publish({type:'done'})".to_owned(),
        AcpEvent::Error(msg) => {
            format!(
                "window.gander._publish({{type:'error',message:{}}})",
                json_str(msg)
            )
        }
        AcpEvent::SessionList(sessions) => {
            let sessions_json = serde_json::to_string(sessions).unwrap_or_else(|_| "[]".into());
            format!(
                "window.gander._publish({{type:'session_list',sessions:{}}})",
                sessions_json
            )
        }
        AcpEvent::SessionActive(id) => {
            format!(
                "window.gander._publish({{type:'session_active',id:{},history:[]}})",
                json_str(id)
            )
        }
        AcpEvent::SessionInfo {
            cwd,
            model,
            tool_count,
        } => {
            let tool_count_js = match tool_count {
                Some(n) => n.to_string(),
                None => "null".to_string(),
            };
            format!(
                "window.gander._publish({{type:'session_info',cwd:{},model:{},tool_count:{}}})",
                json_str(cwd),
                json_str(model),
                tool_count_js,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// AppModel helpers
// ---------------------------------------------------------------------------

impl AppModel {
    /// Build and return a Task that kicks off both the webview creation and the
    /// ACP connection for `entity`/`profile`.
    #[cfg(target_os = "linux")]
    #[allow(clippy::too_many_arguments)]
    fn build_webview_task(
        &self,
        entity: segmented_button::Entity,
        profile: String,
        window_id: cosmic::iced::window::Id,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> Task<cosmic::Action<Message>> {
        // Create the command channel upfront so the IPC handler (installed at
        // webview-creation time) and the ACP task both get the right ends.
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<AcpCommand>(64);
        let nav_tx = self.webview_nav_tx.clone();

        let wv_task = build_webview_task_inner(
            entity,
            profile.clone(),
            window_id,
            x,
            y,
            w,
            h,
            cmd_tx,
            nav_tx,
        );

        let inbox = Arc::clone(&self.acp_inbox);
        let acp_task = Task::perform(
            async move {
                match AcpConnection::connect_with_rx(&profile, cmd_rx).await {
                    Ok(conn) => {
                        inbox.lock().unwrap().insert(entity, conn);
                        Ok(())
                    }
                    Err(e) => Err(format_acp_error(&e)),
                }
            },
            move |result| Message::TabAcpReady(entity, result),
        )
        .map(cosmic::Action::App);

        Task::batch([wv_task, acp_task])
    }

    fn default_page(&self) -> Page {
        if self.tabs.iter().next().is_some() {
            Page::Tab
        } else {
            Page::Empty
        }
    }

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

    fn active_profile_name(&self) -> Option<&str> {
        self.tab_data
            .get(&self.tabs.active())
            .map(|tab| tab.profile.as_str())
    }

    /// Return the entity `offset` positions away from the current active tab,
    /// wrapping around. Returns `None` when there are fewer than two tabs.
    fn adjacent_tab(&self, offset: i32) -> Option<segmented_button::Entity> {
        let entities: Vec<segmented_button::Entity> = self.tabs.iter().collect();
        let len = entities.len();
        if len < 2 {
            return None;
        }
        let current = self.tabs.active();
        let pos = entities.iter().position(|&e| e == current)?;
        let next = ((pos as i32 + offset).rem_euclid(len as i32)) as usize;
        Some(entities[next])
    }

    fn toggle_drawer(&mut self, kind: ContextDrawer) {
        if self.drawer == Some(kind) {
            self.drawer = None;
            self.core.window.show_context = false;
        } else {
            self.drawer = Some(kind);
            self.core.window.show_context = true;
        }
    }

    /// Fire a Task::perform to list profiles via geesed.
    fn task_refresh_profiles(&self) -> Task<cosmic::Action<Message>> {
        let Some(client) = self.geesed.clone() else {
            return Task::none();
        };
        Task::perform(
            async move {
                let mut c = client;
                c.list_profiles().await.map_err(|e| e.to_string())
            },
            Message::ProfilesRefreshed,
        )
        .map(cosmic::Action::App)
    }

    /// Restore open tabs from `state` without validating against geesed.
    /// Stale entries are pruned later once the first `ProfilesRefreshed`
    /// arrives.
    fn restore_tabs_unconditional(&mut self, state: &State) {
        let mut active_entity = None;
        for tab in &state.tabs {
            let entity = self.insert_tab(&tab.name);
            if state.active.as_deref() == Some(tab.name.as_str()) {
                active_entity = Some(entity);
            }
        }
        if let Some(entity) = active_entity {
            self.tabs.activate(entity);
            self.page = Page::Tab;
        } else {
            let first = self.tabs.iter().next();
            if let Some(first) = first {
                self.tabs.activate(first);
                self.page = Page::Tab;
            }
        }
    }

    /// Remove any tabs whose profiles are no longer in `known_profiles`.
    fn prune_stale_tabs(&mut self) {
        let known: std::collections::HashSet<&str> = self
            .known_profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect();

        let stale: Vec<segmented_button::Entity> = self
            .tab_data
            .iter()
            .filter(|(_, tab)| !known.contains(tab.profile.as_str()))
            .map(|(&e, _)| e)
            .collect();

        for entity in stale {
            tracing::info!(
                name = %self.tab_data[&entity].profile,
                "dropping stale tab after profile refresh"
            );
            #[cfg(target_os = "linux")]
            {
                self.webview_store.destroy(entity);
                self.tab_sessions.remove(&entity);
            }
            self.close_tab(entity);
        }
    }

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
    fn view_profile_config<'a>(
        &'a self,
        profile_name: &'a str,
    ) -> context_drawer::ContextDrawer<'a, Message> {
        let body: Element<'_, Message> =
            match self.known_profiles.iter().find(|p| p.name == profile_name) {
                None => widget::text::body(fl!("tab-placeholder-missing")).into(),
                Some(profile) => {
                    let mut section = widget::settings::section();
                    section = section.add(
                        widget::settings::item::builder(fl!("profile-config-path"))
                            .control(widget::text::body(profile.path.clone())),
                    );
                    let status = if profile.locked {
                        fl!("profile-config-status-locked")
                    } else {
                        fl!("profile-config-status-unlocked")
                    };
                    section = section.add(
                        widget::settings::item::builder(fl!("profile-config-status"))
                            .control(widget::text::body(status)),
                    );
                    if let Some(parent) = &profile.parent {
                        section = section.add(
                            widget::settings::item::builder(fl!("profile-config-parent"))
                                .control(widget::text::body(parent.clone())),
                        );
                    }

                    section.into()
                }
            };

        context_drawer::context_drawer(body, Message::CloseContextDrawer)
            .title(profile_name.to_owned())
    }
}

// ---------------------------------------------------------------------------
// ACP error formatting
// ---------------------------------------------------------------------------

fn format_acp_error(error: &ConnectError) -> String {
    match error {
        ConnectError::ProfileNotFound => "Profile not found".into(),
        ConnectError::ProfileInUse => "Profile already in use by another client".into(),
        ConnectError::GooseBinaryUnavailable(msg) => {
            format!("Goose binary not found — set GEESE_GOOSE_BIN: {msg}")
        }
        ConnectError::SpawnFailed(msg) => format!("Goose failed to spawn: {msg}"),
        ConnectError::SocketConnect(e) => format!("Could not connect to geesed: {e}"),
        ConnectError::Protocol(msg) => format!("ACP protocol error: {msg}"),
    }
}
