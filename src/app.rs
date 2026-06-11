// SPDX-License-Identifier: GPL-3.0-or-later

//! `gander`'s top-level COSMIC [`Application`](cosmic::Application).
//!
//! Holds the open-tab list (one [`Tab`] per profile name), wires the tab-bar
//! to a `segmented_button` model, and persists tab state to disk via
//! [`state::Storage`].
//!
//! The flow for a typical user gesture:
//!
//! 1. user clicks "+ tab" → [`Message::ShowNewTabPicker`] → `view()` swaps to
//!    the picker page
//! 2. user picks a profile → [`Message::OpenTab`] → tab is appended and
//!    activated, state is persisted
//! 3. user closes a tab → [`Message::CloseTab`] → tab is removed, neighbour
//!    activated, state is persisted
//!
//! Launching goose for a tab builds a `Command` via `geese::Profile::command`
//! (which sets `GOOSE_PATH_ROOT` for us) and spawns it detached. The tab
//! page reports a per-tab error on failure rather than popping a dialog.
//! See [`DESIGN.md`](../DESIGN.md) for the embedding plan that supersedes
//! this.

use std::{collections::HashMap, process::Stdio};

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
    /// User pressed "Launch goose" on a profile tab.
    LaunchGoose(String),
    /// `Config` watcher tick.
    UpdateConfig(Config),
    /// About-page link click.
    LaunchUrl(String),
    /// About context-drawer toggle.
    ToggleAbout,
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
        app.restore_state(&initial_state);

        let title_task = app.update_title();
        (app, title_task)
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
        vec![
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
        Some(context_drawer::about(
            &self.about,
            |url| Message::LaunchUrl(url.to_string()),
            Message::ToggleAbout,
        ))
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
                    Some(tab) => tab.view(&self.geese_storage),
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
                self.open_tab(&name);
                self.persist_state();
                return self.update_title();
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
                        self.open_tab(&profile_name);
                        self.picker_error = None;
                        self.persist_state();
                        return self.update_title();
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
                self.core.window.show_context = !self.core.window.show_context;
            }
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let config_sub = self
            .core
            .watch_config::<Config>(Self::APP_ID)
            .map(|update| Message::UpdateConfig(update.config));
        Subscription::batch([config_sub])
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
        let known: std::collections::HashSet<String> = self
            .known_profiles
            .iter()
            .map(|p| p.name.clone())
            .collect();

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
        if let Some(tab) = self.tab_data.get(&self.tabs.active()) {
            title.push_str(" — ");
            title.push_str(&tab.profile);
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
}
