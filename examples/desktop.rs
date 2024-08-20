// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic::app::{message, Command, Core, Settings};
use cosmic::{
    cosmic_config::{self, ConfigSet, CosmicConfigEntry},
    executor,
    iced::{
        self, alignment,
        event::{
            self,
            wayland::{Event as WaylandEvent, LayerEvent, OutputEvent},
            Event,
        },
        futures::{self, SinkExt},
        keyboard::{Event as KeyEvent, Modifiers},
        subscription,
        wayland::{
            actions::layer_surface::{IcedMargin, IcedOutput, SctkLayerSurfaceSettings},
            layer_surface::{
                destroy_layer_surface, get_layer_surface, Anchor, KeyboardInteractivity, Layer,
            },
        },
        widget::scrollable,
        Background, Border, Color, Length, Subscription,
    },
    iced_runtime::core::window::Id as SurfaceId,
    style, theme,
    widget::{
        self,
        menu::{Action as MenuAction, KeyBind},
    },
    Element,
};
use cosmic_files::{
    app::{self, Action},
    config::TabConfig,
    tab::{self, ItemMetadata, Location, Tab},
};
use notify_debouncer_full::{
    new_debouncer,
    notify::{self, RecommendedWatcher, Watcher},
    DebouncedEvent, Debouncer, FileIdMap,
};
use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::time;
use wayland_client::{protocol::wl_output::WlOutput, Proxy};

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let settings = Settings::default().no_main_window(true).transparent(true);

    cosmic::app::run::<App>(settings, ())?;

    Ok(())
}

/// Messages that are used specifically by our [`App`].
#[derive(Clone, Debug)]
pub enum Message {
    LayerEvent(LayerEvent, SurfaceId),
    OutputEvent(OutputEvent, WlOutput),
    Modifiers(Modifiers),
    NotifyEvents(Vec<DebouncedEvent>),
    NotifyWatcher(WatcherWrapper),
    TabMessage(tab::Message),
    TabRescan(Vec<tab::Item>),
}

struct WatcherWrapper {
    watcher_opt: Option<Debouncer<RecommendedWatcher, FileIdMap>>,
}

impl Clone for WatcherWrapper {
    fn clone(&self) -> Self {
        Self { watcher_opt: None }
    }
}

impl fmt::Debug for WatcherWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatcherWrapper").finish()
    }
}

impl PartialEq for WatcherWrapper {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

/// The [`App`] stores application-specific state.
pub struct App {
    core: Core,
    key_binds: HashMap<KeyBind, Action>,
    modifiers: Modifiers,
    surface_ids: HashMap<WlOutput, SurfaceId>,
    surface_names: HashMap<SurfaceId, String>,
    tab: Tab,
    watcher_opt: Option<(Debouncer<RecommendedWatcher, FileIdMap>, HashSet<PathBuf>)>,
}

impl App {
    fn rescan_tab(&self) -> Command<Message> {
        let location = self.tab.location.clone();
        let icon_sizes = self.tab.config.icon_sizes;
        Command::perform(
            async move {
                match tokio::task::spawn_blocking(move || location.scan(icon_sizes)).await {
                    Ok(items) => message::app(Message::TabRescan(items)),
                    Err(err) => {
                        log::warn!("failed to rescan: {}", err);
                        message::none()
                    }
                }
            },
            |x| x,
        )
    }

    fn update_watcher(&mut self) -> Command<Message> {
        if let Some((mut watcher, old_paths)) = self.watcher_opt.take() {
            let mut new_paths = HashSet::new();
            if let Location::Path(path) = &self.tab.location {
                new_paths.insert(path.clone());
            }

            // Unwatch paths no longer used
            for path in old_paths.iter() {
                if !new_paths.contains(path) {
                    match watcher.watcher().unwatch(path) {
                        Ok(()) => {
                            log::debug!("unwatching {:?}", path);
                        }
                        Err(err) => {
                            log::debug!("failed to unwatch {:?}: {}", path, err);
                        }
                    }
                }
            }

            // Watch new paths
            for path in new_paths.iter() {
                if !old_paths.contains(path) {
                    //TODO: should this be recursive?
                    match watcher
                        .watcher()
                        .watch(path, notify::RecursiveMode::NonRecursive)
                    {
                        Ok(()) => {
                            log::debug!("watching {:?}", path);
                        }
                        Err(err) => {
                            log::debug!("failed to watch {:?}: {}", path, err);
                        }
                    }
                }
            }

            self.watcher_opt = Some((watcher, new_paths));
        }

        //TODO: should any of this run in a command?
        Command::none()
    }
}

/// Implement [`cosmic::Application`] to integrate with COSMIC.
impl cosmic::Application for App {
    /// Default async executor to use with the app.
    type Executor = executor::Default;

    /// Argument received [`cosmic::Application::new`].
    type Flags = ();

    /// Message type specific to our [`App`].
    type Message = Message;

