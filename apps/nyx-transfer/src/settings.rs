//! Transfer's preferences (theme and motion) and its settings card, which holds
//! only the suite's standard groups.

use gtk::prelude::*;
use nyx_ui::settings_card::{self, About, Spec, Standard};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub mode: nyx_theme::ThemeMode,
    pub theme: String,
    pub reduce_motion: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { mode: nyx_theme::ThemeMode::Omarchy, theme: "tokyo-night".into(), reduce_motion: false }
    }
}

mod store {
    nyx_core::prefs_store!(super::Prefs);
}
pub use store::{get, update};

/// Every shortcut, for the card's Keyboard group and the F1 list.
pub const SHORTCUTS: &[(&[&str], &str)] = &[
    (&["Ctrl", "T"], "New tab"),
    (&["Ctrl", "W"], "Close tab"),
    (&["Ctrl", "Tab"], "Next tab"),
    (&["Ctrl", "F"], "Filter the active pane"),
    (&["Ctrl", "L"], "Edit the location"),
    (&["Alt", "←"], "Back"),
    (&["Alt", "→"], "Forward"),
    (&["Ctrl", "H"], "Show or hide hidden files"),
    (&["Ctrl", "R"], "Refresh"),
    (&["Ctrl", ","], "Settings"),
    (&["Ctrl", "Q"], "Close Transfer"),
];

const SPEC: Spec = Spec {
    groups: &[],
    search_text: None,
    standard: Some(Standard {
        app: "transfer",
        about: About {
            name: "Transfer",
            icon: "io.github.design_nexus.nyx.Transfer",
            version: env!("CARGO_PKG_VERSION"),
            description: "Copy and sync files between this computer and your servers.",
            licence: "MIT",
            website: "https://github.com/design-nexus/nyx-transfer",
            issues: "https://github.com/design-nexus/nyx-transfer/issues",
            credits: &[("rclone", "transfers")],
        },
        shortcuts: SHORTCUTS,
        reduce_motion: || get().reduce_motion,
        set_reduce_motion: |on| update(|p| p.reduce_motion = on),
    }),
};

/// The theme engine's view of these preferences.
pub fn style_prefs() -> nyx_theme::ThemePrefs {
    let p = get();
    nyx_theme::ThemePrefs { mode: p.mode, theme: p.theme, glow: false, reduce_motion: p.reduce_motion }
}

pub fn set_theme(mode: nyx_theme::ThemeMode, theme: String) {
    update(|p| {
        p.mode = mode;
        p.theme = theme;
    });
}

thread_local! {
    static WINDOW: RefCell<Option<(gtk::Overlay, gtk::ApplicationWindow)>> = const { RefCell::new(None) };
}

/// Where the card opens: the overlay over the window's content.
pub fn attach(overlay: &gtk::Overlay, window: &gtk::ApplicationWindow) {
    WINDOW.with(|w| *w.borrow_mut() = Some((overlay.clone(), window.clone())));
    window.connect_default_width_notify(settings_card::fit);
    window.connect_default_height_notify(settings_card::fit);
}

pub fn open() {
    let Some((overlay, window)) = WINDOW.with(|w| w.borrow().clone()) else { return };
    settings_card::open(&overlay, &window, SPEC, || {
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.add_css_class("settings-page");
        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::External)
            .vexpand(true)
            .child(&body)
            .build();
        (root.upcast(), body)
    });
}
