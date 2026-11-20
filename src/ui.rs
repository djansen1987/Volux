use std::collections::{HashMap, HashSet};
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, RichText, Slider, ViewportCommand};
use muda::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::config::{Config, Preset, set_start_at_login};
use crate::state::{AudioChannels, AudioCommand, AudioDevice, AudioFlow, AudioSnapshot, Session};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionGrouping {
    Device,
    Application,
}

pub struct MixerApp {
    audio: AudioChannels,
    snapshot: AudioSnapshot,
    config: Config,
    message: Option<String>,
    _tray: Option<TrayIcon>,
    open_id: muda::MenuId,
    mute_id: muda::MenuId,
    exit_id: muda::MenuId,
    exit_requested: bool,
    playing_only: bool,
    show_all_items: bool,
    session_grouping: SessionGrouping,
    selected_preset: String,
    preset_name: String,
    saving_preset: bool,
    pending_sliders: HashMap<String, (f32, Instant)>,
    spectrum_expanded: bool,
}

impl MixerApp {
    pub fn new(audio: AudioChannels) -> Self {
        let (mut config, load_error) = match Config::load() {
            Ok(config) => (config, None),
            Err(error) => (Config::default(), Some(error.to_string())),
        };
        if config.presets.is_empty() && !config.preset.is_empty() {
            config.presets.push(Preset {
                name: "Previous preset".to_string(),
                entries: config.preset.clone(),
                devices: Vec::new(),
            });
        }
        let selected_preset = config
            .presets
            .first()
            .map(|preset| preset.name.clone())
            .unwrap_or_default();

        let open = MenuItem::new("Open Volux", true, None);
        let mute = MenuItem::new("Toggle master mute", true, None);
        let exit = MenuItem::new("Exit", true, None);
        let open_id = open.id().clone();
        let mute_id = mute.id().clone();
        let exit_id = exit.id().clone();
        let menu = Menu::new();
        let mut tray_error = None;

        if let Err(error) = menu.append_items(&[&open, &mute, &exit]) {
            tray_error = Some(error.to_string());
        }

        let tray = match TrayIconBuilder::new()
            .with_tooltip("Volux")
            .with_icon(create_icon())
            .with_menu(Box::new(menu))
            .build()
        {
            Ok(tray) => Some(tray),
            Err(error) => {
                tray_error = Some(format!("Could not create the tray icon: {error}"));
                None
            }
        };

        Self {
            audio,
            snapshot: AudioSnapshot::default(),
            config,
            message: load_error.or(tray_error),
            _tray: tray,
            open_id,
            mute_id,
            exit_id,
            exit_requested: false,
            playing_only: false,
            show_all_items: false,
            session_grouping: SessionGrouping::Device,
            selected_preset,
            preset_name: String::new(),
            saving_preset: false,
            pending_sliders: HashMap::new(),
            spectrum_expanded: false,
        }
    }

    fn send(&mut self, command: AudioCommand) {
        if let Err(error) = self.audio.commands.send(command) {
            self.message = Some(format!("Audio control stopped: {error}"));
        }
    }