    /// The unique application ID to supply to the window manager.
    const APP_ID: &'static str = "com.system76.CosmicFilesDesktop";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    /// Creates the application, and optionally emits command on initialize.
    fn init(mut core: Core, flags: Self::Flags) -> (Self, Command<Self::Message>) {
        core.window.content_container = false;
        core.window.show_window_menu = false;
        core.window.show_headerbar = false;
        core.window.sharp_corners = false;
        core.window.show_maximize = false;
        core.window.show_minimize = false;
        core.window.use_template = false;

        let location = if let Some(path) = dirs::desktop_dir() {
            Location::Path(path)
        } else {
            Location::Path(cosmic_files::home_dir())
        };
        let mut tab = Tab::new(location, TabConfig::default());
        tab.desktop_mode = true;

        let mut app = App {
            core,
            key_binds: HashMap::new(),
            modifiers: Modifiers::empty(),
            surface_ids: HashMap::new(),
            surface_names: HashMap::new(),
            tab,
            watcher_opt: None,
        };
        let commands = Command::batch([app.update_watcher(), app.rescan_tab()]);
        (app, commands)
    }

    /// Handle application events here.
    fn update(&mut self, message: Self::Message) -> Command<Self::Message> {
        match message {
            Message::OutputEvent(output_event, output) => {
                match output_event {
                    OutputEvent::Created(output_info_opt) => {
                        log::info!("output {}: created", output.id());

                        let surface_id = SurfaceId::unique();
                        match self.surface_ids.insert(output.clone(), surface_id) {
                            Some(old_surface_id) => {
                                //TODO: remove old surface?
                                log::warn!(
                                    "output {}: already had surface ID {:?}",
                                    output.id(),
                                    old_surface_id
                                );
                            }
                            None => {}
                        }

                        match output_info_opt {
                            Some(output_info) => match output_info.name {
                                Some(output_name) => {
                                    self.surface_names.insert(surface_id, output_name.clone());
                                }
                                None => {
                                    log::warn!("output {}: no output name", output.id());
                                }
                            },
                            None => {
                                log::warn!("output {}: no output info", output.id());
                            }
                        }

                        return Command::batch([get_layer_surface(SctkLayerSurfaceSettings {
                            id: surface_id,
                            layer: Layer::Bottom,
                            keyboard_interactivity: KeyboardInteractivity::None,
                            pointer_interactivity: true,
                            anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                            output: IcedOutput::Output(output),
                            namespace: "cosmic-files-desktop".into(),
                            size: Some((None, None)),
                            margin: IcedMargin {
                                top: 0,
                                bottom: 0,
                                left: 0,
                                right: 0,
                            },
                            exclusive_zone: -1,
                            size_limits: iced::Limits::NONE.min_width(1.0).min_height(1.0),
                        })]);
                    }
                    OutputEvent::Removed => {
                        log::info!("output {}: removed", output.id());
                        match self.surface_ids.remove(&output) {
                            Some(surface_id) => {
                                self.surface_names.remove(&surface_id);
                                return destroy_layer_surface(surface_id);
                            }
                            None => {
                                log::warn!("output {}: no surface found", output.id());
                            }
                        }
                    }
                    OutputEvent::InfoUpdate(_output_info) => {
                        log::info!("output {}: info update", output.id());
                    }
                }
            }
            Message::LayerEvent(layer_event, surface_id) => match layer_event {
                LayerEvent::Focused => {
                    log::info!("focus surface {:?}", surface_id);
                }
                _ => {}
            },
            Message::Modifiers(modifiers) => {
                self.modifiers = modifiers;
            }
            Message::NotifyEvents(events) => {
                log::debug!("{:?}", events);

                if let Location::Path(path) = self.tab.location.clone() {
                    let mut contains_change = false;
                    for event in events.iter() {
                        for event_path in event.paths.iter() {
                            if event_path.starts_with(&path) {
                                match event.kind {
                                    notify::EventKind::Modify(
                                        notify::event::ModifyKind::Metadata(_),
                                    )
                                    | notify::EventKind::Modify(notify::event::ModifyKind::Data(
                                        _,
                                    )) => {
                                        // If metadata or data changed, find the matching item and reload it
                                        //TODO: this could be further optimized by looking at what exactly changed
                                        if let Some(items) = &mut self.tab.items_opt_mut() {
                                            for item in items.iter_mut() {
                                                if item.path_opt.as_ref() == Some(event_path) {
                                                    //TODO: reload more, like mime types?
                                                    match fs::metadata(&event_path) {
                                                        Ok(new_metadata) => {
                                                            match &mut item.metadata {
                                                                ItemMetadata::Path {
                                                                    metadata,
                                                                    ..
                                                                } => *metadata = new_metadata,
                                                                _ => {}
                                                            }
                                                        }
                                                        Err(err) => {
                                                            log::warn!("failed to reload metadata for {:?}: {}", path, err);
                                                        }
                                                    }
                                                    //TODO item.thumbnail_opt =
                                                }
                                            }
                                        }
                                    }
                                    _ => {
                                        // Any other events reload the whole tab
                                        contains_change = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if contains_change {
                        return self.rescan_tab();
                    }
                }
            }
            Message::NotifyWatcher(mut watcher_wrapper) => match watcher_wrapper.watcher_opt.take()
            {
                Some(watcher) => {
                    self.watcher_opt = Some((watcher, HashSet::new()));
                    return self.update_watcher();
                }
                None => {
                    log::warn!("message did not contain notify watcher");
                }
            },
            Message::TabMessage(tab_message) => {
                let tab_commands = self.tab.update(tab_message, self.modifiers);

                let mut commands = Vec::new();
                for tab_command in tab_commands {
                    match tab_command {
                        tab::Command::Action(action) => match action.message() {
                            app::Message::TabMessage(_entity_opt, tab_message) => {
                                commands.push(self.update(Message::TabMessage(tab_message)));
                            }
                            unsupported => {
                                log::warn!("{unsupported:?} not supported in desktop mode");
                            }
                        },
                        tab::Command::FocusButton(id) => {
                            commands.push(widget::button::focus(id));
                        }
                        tab::Command::FocusTextInput(id) => {
                            commands.push(widget::text_input::focus(id));
                        }
                        tab::Command::OpenFile(item_path) => {
                            match open::that_detached(&item_path) {
                                Ok(()) => (),
                                Err(err) => {
                                    log::warn!("failed to open {:?}: {}", item_path, err);
                                }
                            }
                        }
                        tab::Command::Scroll(id, offset) => {
                            commands.push(scrollable::scroll_to(id, offset));
                        }
                        unsupported => {
                            log::warn!("{unsupported:?} not supported in desktop mode");
                        }
                    }
                }
                return Command::batch(commands);
            }
            Message::TabRescan(items) => {
                self.tab.set_items(items);
            }
        }
        Command::none()
    }

    // Not used for layer surface window
    fn view(&self) -> Element<Self::Message> {
        unimplemented!()
    }

    /// Creates a view after each update.
    fn view_window(&self, surface_id: SurfaceId) -> Element<Self::Message> {
        self.tab
            .view(&self.key_binds)
            .map(Message::TabMessage)
            .into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        struct WatcherSubscription;

        Subscription::batch([
            event::listen_with(|event, _| match event {
                iced::Event::PlatformSpecific(iced::event::PlatformSpecific::Wayland(
                    wayland_event,
                )) => match wayland_event {
                    WaylandEvent::Output(output_event, output) => {
                        Some(Message::OutputEvent(output_event, output))
                    }
                    WaylandEvent::Layer(layer_event, _surface, surface_id) => {
                        Some(Message::LayerEvent(layer_event, surface_id))
                    }
                    _ => None,
                },
                Event::Keyboard(KeyEvent::ModifiersChanged(modifiers)) => {
                    Some(Message::Modifiers(modifiers))
                }
                _ => None,
            }),
            subscription::channel(
                TypeId::of::<WatcherSubscription>(),
                100,
                |mut output| async move {
                    let watcher_res = {
                        let mut output = output.clone();
                        new_debouncer(
                            time::Duration::from_millis(250),
                            Some(time::Duration::from_millis(250)),
                            move |events_res: notify_debouncer_full::DebounceEventResult| {
                                match events_res {
                                    Ok(mut events) => {
                                        events.retain(|event| {
                                            match &event.kind {
                                                notify::EventKind::Access(_) => {
                                                    // Data not mutated
                                                    false
                                                }
                                                notify::EventKind::Modify(
                                                    notify::event::ModifyKind::Metadata(e),
                                                ) if (*e != notify::event::MetadataKind::Any
                                                    && *e
                                                        != notify::event::MetadataKind::WriteTime) =>
                                                {
                                                    // Data not mutated nor modify time changed
                                                    false
                                                }
                                                _ => true
                                            }
                                        });

                                        if !events.is_empty() {
                                            match futures::executor::block_on(async {
                                                output.send(Message::NotifyEvents(events)).await
                                            }) {
                                                Ok(()) => {}
                                                Err(err) => {
                                                    log::warn!(
                                                        "failed to send notify events: {:?}",
                                                        err
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        log::warn!("failed to watch files: {:?}", err);
                                    }
                                }
                            },
                        )
                    };

                    match watcher_res {
                        Ok(watcher) => {
                            match output
                                .send(Message::NotifyWatcher(WatcherWrapper {
                                    watcher_opt: Some(watcher),
                                }))
                                .await
                            {
                                Ok(()) => {}
                                Err(err) => {
                                    log::warn!("failed to send notify watcher: {:?}", err);
                                }
                            }
                        }
                        Err(err) => {
                            log::warn!("failed to create file watcher: {:?}", err);
                        }
                    }

                    std::future::pending().await
                },
            ),
            self.tab.subscription().map(Message::TabMessage),
        ])
    }
}
