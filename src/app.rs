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

use std::{collections::HashMap, process::Stdio};

use cosmic::app::context_drawer;
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::{Alignment, Length, Subscription, Task as IcedTask, time};
use cosmic::prelude::*;
use cosmic::widget::{self, about::About, menu, nav_bar, segmented_button};
use geese::{ProfileMeta, Storage};
use std::time::Duration;

use crate::config::Config;
use crate::fl;
use crate::state::{self, State, TabState};
use crate::tab::Tab;

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
    /// Route a webview action to a specific tab.
    TabWebView(segmented_button::Entity, iced_webview::Action),
    /// One tab's webview finished creating its first view.
    TabWebViewCreated(segmented_button::Entity),
    /// Periodic render tick for embedded tab webviews.
    TickWebviews,
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
        };

        // Replay persisted tabs against the *current* set of profiles —
        // entries whose profiles have vanished since last run are silently
        // dropped, matching the contract in DESIGN.md.
        app.refresh_known_profiles_inline();
        let restore_task = app.restore_state(&initial_state);

        let title_task = app.update_title();
        (app, Task::batch([restore_task, title_task]))
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

        let body: Element<_> = match self.page {
            Page::Empty => self.view_empty(),
            Page::Picker => self.view_picker(),
            Page::Tab => {
                let entity = self.tabs.active();
                match self.tab_data.get(&entity) {
                    Some(tab) => tab.view(entity, &self.geese_storage),
                    None => self.view_empty(),
                }
            }
        };

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
            }
            Message::HideNewTabPicker => {
                self.page = self.default_page();
            }
            Message::OpenTab(name) => {
                let open_task = self.open_tab(&name);
                self.persist_state();
                return Task::batch([open_task, self.update_title()]);
            }
            Message::ActivateTab(entity) => {
                self.tabs.activate(entity);
                self.page = Page::Tab;
                self.persist_state();
                return self.update_title();
            }
            Message::CloseTab(entity) => {
                self.close_tab(entity);
                self.persist_state();
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
                        let open_task = self.open_tab(&profile_name);
                        self.picker_error = None;
                        self.persist_state();
                        return Task::batch([open_task, self.update_title()]);
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
            Message::TabWebView(entity, action) => {
                return self.update_tab_webview(entity, action);
            }
            Message::TabWebViewCreated(entity) => {
                if let Some(tab) = self.tab_data.get_mut(&entity) {
                    return Self::into_app_task(tab.finish_webview_creation());
                }
            }
            Message::TickWebviews => {
                return Task::batch(
                    self.tab_data
                        .values_mut()
                        .map(|tab| Self::into_app_task(tab.tick_webview()))
                        .collect::<Vec<_>>(),
                );
            }
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let config_sub = self
            .core
            .watch_config::<Config>(Self::APP_ID)
            .map(|update| Message::UpdateConfig(update.config));
        let webview_sub = if self.tab_data.is_empty() {
            Subscription::none()
        } else {
            time::every(Duration::from_millis(16)).map(|_| Message::TickWebviews)
        };
        Subscription::batch([config_sub, webview_sub])
    }

    fn on_nav_select(&mut self, _id: nav_bar::Id) -> Task<cosmic::Action<Self::Message>> {
        Task::none()
    }
}

impl AppModel {
    /// What page to land on when no tab is being actively shown.
    fn default_page(&self) -> Page {
        if self.tabs.iter().next().is_some() {
            Page::Tab
        } else {
            Page::Empty
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
    fn restore_state(&mut self, state: &State) -> Task<cosmic::Action<Message>> {
        // Snapshot the known names into owned strings so we don't hold an
        // immutable borrow of `self.known_profiles` across the subsequent
        // mutable calls to `insert_tab` / `tabs.activate`.
        let known: std::collections::HashSet<String> = self
            .known_profiles
            .iter()
            .map(|p| p.name.clone())
            .collect();

        let mut active_entity = None;
        let mut tasks = Vec::new();
        for tab in &state.tabs {
            if !known.contains(&tab.name) {
                tracing::info!(name = %tab.name, "dropping stale tab on restore");
                continue;
            }
            let (entity, task) = self.insert_tab(&tab.name);
            tasks.push(task);
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
        Task::batch(tasks)
    }

    /// Open `name` as a tab, activating an existing tab if one already shows
    /// that profile. Does not persist — callers do that.
    fn open_tab(&mut self, name: &str) -> Task<cosmic::Action<Message>> {
        if let Some(entity) = self
            .tab_data
            .iter()
            .find(|(_, tab)| tab.profile == name)
            .map(|(entity, _)| *entity)
        {
            self.tabs.activate(entity);
            self.page = Page::Tab;
            Task::none()
        } else {
            let (entity, task) = self.insert_tab(name);
            self.tabs.activate(entity);
            self.page = Page::Tab;
            task
        }
    }

    fn insert_tab(&mut self, name: &str) -> (segmented_button::Entity, Task<cosmic::Action<Message>>) {
        let entity = self.tabs.insert().text(name.to_owned()).closable().id();
        let mut tab = Tab::new(name.to_owned(), entity);
        let task = Self::into_app_task(tab.create_webview());
        self.tab_data.insert(entity, tab);
        (entity, task)
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
        if let Some(mut tab) = self.tab_data.remove(&entity) {
            tab.destroy();
        }
        self.tabs.remove(entity);
        self.page = self.default_page();
        // If the profile-config drawer was open for the now-gone active tab,
        // close it. About is profile-agnostic and stays open.
        if self.drawer == Some(ContextDrawer::ProfileConfig)
            && self.active_profile_name().is_none()
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

    fn into_app_task(task: IcedTask<Message>) -> Task<cosmic::Action<Message>> {
        task.map(cosmic::Action::App)
    }

    fn update_tab_webview(
        &mut self,
        entity: segmented_button::Entity,
        action: iced_webview::Action,
    ) -> Task<cosmic::Action<Message>> {
        match self.tab_data.get_mut(&entity) {
            Some(tab) => Self::into_app_task(tab.update_webview(action)),
            None => Task::none(),
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

        let create_input =
            widget::text_input(fl!("new-tab-page-create-placeholder"), &self.new_profile_name)
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