    fn receive_snapshot(&mut self) {
        self.pending_sliders
            .retain(|_, (_, changed_at)| changed_at.elapsed() < Duration::from_millis(500));
        while let Ok(mut snapshot) = self.audio.snapshots.try_recv() {
            for device in &mut snapshot.devices {
                restore_pending_value(
                    &self.pending_sliders,
                    &format!("device:{}:volume", device.id),
                    &mut device.volume,
                );
                restore_pending_value(
                    &self.pending_sliders,
                    &format!("device:{}:balance", device.id),
                    &mut device.balance,
                );
            }
            for session in &mut snapshot.sessions {
                restore_pending_value(
                    &self.pending_sliders,
                    &format!("session:{}:volume", session.id),
                    &mut session.volume,
                );
                restore_pending_value(
                    &self.pending_sliders,
                    &format!("session:{}:balance", session.id),
                    &mut session.balance,
                );
            }
            self.snapshot = snapshot;
            if self.snapshot.error.is_none() {
                self.message = None;
            }
        }
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id() == &self.open_id {
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            } else if event.id() == &self.mute_id {
                self.send(AudioCommand::SetMasterMuted(!self.snapshot.master_muted));
            } else if event.id() == &self.exit_id {
                self.exit_requested = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }

    fn handle_window_close(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested && !self.exit_requested {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }
    }

    fn save_preset(&mut self) {
        let name = self.preset_name.trim();
        if name.is_empty() {
            self.message = Some("Enter a name for the preset.".to_string());
            return;
        }

        let preset = Preset {
            name: name.to_string(),
            entries: self.snapshot.sessions.iter().map(Into::into).collect(),
            devices: self.snapshot.devices.iter().map(Into::into).collect(),
        };
        let application_count = preset.entries.len();
        let device_count = preset.devices.len();
        if let Some(existing) = self
            .config
            .presets
            .iter_mut()
            .find(|existing| existing.name.eq_ignore_ascii_case(name))
        {
            *existing = preset;
        } else {
            self.config.presets.push(preset);
        }
        self.selected_preset = name.to_string();
        match self.config.save() {
            Ok(()) => {
                self.saving_preset = false;
                self.preset_name.clear();
                self.message = Some(format!(
                    "Preset saved with {} application{} and {} device{}.",
                    application_count,
                    if application_count == 1 { "" } else { "s" },
                    device_count,
                    if device_count == 1 { "" } else { "s" }
                ));
            }
            Err(error) => self.message = Some(error.to_string()),
        }
    }

    fn apply_preset(&mut self) {
        let Some(preset) = self
            .config
            .presets
            .iter()
            .find(|preset| preset.name == self.selected_preset)
        else {
            self.message = Some("Select or save a preset first.".to_string());
            return;
        };

        let mut commands = Vec::new();
        let mut matched = 0;
        let mut matched_devices = 0;
        self.pending_sliders.clear();
        for entry in &preset.entries {
            if let Some(session) = self.snapshot.sessions.iter_mut().find(|session| {
                session
                    .process_name
                    .eq_ignore_ascii_case(&entry.process_name)
                    && session.flow == entry.flow
                    && (entry.device_id.is_empty() && entry.device_name.is_empty()
                        || session.device_id == entry.device_id
                        || session.device_name.eq_ignore_ascii_case(&entry.device_name))
            }) {
                let id = session.id.clone();
                commands.push(AudioCommand::SetVolume {
                    id: id.clone(),
                    value: entry.volume,
                });
                commands.push(AudioCommand::SetMuted {
                    id: id.clone(),
                    value: entry.muted,
                });
                if session.balance_supported {
                    commands.push(AudioCommand::SetBalance {
                        id: id.clone(),
                        value: entry.balance,
                    });
                    session.balance = entry.balance;
                }
                session.volume = entry.volume;
                session.muted = entry.muted;
                matched += 1;
            }
        }
        for entry in &preset.devices {
            if let Some(device) = self.snapshot.devices.iter_mut().find(|device| {
                device.id == entry.id
                    || (device.flow == entry.flow && device.name.eq_ignore_ascii_case(&entry.name))
            }) {
                commands.push(AudioCommand::SetDeviceVolume {
                    id: device.id.clone(),
                    value: entry.volume,
                });
                commands.push(AudioCommand::SetDeviceMuted {
                    id: device.id.clone(),
                    value: entry.muted,
                });
                if device.balance_supported {
                    commands.push(AudioCommand::SetDeviceBalance {
                        id: device.id.clone(),
                        value: entry.balance,
                    });
                    device.balance = entry.balance;
                }
                device.volume = entry.volume;
                device.muted = entry.muted;
                matched_devices += 1;
            }
        }
        for command in commands {
            self.send(command);
        }

        self.message = Some(format!(
            "Applied preset to {matched} running application{} and {matched_devices} device{}.",
            if matched == 1 { "" } else { "s" },
            if matched_devices == 1 { "" } else { "s" }
        ));
    }

    fn launch_windows_command(&mut self, program: &str, argument: Option<&str>, label: &str) {
        let mut command = Command::new(program);
        if let Some(argument) = argument {
            command.arg(argument);
        }
        match command.creation_flags(CREATE_NO_WINDOW).spawn() {
            Ok(_) => self.message = None,
            Err(error) => self.message = Some(format!("Could not open {label}: {error}")),
        }
    }
}

impl eframe::App for MixerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive_snapshot();
        self.handle_tray(ctx);
        self.handle_window_close(ctx);
        ctx.request_repaint_after(Duration::from_millis(25));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Frame::new()
            .fill(Color32::from_rgb(18, 24, 34))
            .inner_margin(18.0)
            .show(ui, |ui| {
                self.draw_header(ui);
                self.draw_toolbar(ui);
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.draw_master(ui);
                        ui.add_space(12.0);
                        self.draw_sessions(ui);
                        ui.add_space(10.0);
                        self.draw_footer(ui);
                    });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.audio.commands.send(AudioCommand::Shutdown);
    }
}

