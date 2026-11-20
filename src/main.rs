#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod state;
mod ui;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Volux")
            .with_inner_size(egui::vec2(590.0, 720.0))
            .with_min_inner_size(egui::vec2(500.0, 560.0))
            .with_resizable(true)
            .with_icon(ui::window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "Volux",
        options,
        Box::new(|_cc| Ok(Box::new(ui::MixerApp::new(audio::start())))),
    )
}
