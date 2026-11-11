//! Framewise – desktop entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use eframe::egui;
use framewise::core::backend;
use framewise::core::config::{AppConfig, AppDirs};
use framewise::core::logging;
use framewise::gui::FramewiseApp;
use framewise::i18n::{self, Language};
use framewise::persistence::Database;

fn main() -> eframe::Result {
    let dirs = AppDirs::resolve();
    logging::init(&dirs.log_dir());
    tracing::info!("Framewise {} starting", env!("CARGO_PKG_VERSION"));

    let config = match AppConfig::load(&dirs.config_file) {
        Ok(c) => c,
        Err(e) => {
            // Keep the broken file untouched; it is only overwritten when the
            // user saves the settings.
            tracing::error!("{e} – using defaults");
            AppConfig::default()
        }
    };
    i18n::set_language(config.ui.language.unwrap_or_else(Language::from_env));

    let db = match Database::open(&dirs.database_file()) {
        Ok(db) => db,
        Err(e) => {
            tracing::error!("database unavailable ({e}) – using in-memory database");
            Database::open_in_memory().expect("in-memory SQLite")
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Framewise – Media Optimizer")
            .with_app_id("framewise")
            .with_inner_size([1440.0, 880.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Framewise",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let notify: backend::Notify = Arc::new(move || ctx.request_repaint());
            let handle = backend::spawn(dirs, config.clone(), db, notify);
            Ok(Box::new(FramewiseApp::new(cc, handle, config)))
        }),
    )
}