impl MixerApp {
    fn draw_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(113, 92, 255, 40))
                .corner_radius(10.0)
                .inner_margin(6.0)
                .show(ui, |ui| {
                    let (rgba, width, height) = icon_rgba();
                    let texture = ui.ctx().load_texture(
                        "volux-header-logo",
                        egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize],
                            &rgba,
                        ),
                        egui::TextureOptions::LINEAR,
                    );
                    ui.add(egui::Image::new(&texture).fit_to_exact_size(egui::vec2(42.0, 42.0)));
                });
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Volux")
                        .size(26.0)
                        .strong()
                        .color(Color32::from_rgb(231, 238, 248)),
                );
                ui.label(
                    RichText::new("Your desktop sound, in balance")
                        .size(12.0)
                        .color(Color32::from_rgb(139, 156, 178)),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(egui::Button::new("Refresh").min_size(egui::vec2(76.0, 30.0)))
                    .clicked()
                {
                    self.send(AudioCommand::Refresh);
                }
            });
        });
    }

    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Preset");
            egui::ComboBox::from_id_salt("preset-selection-toolbar")
                .width(170.0)
                .selected_text(if self.selected_preset.is_empty() {
                    "No presets yet"
                } else {
                    &self.selected_preset
                })
                .show_ui(ui, |ui| {
                    for preset in &self.config.presets {
                        ui.selectable_value(
                            &mut self.selected_preset,
                            preset.name.clone(),
                            &preset.name,
                        );
                    }
                });
            if ui
                .add_enabled(
                    !self.config.presets.is_empty(),
                    egui::Button::new("Load preset"),
                )
                .clicked()
            {
                self.apply_preset();
            }
            if ui.button("Save preset…").clicked() {
                self.saving_preset = !self.saving_preset;
                self.message = None;
            }
            ui.separator();
            ui.menu_button("Menus", |ui| {
                if ui.button("Volume mixer (sndvol.exe)").clicked() {
                    self.launch_windows_command("sndvol.exe", None, "Windows volume mixer");
                    ui.close();
                }
                if ui.button("Classic Sound control panel").clicked() {
                    self.launch_windows_command(
                        "control.exe",
                        Some("mmsys.cpl"),
                        "classic Windows Sound settings",
                    );
                    ui.close();
                }
                if ui.button("App volume settings").clicked() {
                    self.launch_windows_command(
                        "explorer.exe",
                        Some("ms-settings:apps-volume"),
                        "Windows app volume settings",
                    );
                    ui.close();
                }
                if ui.button("Windows Sound settings").clicked() {
                    self.launch_windows_command(
                        "explorer.exe",
                        Some("ms-settings:sound"),
                        "Windows Sound settings",
                    );
                    ui.close();
                }
            });
        });

        if self.saving_preset {
            ui.horizontal(|ui| {
                ui.label("Preset name");
                let response = ui.text_edit_singleline(&mut self.preset_name);
                if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    self.save_preset();
                } else {
                    response.request_focus();
                }
                if ui.button("Save as new").clicked() {
                    self.save_preset();
                }
                if ui.button("Cancel").clicked() {
                    self.saving_preset = false;
                    self.preset_name.clear();
                    self.message = None;
                }
            });
        }
    }

    fn draw_master(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Audio devices");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.checkbox(&mut self.show_all_items, "Show all");
            });
        });
        ui.add_space(4.0);
        let mut commands = Vec::new();
        let active_device_ids = self
            .snapshot
            .sessions
            .iter()
            .filter(|session| session.active)
            .map(|session| session.device_id.clone())
            .collect::<HashSet<_>>();
        let hidden_devices = self.config.hidden_devices.clone();
        let mut visibility_changes = Vec::new();

        for flow in [AudioFlow::Output, AudioFlow::Input] {
            let has_visible_devices = self.snapshot.devices.iter().any(|device| {
                device.flow == flow
                    && (self.show_all_items
                        || (!hidden_devices.contains(&device.id)
                            && (device.is_default
                                || device.peak > 0.001
                                || active_device_ids.contains(&device.id))))
            });
            if !has_visible_devices {
                continue;
            }

            ui.label(
                RichText::new(match flow {
                    AudioFlow::Output => "OUTPUT DEVICES",
                    AudioFlow::Input => "INPUT DEVICES",
                })
                .size(11.0)
                .strong()
                .color(Color32::from_rgb(166, 151, 255)),
            );
            for device in self
                .snapshot
                .devices
                .iter_mut()
                .filter(|device| device.flow == flow)
                .filter(|device| {
                    self.show_all_items
                        || (!hidden_devices.contains(&device.id)
                            && (device.is_default
                                || device.peak > 0.001
                                || active_device_ids.contains(&device.id)))
                })
            {
                let hidden = hidden_devices.contains(&device.id);
                featured_card(ui, |ui| {
                    if draw_device_card(
                        ui,
                        device,
                        &mut commands,
                        hidden,
                        &mut self.pending_sliders,
                        &mut self.spectrum_expanded,
                    ) {
                        visibility_changes.push((device.id.clone(), !hidden));
                    }
                });
                ui.add_space(6.0);
            }
        }

        for command in commands {
            self.send(command);
        }
        if !visibility_changes.is_empty() {
            for (id, hidden) in visibility_changes {
                set_hidden(&mut self.config.hidden_devices, &id, hidden);
            }
            self.save_visibility();
        }
    }

    fn draw_sessions(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Applications");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.checkbox(&mut self.playing_only, "Playing only");
                let grouping_label = match self.session_grouping {
                    SessionGrouping::Device => "Device",
                    SessionGrouping::Application => "Application",
                };
                egui::ComboBox::from_id_salt("session-grouping")
                    .width(104.0)
                    .selected_text(grouping_label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.session_grouping,
                            SessionGrouping::Device,
                            "Device",
                        );
                        ui.selectable_value(
                            &mut self.session_grouping,
                            SessionGrouping::Application,
                            "Application",
                        );
                    });
                ui.label(
                    RichText::new(format!(
                        "{} playing · {} total",
                        self.snapshot
                            .sessions
                            .iter()
                            .filter(|session| session.active)
                            .count(),
                        self.snapshot.sessions.len()
                    ))
                    .size(11.0)
                    .color(Color32::from_rgb(137, 153, 175)),
                );
            });
        });
        ui.add_space(4.0);

        let visible_count = self
            .snapshot
            .sessions
            .iter()
            .filter(|session| !self.playing_only || session.active)
            .filter(|session| {
                self.show_all_items
                    || !self
                        .config
                        .hidden_sessions
                        .contains(&session_visibility_key(session))
            })
            .count();

        if visible_count == 0 {
            card(ui, |ui| {
                ui.label(
                    RichText::new(if self.playing_only {
                        "No applications are playing audio right now."
                    } else {
                        "No audio applications were found."
                    })
                    .color(Color32::from_rgb(139, 156, 178)),
                );
                ui.label("Start audio in an application and it will appear here automatically.");
            });
            return;
        }

        let mut commands = Vec::new();
        let mut groups = std::collections::BTreeMap::<String, Vec<usize>>::new();
        for (index, session) in self.snapshot.sessions.iter().enumerate() {
            if self.playing_only && !session.active {
                continue;
            }
            if !self.show_all_items
                && self
                    .config
                    .hidden_sessions
                    .contains(&session_visibility_key(session))
            {
                continue;
            }
            let key = match self.session_grouping {
                SessionGrouping::Device => {
                    format!("{} · {}", flow_label(session.flow), session.device_name)
                }
                SessionGrouping::Application => session.process_name.clone(),
            };
            groups.entry(key).or_default().push(index);
        }

        let mut visibility_changes = Vec::new();
        let hidden_sessions = self.config.hidden_sessions.clone();
        for (group, indices) in groups {
            ui.label(
                RichText::new(group)
                    .size(12.0)
                    .strong()
                    .color(Color32::from_rgb(166, 151, 255)),
            );
            ui.add_space(3.0);
            for index in indices {
                let session = &mut self.snapshot.sessions[index];
                let key = session_visibility_key(session);
                let hidden = hidden_sessions.contains(&key);
                card(ui, |ui| {
                    if draw_session_card(
                        ui,
                        session,
                        &mut commands,
                        hidden,
                        &mut self.pending_sliders,
                    ) {
                        visibility_changes.push((key, !hidden));
                    }
                });
                ui.add_space(5.0);
            }
        }
        for command in commands {
            self.send(command);
        }
        if !visibility_changes.is_empty() {
            for (key, hidden) in visibility_changes {
                set_hidden(&mut self.config.hidden_sessions, &key, hidden);
            }
            self.save_visibility();
        }
    }

    fn save_visibility(&mut self) {
        if let Err(error) = self.config.save() {
            self.message = Some(error.to_string());
        }
    }

    fn draw_footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut startup = self.config.start_at_login;
            if ui.checkbox(&mut startup, "Start with Windows").changed() {
                match set_start_at_login(startup) {
                    Ok(()) => {
                        self.config.start_at_login = startup;
                        match self.config.save() {
                            Ok(()) => self.message = None,
                            Err(error) => self.message = Some(error.to_string()),
                        }
                    }
                    Err(error) => self.message = Some(error.to_string()),
                }
            }
        });

        if let Some(error) = self.snapshot.error.as_deref().or(self.message.as_deref()) {
            ui.add_space(5.0);
            ui.colored_label(Color32::from_rgb(255, 179, 99), error);
        }
    }
}

