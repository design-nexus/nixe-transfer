//! Transfer's preferences and its settings card: where browsing starts, how
//! failed transfers are retried, then the suite's standard groups.

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
    /// Where local panes start; empty is the home folder.
    pub start_folder: String,
    pub show_hidden: bool,
    /// `name`, `size` or `modified`.
    pub sort: String,
    /// How often a transfer that failed on the network is tried again. The
    /// transfer service reads it from this file.
    pub retries: u32,
    pub concurrent_transfers: u32,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            mode: nyx_theme::ThemeMode::Omarchy,
            theme: "tokyo-night".into(),
            reduce_motion: false,
            start_folder: String::new(),
            show_hidden: false,
            sort: "name".into(),
            retries: 2,
            concurrent_transfers: 2,
        }
    }
}

/// The folder local panes open on.
pub fn start_folder() -> std::path::PathBuf {
    let folder = get().start_folder;
    if !folder.is_empty() && std::path::Path::new(&folder).is_dir() {
        return folder.into();
    }
    std::env::var("HOME").unwrap_or_else(|_| "/".into()).into()
}

/// The sort list's position for the saved sort.
pub fn sort_index() -> u32 {
    match get().sort.as_str() {
        "size" => 1,
        "modified" => 2,
        _ => 0,
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
    groups: &[
        (
            "Browsing",
            "folder-symbolic",
            "Start folder, hidden files, sorting",
        ),
        (
            "Transfers",
            "folder-download-symbolic",
            "Queue size, retrying failed transfers",
        ),
    ],
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
    nyx_theme::ThemePrefs {
        mode: p.mode,
        theme: p.theme,
        glow: false,
        reduce_motion: p.reduce_motion,
    }
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
    let Some((overlay, window)) = WINDOW.with(|w| w.borrow().clone()) else {
        return;
    };
    settings_card::open(&overlay, &window, SPEC, || {
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.add_css_class("settings-page");
        build(&body);
        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::External)
            .vexpand(true)
            .child(&body)
            .build();
        (root.upcast(), body)
    });
}

fn group(body: &gtk::Box, title: &str) -> gtk::Box {
    let wrapper = gtk::Box::new(gtk::Orientation::Vertical, 0);
    wrapper.add_css_class("settings-group");
    wrapper.set_widget_name(title);
    let heading = gtk::Label::new(Some(&title.to_uppercase()));
    heading.add_css_class("group-title");
    heading.set_xalign(0.0);
    wrapper.append(&heading);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.add_css_class("group-list");
    wrapper.append(&list);
    body.append(&wrapper);
    list
}

fn opts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect()
}

fn build(body: &gtk::Box) {
    use nyx_ui::widgets;
    let p = get();

    let g = group(body, "Browsing");
    let folder = gtk::Button::with_label(&folder_label(&p.start_folder));
    folder.set_valign(gtk::Align::Center);
    folder.connect_clicked(|button| {
        let dialog = gtk::FileDialog::builder()
            .title("Start in")
            .modal(true)
            .build();
        dialog.set_initial_folder(Some(&gtk::gio::File::for_path(start_folder())));
        let window = button.root().and_downcast::<gtk::Window>();
        let button = button.clone();
        dialog.select_folder(
            window.as_ref(),
            gtk::gio::Cancellable::NONE,
            move |result| {
                if let Some(path) = result.ok().and_then(|file| file.path()) {
                    let text = path.to_string_lossy().into_owned();
                    button.set_label(&folder_label(&text));
                    update(|p| p.start_folder = text);
                }
            },
        );
    });
    g.append(&widgets::row(
        "Start in",
        "The folder local panes open on.",
        Some(folder.upcast_ref()),
    ));
    let (row, _) = widgets::switch_row(
        "Show hidden files",
        "Files and folders whose names start with a dot. Ctrl+H switches the active pane.",
        p.show_hidden,
        |on| update(|p| p.show_hidden = on),
    );
    g.append(&row);
    g.append(&widgets::segmented_row(
        "Sort by",
        "How new panes order their files.",
        opts(&[("name", "Name"), ("size", "Size"), ("modified", "Modified")]),
        &p.sort,
        |id| update(|p| p.sort = id),
    ));

    let g = group(body, "Transfers");
    let (row, _) = widgets::choice_row(
        "Transfers at once",
        "Maximum active jobs. Takes effect when the transfer service next starts.",
        opts(&[("1", "1"), ("2", "2"), ("3", "3"), ("4", "4")]),
        &p.concurrent_transfers.to_string(),
        |id| update(|p| p.concurrent_transfers = id.parse().unwrap_or(2)),
    );
    g.append(&row);
    let (row, _) = widgets::choice_row(
        "Try again after a network error",
        "Each try waits twice as long as the one before.",
        opts(&[
            ("0", "Never"),
            ("1", "Once"),
            ("2", "Twice"),
            ("3", "3 times"),
            ("5", "5 times"),
        ]),
        &p.retries.to_string(),
        |id| update(|p| p.retries = id.parse().unwrap_or(2)),
    );
    g.append(&row);
}

fn folder_label(folder: &str) -> String {
    if folder.is_empty() {
        return "Home".into();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    match folder.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => folder.to_string(),
    }
}