fn draw_device_card(
    ui: &mut egui::Ui,
    device: &mut AudioDevice,
    commands: &mut Vec<AudioCommand>,
    hidden: bool,
    pending_sliders: &mut HashMap<String, (f32, Instant)>,
    spectrum_expanded: &mut bool,
) -> bool {
    let visibility_changed = ui
        .horizontal(|ui| {
            ui.label(
                RichText::new(if device.is_default {
                    format!("{} · DEFAULT", device.name)
                } else {
                    device.name.clone()
                })
                .size(15.0)
                .strong(),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let visibility_changed =
                    ui.button(if hidden { "Unhide" } else { "Hide" }).clicked();
                if ui
                    .add_sized(
                        [74.0, 24.0],
                        egui::Button::new(if device.muted { "Muted" } else { "Sound on" }),
                    )
                    .clicked()
                {
                    device.muted = !device.muted;
                    commands.push(AudioCommand::SetDeviceMuted {
                        id: device.id.clone(),
                        value: device.muted,
                    });
                }
                visibility_changed
            })
            .inner
        })
        .inner;

    if device.flow == AudioFlow::Output && device.is_default {
        ui.add_space(2.0);
        ui.label(
            RichText::new("Master output spectrum")
                .size(10.0)
                .color(Color32::from_rgb(139, 156, 178)),
        );
        if draw_spectrum(
            ui,
            egui::Id::unique(("master-spectrum", &device.id)),
            &device.spectrum,
            &device.spectrum_peak,
            *spectrum_expanded,
        )
        .clicked()
        {
            *spectrum_expanded = !*spectrum_expanded;
        }
        ui.add_space(2.0);
    }

    ui.horizontal(|ui| {
        ui.label("Volume");
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            ui.label(percent(device.volume));
        });
    });
    let response = full_width_slider(ui, &mut device.volume, 0.0..=1.0, true);
    if response.changed() {
        remember_pending(
            pending_sliders,
            format!("device:{}:volume", device.id),
            device.volume,
        );
        commands.push(AudioCommand::SetDeviceVolume {
            id: device.id.clone(),
            value: device.volume,
        });
    }
    paint_peak(ui, response.rect, device.peak);

    ui.horizontal(|ui| {
        ui.label("Balance");
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            if device.balance_supported {
                let label = balance_label(device.balance);
                if !label.is_empty() {
                    ui.label(RichText::new(label).monospace());
                }
            } else {
                ui.label("Not available");
            }
        });
    });
    if device.balance_supported {
        let response = full_width_slider(ui, &mut device.balance, -1.0..=1.0, false);
        paint_balance_center(ui, response.rect);
        if balance_center_clicked(&response) {
            device.balance = 0.0;
            remember_pending(
                pending_sliders,
                format!("device:{}:balance", device.id),
                device.balance,
            );
            commands.push(AudioCommand::SetDeviceBalance {
                id: device.id.clone(),
                value: 0.0,
            });
        } else if response.changed() {
            if device.balance.abs() < 0.04 {
                device.balance = 0.0;
            }
            remember_pending(
                pending_sliders,
                format!("device:{}:balance", device.id),
                device.balance,
            );
            commands.push(AudioCommand::SetDeviceBalance {
                id: device.id.clone(),
                value: device.balance,
            });
        }
    } else {
        ui.add_enabled_ui(false, |ui| {
            full_width_slider(ui, &mut device.balance, -1.0..=1.0, false);
        });
    }
    visibility_changed
}

fn draw_spectrum(
    ui: &mut egui::Ui,
    id: egui::Id,
    bands: &[f32],
    peak_bands: &[f32],
    expanded: bool,
) -> egui::Response {
    let hover_expanded = ui.ctx().data(|data| {
        data.get_temp::<bool>(id.with("hover-expanded"))
            .unwrap_or(false)
    });
    let response = ui.allocate_response(
        egui::vec2(
            ui.available_width(),
            if expanded {
                220.0
            } else if hover_expanded {
                112.0
            } else {
                34.0
            },
        ),
        egui::Sense::click(),
    );
    paint_spectrum(ui, response.rect, bands, peak_bands);
    ui.ctx()
        .data_mut(|data| data.insert_temp(id.with("hover-expanded"), response.hovered()));

    response
}

fn paint_spectrum(ui: &egui::Ui, rect: egui::Rect, bands: &[f32], peak_bands: &[f32]) {
    ui.painter()
        .rect_filled(rect, 6.0, Color32::from_rgb(12, 17, 24));
    if bands.is_empty() {
        return;
    }

    let detailed = rect.height() >= 96.0;
    let left_margin = if detailed { 35.0 } else { 0.0 };
    let bottom_margin = if detailed { 18.0 } else { 0.0 };
    let plot = egui::Rect::from_min_max(
        egui::pos2(rect.left() + left_margin, rect.top() + 5.0),
        egui::pos2(rect.right() - 4.0, rect.bottom() - bottom_margin),
    );

    for (index, db) in [0, -12, -24, -36, -48, -60, -72].iter().enumerate() {
        let fraction = index as f32 / 6.0;
        let y = plot.top() + plot.height() * fraction;
        ui.painter().line_segment(
            [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
            egui::Stroke::new(0.5, Color32::from_rgba_unmultiplied(129, 143, 166, 22)),
        );
        if detailed {
            ui.painter().text(
                egui::pos2(rect.left() + 31.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{db}"),
                egui::FontId::monospace(8.0),
                Color32::from_rgb(119, 133, 153),
            );
        }
    }

    let points = spectrum_points(plot, bands);
    let peak_points = spectrum_points(plot, peak_bands);

    if detailed {
        for frequency in [
            20.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, 20_000.0,
        ] {
            let position = (frequency / 20.0_f32).ln() / 1_000.0_f32.ln();
            let x = plot.left() + plot.width() * position;
            ui.painter().line_segment(
                [egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())],
                egui::Stroke::new(0.5, Color32::from_rgba_unmultiplied(129, 143, 166, 14)),
            );
            ui.painter().text(
                egui::pos2(x, rect.bottom() - 9.0),
                egui::Align2::CENTER_CENTER,
                frequency_label(frequency),
                egui::FontId::monospace(8.0),
                Color32::from_rgb(119, 133, 153),
            );
        }
        ui.painter().text(
            egui::pos2(rect.right() - 4.0, rect.top() + 10.0),
            egui::Align2::RIGHT_TOP,
            "LIVE  /  PEAK HOLD",
            egui::FontId::monospace(8.0),
            Color32::from_rgba_unmultiplied(191, 201, 219, 170),
        );
        ui.painter().text(
            egui::pos2(rect.left() + 3.0, rect.top() + 2.0),
            egui::Align2::LEFT_TOP,
            "dB",
            egui::FontId::monospace(8.0),
            Color32::from_rgb(119, 133, 153),
        );
    }

    paint_spectrum_fill(
        ui,
        &peak_points,
        plot.bottom(),
        Color32::from_rgba_unmultiplied(255, 178, 70, 68),
    );

    if !peak_points.is_empty() {
        ui.painter().add(egui::Shape::line(
            peak_points,
            egui::Stroke::new(1.3, Color32::from_rgb(255, 192, 86)),
        ));
    }

    paint_spectrum_fill(
        ui,
        &points,
        plot.bottom(),
        Color32::from_rgba_unmultiplied(76, 210, 178, 42),
    );

    ui.painter().add(egui::Shape::line(
        points.clone(),
        egui::Stroke::new(5.0, Color32::from_rgba_unmultiplied(113, 92, 255, 25)),
    ));
    ui.painter().add(egui::Shape::line(
        points.clone(),
        egui::Stroke::new(2.7, Color32::from_rgba_unmultiplied(174, 150, 255, 92)),
    ));
    ui.painter().add(egui::Shape::line(
        points,
        egui::Stroke::new(1.15, Color32::from_rgb(116, 241, 200)),
    ));
}

fn paint_spectrum_fill(ui: &egui::Ui, points: &[egui::Pos2], bottom: f32, color: Color32) {
    if points.len() < 2 {
        return;
    }

    let mut mesh = egui::epaint::Mesh::default();
    for point in points {
        mesh.vertices
            .push(egui::epaint::Vertex::untextured(*point, color));
        mesh.vertices.push(egui::epaint::Vertex::untextured(
            egui::pos2(point.x, bottom),
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 6),
        ));
    }
    for index in 0..points.len() - 1 {
        let base = index as u32 * 2;
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base + 1, base + 2, base + 3);
    }
    ui.painter().add(egui::Shape::mesh(mesh));
}

fn spectrum_points(plot: egui::Rect, bands: &[f32]) -> Vec<egui::Pos2> {
    let points = bands
        .iter()
        .enumerate()
        .map(|(index, band)| {
            let x = plot.left() + plot.width() * index as f32 / (bands.len() - 1).max(1) as f32;
            let normalized = band.clamp(0.0, 1.0).powf(0.68);
            let y = plot.bottom() - normalized * plot.height();
            egui::pos2(x, y)
        })
        .collect::<Vec<_>>();

    if points.len() < 3 {
        return points;
    }

    let mut smoothed = Vec::with_capacity((points.len() - 1) * 4 + 1);
    for index in 0..points.len() - 1 {
        let previous = points[index.saturating_sub(1)];
        let current = points[index];
        let next = points[index + 1];
        let following = points[(index + 2).min(points.len() - 1)];
        for step in 0..4 {
            let t = step as f32 / 4.0;
            let x = current.x + (next.x - current.x) * t;
            let t_squared = t * t;
            let t_cubed = t_squared * t;
            let y = 0.5
                * ((2.0 * current.y)
                    + (-previous.y + next.y) * t
                    + (2.0 * previous.y - 5.0 * current.y + 4.0 * next.y - following.y)
                        * t_squared
                    + (-previous.y + 3.0 * current.y - 3.0 * next.y + following.y) * t_cubed);
            smoothed.push(egui::pos2(x, y.clamp(plot.top(), plot.bottom())));
        }
    }
    smoothed.push(*points.last().unwrap());
    smoothed
}

fn frequency_label(frequency: f32) -> String {
    if frequency >= 1_000.0 {
        format!("{}k", frequency / 1_000.0)
    } else {
        format!("{}", frequency as u32)
    }
}

fn draw_session_card(
    ui: &mut egui::Ui,
    session: &mut Session,
    commands: &mut Vec<AudioCommand>,
    hidden: bool,
    pending_sliders: &mut HashMap<String, (f32, Instant)>,
) -> bool {
    let visibility_changed = ui
        .horizontal(|ui| {
            ui.label(
                RichText::new(&session.process_name)
                    .size(14.0)
                    .strong()
                    .color(if session.active {
                        Color32::from_rgb(231, 238, 248)
                    } else {
                        Color32::from_rgb(171, 184, 202)
                    }),
            );
            ui.label(
                RichText::new(format!(
                    "{} · {}",
                    flow_label(session.flow),
                    session.device_name
                ))
                .size(10.0)
                .color(Color32::from_rgb(126, 143, 165)),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let visibility_changed =
                    ui.button(if hidden { "Unhide" } else { "Hide" }).clicked();
                if ui.checkbox(&mut session.muted, "Mute").changed() {
                    commands.push(AudioCommand::SetMuted {
                        id: session.id.clone(),
                        value: session.muted,
                    });
                }
                visibility_changed
            })
            .inner
        })
        .inner;

    ui.horizontal(|ui| {
        ui.label("Volume");
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            ui.label(percent(session.volume));
        });
    });
    let response = full_width_slider(ui, &mut session.volume, 0.0..=1.0, true);
    if response.changed() {
        remember_pending(
            pending_sliders,
            format!("session:{}:volume", session.id),
            session.volume,
        );
        commands.push(AudioCommand::SetVolume {
            id: session.id.clone(),
            value: session.volume,
        });
    }
    paint_peak(ui, response.rect, session.peak);

    ui.horizontal(|ui| {
        ui.label("Balance");
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            if session.balance_supported {
                let label = balance_label(session.balance);
                if !label.is_empty() {
                    ui.label(RichText::new(label).monospace());
                }
            } else {
                ui.label("Not available");
            }
        });
    });
    if session.balance_supported {
        let response = full_width_slider(ui, &mut session.balance, -1.0..=1.0, false);
        paint_balance_center(ui, response.rect);
        if balance_center_clicked(&response) {
            session.balance = 0.0;
            remember_pending(
                pending_sliders,
                format!("session:{}:balance", session.id),
                session.balance,
            );
            commands.push(AudioCommand::SetBalance {
                id: session.id.clone(),
                value: 0.0,
            });
        } else if response.changed() {
            if session.balance.abs() < 0.04 {
                session.balance = 0.0;
            }
            remember_pending(
                pending_sliders,
                format!("session:{}:balance", session.id),
                session.balance,
            );
            commands.push(AudioCommand::SetBalance {
                id: session.id.clone(),
                value: session.balance,
            });
        }
    } else {
        ui.add_enabled_ui(false, |ui| {
            full_width_slider(ui, &mut session.balance, -1.0..=1.0, false);
        });
    }
    visibility_changed
}

fn remember_pending(pending: &mut HashMap<String, (f32, Instant)>, key: String, value: f32) {
    pending.insert(key, (value, Instant::now()));
}

fn restore_pending_value(pending: &HashMap<String, (f32, Instant)>, key: &str, value: &mut f32) {
    if let Some((pending_value, changed_at)) = pending.get(key)
        && changed_at.elapsed() < Duration::from_millis(500)
    {
        *value = *pending_value;
    }
}

fn session_visibility_key(session: &Session) -> String {
    format!(
        "{}|{:?}|{}",
        session.process_name.to_lowercase(),
        session.flow,
        session.device_id
    )
}

fn set_hidden(items: &mut Vec<String>, key: &str, hidden: bool) {
    items.retain(|item| item != key);
    if hidden {
        items.push(key.to_string());
        items.sort();
        items.dedup();
    }
}

fn full_width_slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    trailing_fill: bool,
) -> egui::Response {
    let width = ui.available_width();
    ui.scope(|ui| {
        ui.spacing_mut().slider_width = width;
        ui.add(
            Slider::new(value, range)
                .show_value(false)
                .trailing_fill(trailing_fill),
        )
    })
    .inner
}

fn balance_center_clicked(response: &egui::Response) -> bool {
    response.clicked()
        && response
            .interact_pointer_pos()
            .is_some_and(|position| (position.x - response.rect.center().x).abs() <= 8.0)
}

fn paint_balance_center(ui: &egui::Ui, slider_rect: egui::Rect) {
    let center = slider_rect.center();
    ui.painter().line_segment(
        [
            egui::pos2(center.x, slider_rect.top() + 3.0),
            egui::pos2(center.x, slider_rect.bottom() - 3.0),
        ],
        egui::Stroke::new(2.0, Color32::from_rgb(225, 230, 240)),
    );
    ui.painter().circle_filled(
        center,
        3.0,
        Color32::from_rgba_unmultiplied(225, 230, 240, 210),
    );
}

fn flow_label(flow: AudioFlow) -> &'static str {
    match flow {
        AudioFlow::Output => "Output",
        AudioFlow::Input => "Input",
    }
}

fn card(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(Color32::from_rgb(24, 33, 46))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(43, 55, 73)))
        .corner_radius(12.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            let width = ui.available_width();
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 2.0;
            contents(ui);
        });
}

fn featured_card(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(Color32::from_rgb(31, 35, 58))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(92, 75, 190)))
        .corner_radius(14.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            let width = ui.available_width();
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 2.0;
            contents(ui);
        });
}

fn percent(value: f32) -> String {
    format!("{:>3}%", (value * 100.0).round() as i32)
}

fn balance_label(value: f32) -> String {
    let amount = (value.abs() * 100.0).round() as i32;
    if value < -0.004 {
        format!("L {amount:>3}%")
    } else if value > 0.004 {
        format!("R {amount:>3}%")
    } else {
        String::new()
    }
}

fn paint_peak(ui: &egui::Ui, slider_rect: egui::Rect, peak: f32) {
    let width = slider_rect.width() * peak.clamp(0.0, 1.0);
    let meter = egui::Rect::from_min_size(
        slider_rect.left_bottom() - egui::vec2(0.0, 3.0),
        egui::vec2(width, 3.0),
    );
    ui.painter().rect_filled(
        meter,
        2.0,
        if peak > 0.85 {
            Color32::from_rgb(255, 112, 112)
        } else if peak > 0.6 {
            Color32::from_rgb(255, 187, 92)
        } else {
            Color32::from_rgb(75, 214, 159)
        },
    );
}

pub fn icon_rgba() -> (Vec<u8>, u32, u32) {
    const SIZE: u32 = 32;
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let offset = ((y * SIZE + x) * 4) as usize;
            let rounded = !(3..SIZE - 3).contains(&x) || !(3..SIZE - 3).contains(&y);
            if rounded {
                continue;
            }
            rgba[offset] = 113;
            rgba[offset + 1] = 92;
            rgba[offset + 2] = 255;
            rgba[offset + 3] = 255;
            let is_letter = ((8..=12).contains(&x) && (8..=21).contains(&y) && x - 8 <= y - 7)
                || ((19..=23).contains(&x) && (8..=21).contains(&y) && 23 - x <= y - 7)
                || (11..=20).contains(&x) && (19..=24).contains(&y);
            if is_letter {
                rgba[offset] = 255;
                rgba[offset + 1] = 255;
                rgba[offset + 2] = 255;
            }
        }
    }
    (rgba, SIZE, SIZE)
}

fn create_icon() -> Icon {
    let (rgba, width, height) = icon_rgba();
    Icon::from_rgba(rgba, width, height).expect("the generated icon is valid")
}

pub fn window_icon() -> egui::IconData {
    let (rgba, width, height) = icon_rgba();
    egui::IconData {
        rgba,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::balance_label;

    #[test]
    fn centered_balance_has_no_percentage_label() {
        assert_eq!(balance_label(0.0), "");
        assert_eq!(balance_label(0.003), "");
    }

    #[test]
    fn off_center_balance_names_the_boosted_side() {
        assert_eq!(balance_label(-0.25), "L  25%");
        assert_eq!(balance_label(0.5), "R  50%");
    }
}
