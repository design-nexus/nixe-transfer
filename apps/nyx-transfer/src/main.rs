use gtk::prelude::*;
use gtk::{gio, glib};
use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, sync::Arc, time::Duration};
use transfer_core::{
    Auth, ConnectionProfile, FileEntry, JobSnapshot, JobStatus, Location, Protocol, StorageBackend,
    SyncAction, SyncMode, SyncPreview,
};
use transfer_storage as store;
mod workflows;

const APP_ID: &str = "io.github.design_nexus.nyx.Transfer";
const CONTROL_SIZE: i32 = 32;
/// The gap between the two panes, which holds the copy buttons.
const COPY_GAP: i32 = 76;

fn square_control(widget: &impl IsA<gtk::Widget>) {
    widget.add_css_class("square-button");
    widget.set_size_request(CONTROL_SIZE, CONTROL_SIZE);
    widget.set_halign(gtk::Align::Center);
    widget.set_valign(gtk::Align::Center);
}

struct Pane {
    location: Location,
    history: Vec<Location>,
    history_position: usize,
    entries: Vec<FileEntry>,
    selector: gtk::DropDown,
    breadcrumb: gtk::Box,
    path: gtk::Entry,
    path_stack: gtk::Stack,
    filter: gtk::SearchEntry,
    filter_revealer: gtk::Revealer,
    sort: gtk::DropDown,
    hidden: gtk::CheckButton,
    list: gtk::ListBox,
    back: gtk::Button,
    forward: gtk::Button,
    up: gtk::Button,
    choose_local: gtk::Button,
    refresh: gtk::Button,
}
#[derive(Clone)]
struct TabSnapshot {
    locations: [Location; 2],
    histories: [Vec<Location>; 2],
    positions: [usize; 2],
    filters: [String; 2],
    sorts: [u32; 2],
    hidden: [bool; 2],
}
struct State {
    panes: Vec<Pane>,
    tabs: Vec<TabSnapshot>,
    active_tab: usize,
    active_pane: usize,
    restoring_tab: bool,
    tab_strip: gtk::Box,
    profiles: Vec<ConnectionProfile>,
    queue: gtk::Box,
    empty_queue: gtk::Label,
    queue_revealer: gtk::Revealer,
    queue_toggle: gtk::Button,
    clear_completed: gtk::Button,
    activity: gtk::Label,
    activity_spinner: gtk::Spinner,
    status: gtk::Label,
    jobs: HashMap<String, JobView>,
    runtime: Arc<tokio::runtime::Runtime>,
    client: Arc<tokio::sync::OnceCell<transfer_ipc::Client>>,
    sender: std::sync::mpsc::Sender<Message>,
    sync_dialog: Option<SyncDialog>,
    workflow_dialog: Option<workflows::WorkflowDialog>,
}
struct SyncDialog {
    window: gtk::Window,
    request_id: String,
    source: Location,
    destination: Location,
    mode: SyncMode,
    preview: Option<SyncPreview>,
    report: gtk::TextView,
    status: gtk::Label,
    queue: gtk::Button,
}
struct JobView {
    row: gtk::Box,
    status: gtk::Label,
    detail: gtk::Label,
    progress: gtk::ProgressBar,
    retry: gtk::Button,
    pause: gtk::Button,
    cancel: gtk::Button,
    state: JobStatus,
}
enum Message {
    Listed(usize, Location, Result<Vec<FileEntry>, String>),
    Jobs(Result<Vec<JobSnapshot>, String>),
    QueueResult(Result<(), String>),
    ControlResult(Result<(), String>),
    Saved(Result<ConnectionProfile, String>),
    Remotes(Result<(PathBuf, Vec<transfer_rclone::AvailableRemote>), String>),
    SyncPreview(String, Result<SyncPreview, String>),
    WorkflowPreview(String, Result<transfer_core::WorkflowPreview, String>),
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();
    nyx_core::app::init(nyx_core::app::App {
        slug: "nyx-transfer",
        name: "Transfer",
    });
    // Developer aid: a different id gives a separate instance (a test copy on a virtual
    // monitor), so it doesn't hand its arguments to the copy that's open.
    let id = std::env::var("NYX_TRANSFER_APP_ID").unwrap_or_else(|_| APP_ID.to_string());
    let app = gtk::Application::builder().application_id(id).build();
    app.connect_activate(build);
    app.run()
}

fn build(app: &gtk::Application) {
    nyx_theme::use_design_2();
    nyx_theme::install(nyx_theme::Style {
        window_class: "transfer-window",
        base_skip: &[],
        css: include_str!("style.css"),
        no_glow_css: "",
        extra_colors: None,
        prefs: || nyx_theme::ThemePrefs {
            mode: nyx_theme::ThemeMode::Omarchy,
            theme: "tokyo-night".into(),
            glow: false,
            reduce_motion: false,
        },
        set_theme: None,
    });
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Transfer")
        .default_width(1280)
        .default_height(800)
        .build();
    window.add_css_class("transfer-window");
    window.set_titlebar(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    window.set_icon_name(Some(APP_ID));
    nyx_ui::keys::install(
        &window,
        nyx_ui::keys::Common {
            close: false,
            ..Default::default()
        },
    );
    let profiles = store::load().unwrap_or_default();
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
            .expect("Tokio runtime"),
    );
    let (sender, receiver) = std::sync::mpsc::channel();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("window-frame");
    // One top bar: the app and its tabs, then the copy, connection and search actions.
    let header = nyx_ui::chrome::app_header(&window, "folder-remote-symbolic", "Transfer");
    let toolbar = header.end.clone();
    toolbar.set_spacing(2);
    let activity_spinner = gtk::Spinner::new();
    let activity = gtk::Label::new(Some("No active transfers"));
    activity.add_css_class("dim-label");
    let tab_strip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    tab_strip.add_css_class("transfer-tabs");
    let tab_scroll = gtk::ScrolledWindow::builder()
        .child(&tab_strip)
        .propagate_natural_width(true)
        .max_content_width(640)
        .build();
    tab_scroll.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
    tab_scroll.set_valign(gtk::Align::Center);
    tab_strip.set_valign(gtk::Align::Center);
    header.start.append(&tab_scroll);
    // Copy between the panes, in the gap between them.
    let to_right = gtk::Button::from_icon_name("go-next-symbolic");
    to_right.add_css_class("copy-button");
    to_right.set_tooltip_text(Some("Copy selected items from left to right"));
    let to_left = gtk::Button::from_icon_name("go-previous-symbolic");
    to_left.add_css_class("copy-button");
    to_left.set_tooltip_text(Some("Copy selected items from right to left"));
    let copy_column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    copy_column.add_css_class("copy-column");
    copy_column.set_halign(gtk::Align::Center);
    copy_column.set_valign(gtk::Align::Center);
    copy_column.append(&to_right);
    copy_column.append(&to_left);
    let new_connection = gtk::Button::from_icon_name("network-server-symbolic");
    square_control(&new_connection);
    new_connection.set_tooltip_text(Some("Create a server connection"));
    toolbar.append(&new_connection);
    let import_remote = gtk::Button::from_icon_name("document-open-symbolic");
    square_control(&import_remote);
    import_remote.set_tooltip_text(Some("Import a remote from the user rclone configuration"));
    toolbar.append(&import_remote);
    let sync_folders = gtk::Button::from_icon_name("emblem-synchronizing-symbolic");
    square_control(&sync_folders);
    sync_folders.set_tooltip_text(Some("Synchronize the current pane locations"));
    toolbar.append(&sync_folders);
    let workflows = gtk::Button::from_icon_name("view-list-symbolic");
    square_control(&workflows);
    workflows.set_tooltip_text(Some("Saved workflows"));
    toolbar.append(&workflows);
    let search_button = gtk::Button::from_icon_name("system-search-symbolic");
    square_control(&search_button);
    search_button.set_tooltip_text(Some("Search the active pane (Ctrl+F)"));
    toolbar.append(&search_button);
    root.append(&header.root);
    // Two equal panes with a gap between them for the copy buttons.
    let browser = gtk::Box::new(gtk::Orientation::Horizontal, COPY_GAP);
    browser.add_css_class("transfer-browser");
    browser.set_hexpand(true);
    browser.set_vexpand(true);
    browser.set_homogeneous(true);
    let (left_ui, left) = make_pane(&profiles);
    let (right_ui, right) = make_pane(&profiles);
    browser.append(&left_ui);
    browser.append(&right_ui);
    let browser_overlay = gtk::Overlay::new();
    browser_overlay.set_child(Some(&browser));
    browser_overlay.add_overlay(&copy_column);
    root.append(&browser_overlay);
    let queue_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    queue_header.add_css_class("queue-head");
    let section = gtk::Label::new(Some("TRANSFERS"));
    section.add_css_class("dim-label");
    queue_header.append(&section);
    let queue_toggle = gtk::Button::from_icon_name("pan-end-symbolic");
    square_control(&queue_toggle);
    queue_header.append(&queue_toggle);
    // What the panes are doing ("67 items", errors), as quiet text beside the queue.
    let status = gtk::Label::new(Some("Choose a location in each pane."));
    status.add_css_class("transfer-status");
    status.set_xalign(0.0);
    status.set_hexpand(true);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    queue_header.append(&status);
    queue_header.append(&activity_spinner);
    queue_header.append(&activity);
    let clear_completed = gtk::Button::from_icon_name("edit-clear-symbolic");
    square_control(&clear_completed);
    clear_completed.set_tooltip_text(Some("Clear completed transfers"));
    clear_completed.set_sensitive(false);
    queue_header.append(&clear_completed);
    root.append(&queue_header);
    let queue = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let empty_queue = gtk::Label::new(Some("No transfers yet"));
    empty_queue.add_css_class("queue-empty");
    empty_queue.set_xalign(0.0);
    queue.append(&empty_queue);
    let queue_scroll = gtk::ScrolledWindow::builder()
        .child(&queue)
        .min_content_height(200)
        .max_content_height(320)
        .vexpand(false)
        .build();
    queue_scroll.add_css_class("transfer-queue-view");
    queue_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
    let queue_revealer = gtk::Revealer::new();
    queue_revealer.set_reveal_child(false);
    queue_revealer.set_child(Some(&queue_scroll));
    root.append(&queue_revealer);
    window.set_child(Some(&root));
    let panes = vec![left, right];
    let first_tab = TabSnapshot::from_panes(&panes);
    let state = Rc::new(RefCell::new(State {
        panes,
        tabs: vec![first_tab],
        active_tab: 0,
        active_pane: 0,
        restoring_tab: false,
        tab_strip,
        profiles,
        queue,
        empty_queue,
        queue_revealer: queue_revealer.clone(),
        queue_toggle: queue_toggle.clone(),
        clear_completed: clear_completed.clone(),
        activity,
        activity_spinner,
        status,
        jobs: HashMap::new(),
        runtime,
        client: Arc::new(tokio::sync::OnceCell::new()),
        sender,
        sync_dialog: None,
        workflow_dialog: None,
    }));
    for index in 0..2 {
        connect_pane(state.clone(), index);
        refresh(state.clone(), index);
    }
    render_tabs(state.clone());
    {
        let revealer = queue_revealer.clone();
        queue_toggle.connect_clicked(move |button| {
            let show = !revealer.reveals_child();
            revealer.set_reveal_child(show);
            button.set_icon_name(if show {
                "pan-down-symbolic"
            } else {
                "pan-end-symbolic"
            });
        });
    }
    {
        let s = state.clone();
        search_button.connect_clicked(move |_| {
            let index = s.borrow().active_pane;
            focus_filter(&s, index);
        });
    }
    {
        let s = state.clone();
        let w = window.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let ctrl = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            let alt = modifiers.contains(gtk::gdk::ModifierType::ALT_MASK);
            match key {
                gtk::gdk::Key::q if ctrl => w.close(),
                gtk::gdk::Key::w if ctrl => {
                    if s.borrow().tabs.len() > 1 {
                        let active = s.borrow().active_tab;
                        close_tab(s.clone(), active);
                    } else {
                        w.close();
                    }
                }
                gtk::gdk::Key::t if ctrl => new_tab(s.clone()),
                gtk::gdk::Key::Tab if ctrl => {
                    let next = {
                        let state = s.borrow();
                        (state.active_tab + 1) % state.tabs.len()
                    };
                    switch_tab(s.clone(), next);
                }
                gtk::gdk::Key::f if ctrl => {
                    let active = s.borrow().active_pane;
                    focus_filter(&s, active);
                }
                gtk::gdk::Key::l if ctrl => {
                    let active = s.borrow().active_pane;
                    focus_path(&s, active);
                }
                gtk::gdk::Key::h if ctrl => {
                    let active = s.borrow().active_pane;
                    let hidden = s.borrow().panes[active].hidden.clone();
                    hidden.set_active(!hidden.is_active());
                }
                gtk::gdk::Key::r if ctrl => {
                    let active = s.borrow().active_pane;
                    refresh(s.clone(), active);
                }
                gtk::gdk::Key::Left if alt => {
                    let active = s.borrow().active_pane;
                    go_history(s.clone(), active, -1);
                }
                gtk::gdk::Key::Right if alt => {
                    let active = s.borrow().active_pane;
                    go_history(s.clone(), active, 1);
                }
                gtk::gdk::Key::F5 => {
                    refresh(s.clone(), 0);
                    refresh(s.clone(), 1);
                }
                gtk::gdk::Key::F1 => show_shortcuts(&w),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        window.add_controller(keys);
    }
    {
        let s = state.clone();
        to_right.connect_clicked(move |_| queue_selected(s.clone(), 0, 1));
    }
    {
        let s = state.clone();
        to_left.connect_clicked(move |_| queue_selected(s.clone(), 1, 0));
    }
    {
        let s = state.clone();
        let w = window.clone();
        new_connection.connect_clicked(move |_| connection_dialog(&w, s.clone()));
    }
    {
        let s = state.clone();
        import_remote.connect_clicked(move |_| request_remotes(s.clone()));
    }
    {
        let s = state.clone();
        let w = window.clone();
        sync_folders.connect_clicked(move |_| sync_dialog(&w, s.clone()));
    }
    {
        let s = state.clone();
        let w = window.clone();
        workflows.connect_clicked(move |_| workflows::manager(&w, s.clone()));
    }
    {
        let s = state.clone();
        clear_completed.connect_clicked(move |_| control_job(s.clone(), String::new(), "clear"));
    }
    request_jobs(state.clone());
    {
        let s = state.clone();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            request_jobs(s.clone());
            glib::ControlFlow::Continue
        });
    }
    glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(message) = receiver.try_recv() {
            handle_message(state.clone(), message);
        }
        glib::ControlFlow::Continue
    });
    window.present();
}

fn show_shortcuts(window: &gtk::ApplicationWindow) {
    gtk::AlertDialog::builder()
        .message("Transfer shortcuts")
        .detail("Ctrl+T  New tab\nCtrl+W  Close tab\nCtrl+Tab  Next tab\nCtrl+F  Filter active pane\nCtrl+L  Edit location\nAlt+Left/Right  Browse history\nCtrl+H  Toggle hidden files\nCtrl+R / F5  Refresh\nCtrl+Q  Close Transfer")
        .buttons(["Close"])
        .build()
        .show(Some(window));
}

fn focus_filter(state: &Rc<RefCell<State>>, index: usize) {
    let state = state.borrow();
    let pane = &state.panes[index];
    pane.filter_revealer.set_reveal_child(true);
    pane.filter.grab_focus();
}

fn focus_path(state: &Rc<RefCell<State>>, index: usize) {
    let state = state.borrow();
    let pane = &state.panes[index];
    pane.path_stack.set_visible_child_name("entry");
    pane.path.grab_focus();
    pane.path.select_region(0, -1);
}

impl TabSnapshot {
    fn from_panes(panes: &[Pane]) -> Self {
        Self {
            locations: [panes[0].location.clone(), panes[1].location.clone()],
            histories: [panes[0].history.clone(), panes[1].history.clone()],
            positions: [panes[0].history_position, panes[1].history_position],
            filters: [
                panes[0].filter.text().to_string(),
                panes[1].filter.text().to_string(),
            ],
            sorts: [panes[0].sort.selected(), panes[1].sort.selected()],
            hidden: [panes[0].hidden.is_active(), panes[1].hidden.is_active()],
        }
    }
}

fn tab_location_label(location: &Location, profiles: &[ConnectionProfile]) -> String {
    let label = match location {
        Location::Local(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "Local".into()),
        Location::Remote { profile_id, .. } => profiles
            .iter()
            .find(|p| &p.id == profile_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Remote".into()),
    };
    if label.chars().count() > 18 {
        format!("{}…", label.chars().take(17).collect::<String>())
    } else {
        label
    }
}

fn render_tabs(state: Rc<RefCell<State>>) {
    let (strip, tabs, active, profiles) = {
        let s = state.borrow();
        (
            s.tab_strip.clone(),
            s.tabs.clone(),
            s.active_tab,
            s.profiles.clone(),
        )
    };
    while let Some(child) = strip.first_child() {
        strip.remove(&child);
    }
    for (index, tab) in tabs.iter().enumerate() {
        let group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        group.add_css_class("transfer-tab");
        group.set_valign(gtk::Align::Center);
        if index == active {
            group.add_css_class("active");
        }
        let label = format!(
            "{}  →  {}",
            tab_location_label(&tab.locations[0], &profiles),
            tab_location_label(&tab.locations[1], &profiles)
        );
        let select = gtk::Button::with_label(&label);
        select.set_tooltip_text(Some(&format!(
            "{} → {}",
            location_name(&tab.locations[0], &profiles),
            location_name(&tab.locations[1], &profiles)
        )));
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("tab-close");
        close.set_valign(gtk::Align::Center);
        close.set_tooltip_text(Some("Close tab"));
        close.set_sensitive(tabs.len() > 1);
        group.append(&select);
        group.append(&close);
        strip.append(&group);
        {
            let s = state.clone();
            select.connect_clicked(move |_| switch_tab(s.clone(), index));
        }
        {
            let s = state.clone();
            close.connect_clicked(move |_| close_tab(s.clone(), index));
        }
    }
    let add = gtk::Button::from_icon_name("list-add-symbolic");
    add.add_css_class("tab-add");
    add.set_valign(gtk::Align::Center);
    add.set_tooltip_text(Some("New tab (Ctrl+T)"));
    strip.append(&add);
    add.connect_clicked(move |_| new_tab(state.clone()));
}

fn save_active_tab(state: &mut State) {
    let snapshot = TabSnapshot::from_panes(&state.panes);
    state.tabs[state.active_tab] = snapshot;
}

fn restore_tab(state: Rc<RefCell<State>>, index: usize) {
    let (tab, selectors, filters, sorts, hidden, profiles) = {
        let mut s = state.borrow_mut();
        if index >= s.tabs.len() {
            return;
        }
        s.active_tab = index;
        s.active_pane = 0;
        s.restoring_tab = true;
        (
            s.tabs[index].clone(),
            [s.panes[0].selector.clone(), s.panes[1].selector.clone()],
            [s.panes[0].filter.clone(), s.panes[1].filter.clone()],
            [s.panes[0].sort.clone(), s.panes[1].sort.clone()],
            [s.panes[0].hidden.clone(), s.panes[1].hidden.clone()],
            s.profiles.clone(),
        )
    };
    for pane_index in 0..2 {
        let selected = match &tab.locations[pane_index] {
            Location::Local(_) => 0,
            Location::Remote { profile_id, .. } => profiles
                .iter()
                .position(|p| &p.id == profile_id)
                .map(|i| i as u32 + 1)
                .unwrap_or(0),
        };
        selectors[pane_index].set_selected(selected);
        filters[pane_index].set_text(&tab.filters[pane_index]);
        sorts[pane_index].set_selected(tab.sorts[pane_index]);
        hidden[pane_index].set_active(tab.hidden[pane_index]);
    }
    {
        let mut s = state.borrow_mut();
        for pane_index in 0..2 {
            s.panes[pane_index].location = tab.locations[pane_index].clone();
            s.panes[pane_index].history = tab.histories[pane_index].clone();
            s.panes[pane_index].history_position = tab.positions[pane_index];
            s.panes[pane_index].entries.clear();
        }
        s.restoring_tab = false;
    }
    for pane_index in 0..2 {
        render_pane(&state, pane_index);
        refresh(state.clone(), pane_index);
    }
    render_tabs(state);
}

fn switch_tab(state: Rc<RefCell<State>>, index: usize) {
    if index == state.borrow().active_tab {
        return;
    }
    save_active_tab(&mut state.borrow_mut());
    restore_tab(state, index);
}

fn new_tab(state: Rc<RefCell<State>>) {
    let index = {
        let mut s = state.borrow_mut();
        save_active_tab(&mut s);
        let copy = s.tabs[s.active_tab].clone();
        s.tabs.push(copy);
        s.tabs.len() - 1
    };
    restore_tab(state, index);
}

fn close_tab(state: Rc<RefCell<State>>, index: usize) {
    let next = {
        let mut s = state.borrow_mut();
        if s.tabs.len() <= 1 || index >= s.tabs.len() {
            return;
        }
        save_active_tab(&mut s);
        s.tabs.remove(index);
        if index < s.active_tab {
            s.active_tab -= 1;
        } else if index == s.active_tab {
            s.active_tab = s.active_tab.min(s.tabs.len() - 1);
        }
        s.active_tab
    };
    restore_tab(state, next);
}

fn go_history(state: Rc<RefCell<State>>, index: usize, offset: isize) {
    let changed = {
        let mut s = state.borrow_mut();
        let pane = &mut s.panes[index];
        let next = pane.history_position as isize + offset;
        if next < 0 || next >= pane.history.len() as isize {
            false
        } else {
            pane.history_position = next as usize;
            pane.location = pane.history[pane.history_position].clone();
            save_active_tab(&mut s);
            true
        }
    };
    if changed {
        refresh(state.clone(), index);
        render_tabs(state);
    }
}

fn make_pane(profiles: &[ConnectionProfile]) -> (gtk::Box, Pane) {
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 0);
    box_.add_css_class("transfer-pane");
    box_.set_hexpand(true);
    box_.set_vexpand(true);
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    controls.add_css_class("pane-controls");
    let mut names = vec!["Local".to_owned()];
    names.extend(
        profiles
            .iter()
            .map(|p| format!("{} · {}", p.name, p.protocol.label())),
    );
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let selector = gtk::DropDown::from_strings(&refs);
    selector.add_css_class("location-selector");
    selector.set_hexpand(true);
    selector.set_valign(gtk::Align::Center);
    controls.append(&selector);
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    square_control(&back);
    back.set_tooltip_text(Some("Back"));
    back.set_sensitive(false);
    let forward = gtk::Button::from_icon_name("go-next-symbolic");
    square_control(&forward);
    forward.set_tooltip_text(Some("Forward"));
    forward.set_sensitive(false);
    controls.append(&back);
    controls.append(&forward);
    let up = gtk::Button::from_icon_name("go-up-symbolic");
    square_control(&up);
    up.set_tooltip_text(Some("Parent directory"));
    let choose_local = gtk::Button::with_label("Open local folder…");
    choose_local.set_tooltip_text(Some("Open a local folder"));
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    square_control(&refresh);
    refresh.set_tooltip_text(Some("Refresh"));
    controls.append(&up);
    controls.append(&refresh);
    let sort = gtk::DropDown::from_strings(&["Name", "Size", "Modified"]);
    sort.set_tooltip_text(Some("Sort directory"));
    let hidden = gtk::CheckButton::with_label("Show hidden files");
    let view_options = gtk::Box::new(gtk::Orientation::Vertical, 8);
    view_options.add_css_class("view-options");
    view_options.append(&gtk::Label::new(Some("Sort by")));
    view_options.append(&sort);
    view_options.append(&hidden);
    view_options.append(&choose_local);
    let view_popover = gtk::Popover::new();
    view_popover.set_child(Some(&view_options));
    let view_menu = gtk::MenuButton::new();
    view_menu.set_icon_name("view-more-symbolic");
    square_control(&view_menu);
    view_menu.set_tooltip_text(Some("Sort and view options"));
    view_menu.set_popover(Some(&view_popover));
    controls.append(&view_menu);
    box_.append(&controls);
    let breadcrumb = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    breadcrumb.add_css_class("transfer-breadcrumb");
    let crumbs_scroll = gtk::ScrolledWindow::builder()
        .child(&breadcrumb)
        .hexpand(true)
        .min_content_height(32)
        .build();
    crumbs_scroll.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
    let path_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    path_row.add_css_class("pane-path-row");
    let path = gtk::Entry::new();
    path.set_hexpand(true);
    path.set_valign(gtk::Align::Center);
    path.set_text(&std::env::var("HOME").unwrap_or_else(|_| "/".into()));
    let path_stack = gtk::Stack::new();
    path_stack.set_hexpand(true);
    path_stack.add_named(&crumbs_scroll, Some("breadcrumbs"));
    path_stack.add_named(&path, Some("entry"));
    path_stack.set_visible_child_name("breadcrumbs");
    path_row.append(&path_stack);
    let edit_path = gtk::Button::from_icon_name("document-edit-symbolic");
    square_control(&edit_path);
    edit_path.set_tooltip_text(Some("Edit location (Ctrl+L)"));
    path_row.append(&edit_path);
    {
        let stack = path_stack.clone();
        let entry = path.clone();
        edit_path.connect_clicked(move |_| {
            stack.set_visible_child_name("entry");
            entry.grab_focus();
            entry.select_region(0, -1);
        });
    }
    box_.append(&path_row);
    let filter_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    filter_row.add_css_class("pane-search-row");
    let filter = gtk::SearchEntry::new();
    filter.set_placeholder_text(Some("Filter files"));
    filter.set_hexpand(true);
    filter_row.append(&filter);
    let close_filter = gtk::Button::from_icon_name("window-close-symbolic");
    square_control(&close_filter);
    close_filter.set_tooltip_text(Some("Close filter"));
    filter_row.append(&close_filter);
    let filter_revealer = gtk::Revealer::new();
    filter_revealer.set_child(Some(&filter_row));
    filter_revealer.set_reveal_child(false);
    {
        let revealer = filter_revealer.clone();
        let entry = filter.clone();
        close_filter.connect_clicked(move |_| {
            entry.set_text("");
            revealer.set_reveal_child(false);
        });
    }
    box_.append(&filter_revealer);
    let headings = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    headings.add_css_class("file-headings");
    let name_heading = gtk::Label::new(Some("NAME"));
    name_heading.set_xalign(0.0);
    name_heading.set_margin_start(24);
    name_heading.set_hexpand(true);
    headings.append(&name_heading);
    let size_heading = gtk::Label::new(Some("SIZE"));
    size_heading.set_width_chars(8);
    size_heading.set_xalign(1.0);
    headings.append(&size_heading);
    box_.append(&headings);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Multiple);
    list.add_css_class("file-list");
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .build();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
    box_.append(&scroll);
    let initial_location = Location::Local(PathBuf::from(path.text().as_str()));
    (
        box_,
        Pane {
            location: initial_location.clone(),
            history: vec![initial_location],
            history_position: 0,
            entries: Vec::new(),
            selector,
            breadcrumb,
            path,
            path_stack,
            filter,
            filter_revealer,
            sort,
            hidden,
            list,
            back,
            forward,
            up,
            choose_local,
            refresh,
        },
    )
}

fn connect_pane(state: Rc<RefCell<State>>, index: usize) {
    let (selector, path, filter, sort, hidden, list, back, forward, up, choose_local, reload) = {
        let s = state.borrow();
        let p = &s.panes[index];
        (
            p.selector.clone(),
            p.path.clone(),
            p.filter.clone(),
            p.sort.clone(),
            p.hidden.clone(),
            p.list.clone(),
            p.back.clone(),
            p.forward.clone(),
            p.up.clone(),
            p.choose_local.clone(),
            p.refresh.clone(),
        )
    };
    {
        let s = state.clone();
        selector.connect_selected_notify(move |selector| {
            if s.borrow().restoring_tab {
                return;
            }
            let selected = selector.selected();
            let location = if selected == 0 {
                Location::Local(PathBuf::from(
                    std::env::var("HOME").unwrap_or_else(|_| "/".into()),
                ))
            } else {
                let state = s.borrow();
                let Some(p) = state.profiles.get(selected as usize - 1) else {
                    return;
                };
                Location::Remote {
                    profile_id: p.id.clone(),
                    path: p.default_dir.trim_start_matches('/').to_owned(),
                }
            };
            navigate(s.clone(), index, location);
        });
    }
    {
        let s = state.clone();
        path.connect_activate(move |entry| {
            let text = entry.text().to_string();
            let location = match &s.borrow().panes[index].location {
                Location::Local(_) => Location::Local(PathBuf::from(text)),
                Location::Remote { profile_id, .. } => Location::Remote {
                    profile_id: profile_id.clone(),
                    path: text.trim_start_matches('/').to_owned(),
                },
            };
            navigate(s.clone(), index, location);
            s.borrow().panes[index]
                .path_stack
                .set_visible_child_name("breadcrumbs");
        });
    }
    {
        let stack = state.borrow().panes[index].path_stack.clone();
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                stack.set_visible_child_name("breadcrumbs");
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        path.add_controller(keys);
    }
    {
        let s = state.clone();
        filter.connect_search_changed(move |_| render_pane(&s, index));
    }
    {
        let s = state.clone();
        sort.connect_selected_notify(move |_| render_pane(&s, index));
    }
    {
        let s = state.clone();
        hidden.connect_toggled(move |_| render_pane(&s, index));
    }
    {
        let focus = gtk::EventControllerFocus::new();
        let s = state.clone();
        focus.connect_enter(move |_| {
            s.borrow_mut().active_pane = index;
        });
        list.add_controller(focus);
    }
    {
        let s = state.clone();
        list.connect_row_activated(move |_, row| {
            let name = row.widget_name().to_string();
            let current = s.borrow().panes[index].location.clone();
            let is_dir = s.borrow().panes[index]
                .entries
                .iter()
                .any(|e| e.name == name && e.is_dir);
            if is_dir {
                match current.child(&name) {
                    Ok(next) => navigate(s.clone(), index, next),
                    Err(e) => s.borrow().status.set_text(&e.to_string()),
                }
            }
        });
    }
    {
        let s = state.clone();
        back.connect_clicked(move |_| go_history(s.clone(), index, -1));
    }
    {
        let s = state.clone();
        forward.connect_clicked(move |_| go_history(s.clone(), index, 1));
    }
    {
        let s = state.clone();
        up.connect_clicked(move |_| {
            let next = s.borrow().panes[index].location.parent();
            navigate(s.clone(), index, next);
        });
    }
    {
        let s = state.clone();
        reload.connect_clicked(move |_| refresh(s.clone(), index));
    }
    {
        let s = state.clone();
        choose_local.connect_clicked(move |_| {
            let Some(window) = s.borrow().panes[index]
                .selector
                .root()
                .and_downcast::<gtk::ApplicationWindow>()
            else {
                return;
            };
            let s = s.clone();
            glib::spawn_future_local(async move {
                let dialog = gtk::FileDialog::new();
                if let Ok(file) = dialog.select_folder_future(Some(&window)).await
                    && let Some(path) = file.path()
                {
                    let selector = {
                        let mut state = s.borrow_mut();
                        state.restoring_tab = true;
                        state.panes[index].selector.clone()
                    };
                    selector.set_selected(0);
                    s.borrow_mut().restoring_tab = false;
                    navigate(s, index, Location::Local(path));
                }
            });
        });
    }
    install_drop_targets(state, index, &list);
}

fn navigate(state: Rc<RefCell<State>>, index: usize, location: Location) {
    if let Err(error) = location.validate() {
        state.borrow().status.set_text(&error.to_string());
        return;
    }
    {
        let mut s = state.borrow_mut();
        let pane = &mut s.panes[index];
        if pane.location == location {
            drop(s);
            refresh(state, index);
            return;
        }
        pane.history.truncate(pane.history_position + 1);
        pane.history.push(location.clone());
        pane.history_position = pane.history.len() - 1;
        pane.location = location;
        save_active_tab(&mut s);
    }
    refresh(state.clone(), index);
    render_tabs(state);
}
fn refresh(state: Rc<RefCell<State>>, index: usize) {
    render_breadcrumb(state.clone(), index);
    let (location, profiles, runtime, sender) = {
        let s = state.borrow();
        let pane = &s.panes[index];
        pane.back.set_sensitive(pane.history_position > 0);
        pane.forward
            .set_sensitive(pane.history_position + 1 < pane.history.len());
        (
            s.panes[index].location.clone(),
            s.profiles.clone(),
            s.runtime.clone(),
            s.sender.clone(),
        )
    };
    state.borrow().status.set_text("Loading directory…");
    runtime.spawn(async move {
        let selected: Vec<_> = profiles
            .into_iter()
            .filter(
                |p| matches!(&location, Location::Remote { profile_id, .. } if profile_id == &p.id),
            )
            .collect();
        let result = async {
            let credentials = store::with_credentials(&selected)
                .await
                .map_err(|e| e.to_string())?;
            transfer_rclone::RcloneBackend::new(credentials)
                .list(&location)
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        let _ = sender.send(Message::Listed(index, location, result));
    });
}

fn render_breadcrumb(state: Rc<RefCell<State>>, index: usize) {
    let (bar, location, profiles) = {
        let s = state.borrow();
        (
            s.panes[index].breadcrumb.clone(),
            s.panes[index].location.clone(),
            s.profiles.clone(),
        )
    };
    while let Some(child) = bar.first_child() {
        bar.remove(&child);
    }
    let mut crumbs: Vec<(String, Location)> = Vec::new();
    match location {
        Location::Local(path) => {
            let mut current = PathBuf::from("/");
            crumbs.push(("/".into(), Location::Local(current.clone())));
            for part in path.components() {
                if let std::path::Component::Normal(name) = part {
                    current.push(name);
                    crumbs.push((
                        name.to_string_lossy().to_string(),
                        Location::Local(current.clone()),
                    ));
                }
            }
        }
        Location::Remote { profile_id, path } => {
            let name = profiles
                .iter()
                .find(|p| p.id == profile_id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Remote".into());
            crumbs.push((
                name,
                Location::Remote {
                    profile_id: profile_id.clone(),
                    path: String::new(),
                },
            ));
            let mut current = String::new();
            for part in path.split('/').filter(|part| !part.is_empty()) {
                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(part);
                crumbs.push((
                    part.to_owned(),
                    Location::Remote {
                        profile_id: profile_id.clone(),
                        path: current.clone(),
                    },
                ));
            }
        }
    }
    for (position, (name, location)) in crumbs.into_iter().enumerate() {
        if position > 0 {
            bar.append(&gtk::Label::new(Some("›")));
        }
        let button = gtk::Button::with_label(&name);
        button.add_css_class("flat");
        let s = state.clone();
        button.connect_clicked(move |_| navigate(s.clone(), index, location.clone()));
        bar.append(&button);
    }
}
fn render_pane(state: &Rc<RefCell<State>>, index: usize) {
    let s = state.borrow();
    let pane = &s.panes[index];
    while let Some(child) = pane.list.first_child() {
        pane.list.remove(&child);
    }
    let filter = pane.filter.text().to_lowercase();
    let mut entries: Vec<_> = pane
        .entries
        .iter()
        .filter(|e| {
            (pane.hidden.is_active() || !e.name.starts_with('.'))
                && e.name.to_lowercase().contains(&filter)
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| match pane.sort.selected() {
                1 => a.size.cmp(&b.size),
                2 => a.modified.cmp(&b.modified),
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            })
    });
    for entry in entries {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&entry.name);
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        line.append(&gtk::Image::from_icon_name(if entry.is_dir {
            "folder-symbolic"
        } else {
            "text-x-generic-symbolic"
        }));
        let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
        text.set_hexpand(true);
        let name = gtk::Label::new(Some(&entry.name));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&name);
        let kind = if entry.is_dir {
            "Folder"
        } else {
            entry.mime.as_deref().unwrap_or("File")
        };
        let date = entry
            .modified
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(10)
            .collect::<String>();
        let metadata = gtk::Label::new(Some(
            if date.is_empty() {
                kind.to_owned()
            } else {
                format!("{kind} · {date}")
            }
            .as_str(),
        ));
        metadata.add_css_class("dim-label");
        metadata.set_xalign(0.0);
        metadata.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&metadata);
        line.append(&text);
        let size = gtk::Label::new(Some(&entry.size.map(format_size).unwrap_or_default()));
        size.add_css_class("dim-label");
        size.set_width_chars(8);
        size.set_xalign(1.0);
        line.append(&size);
        row.set_child(Some(&line));
        install_row_actions(state.clone(), index, &row, &entry.name, entry.is_dir);
        pane.list.append(&row);
    }
}

fn install_row_actions(
    state: Rc<RefCell<State>>,
    index: usize,
    row: &gtk::ListBoxRow,
    name: &str,
    is_dir: bool,
) {
    let drag = gtk::DragSource::builder()
        .actions(gtk::gdk::DragAction::COPY)
        .build();
    {
        let state = state.clone();
        let row = row.clone();
        let name = name.to_owned();
        drag.connect_prepare(move |_, _, _| {
            let s = state.borrow();
            let pane = &s.panes[index];
            let names: Vec<String> = if row.is_selected() {
                pane.list
                    .selected_rows()
                    .iter()
                    .map(|r| r.widget_name().to_string())
                    .collect()
            } else {
                vec![name.clone()]
            };
            let sources: Vec<Location> = names
                .iter()
                .filter_map(|name| pane.location.child(name).ok())
                .collect();
            if sources.is_empty() {
                return None;
            }
            if sources
                .iter()
                .all(|source| matches!(source, Location::Local(_)))
            {
                let files: Vec<gio::File> = sources
                    .iter()
                    .filter_map(|source| match source {
                        Location::Local(path) => Some(gio::File::for_path(path)),
                        _ => None,
                    })
                    .collect();
                return Some(gtk::gdk::ContentProvider::for_value(
                    &gtk::gdk::FileList::from_array(&files).to_value(),
                ));
            }
            let payload = format!("nyx-transfer:{}", serde_json::to_string(&sources).ok()?);
            Some(gtk::gdk::ContentProvider::for_value(&payload.to_value()))
        });
    }
    row.add_controller(drag);
    let menu = gtk::GestureClick::new();
    menu.set_button(3);
    {
        let s = state.clone();
        let row = row.clone();
        let name = name.to_owned();
        menu.connect_pressed(move |_, _, _, _| {
            let (list, source) = {
                let state = s.borrow();
                let pane = &state.panes[index];
                (pane.list.clone(), pane.location.child(&name).ok())
            };
            if !row.is_selected() {
                list.unselect_all();
                list.select_row(Some(&row));
            }
            let popover = gtk::Popover::new();
            popover.set_parent(&row);
            let actions = gtk::Box::new(gtk::Orientation::Vertical, 2);
            if is_dir {
                let open = gtk::Button::with_label("Open folder");
                let s = s.clone();
                let source = source.clone();
                let pop = popover.clone();
                open.connect_clicked(move |_| {
                    if let Some(source) = &source {
                        navigate(s.clone(), index, source.clone());
                    }
                    pop.popdown();
                });
                actions.append(&open);
            }
            let copy = gtk::Button::with_label("Copy to other pane");
            {
                let s = s.clone();
                let pop = popover.clone();
                copy.connect_clicked(move |_| {
                    queue_selected(s.clone(), index, 1 - index);
                    pop.popdown();
                });
            }
            actions.append(&copy);
            let copy_path = gtk::Button::with_label("Copy path");
            {
                let source = source.clone();
                let pop = popover.clone();
                let s = s.clone();
                copy_path.connect_clicked(move |_| {
                    if let Some(source) = &source
                        && let Some(display) = gtk::gdk::Display::default()
                    {
                        let value = location_name(source, &s.borrow().profiles);
                        display.clipboard().set_text(&value);
                    }
                    pop.popdown();
                });
            }
            actions.append(&copy_path);
            popover.set_child(Some(&actions));
            popover.connect_closed(|popover| popover.unparent());
            popover.popup();
        });
    }
    row.add_controller(menu);
}

fn drop_destination(state: &Rc<RefCell<State>>, index: usize, y: f64) -> Location {
    let s = state.borrow();
    let pane = &s.panes[index];
    let current = pane.location.clone();
    if let Some(row) = pane.list.row_at_y(y as i32) {
        let name = row.widget_name().to_string();
        if pane
            .entries
            .iter()
            .any(|entry| entry.name == name && entry.is_dir)
        {
            return current.child(&name).unwrap_or(current);
        }
    }
    current
}

fn queue_dropped(state: Rc<RefCell<State>>, sources: Vec<Location>, destination: Location) -> bool {
    let mut queued = false;
    for source in sources {
        let name = match &source {
            Location::Local(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().to_string()),
            Location::Remote { path, .. } => path.rsplit('/').next().map(str::to_owned),
        };
        let Some(name) = name else { continue };
        let Ok(target) = destination.child(&name) else {
            continue;
        };
        if target == source {
            continue;
        }
        enqueue_job(state.clone(), source, target);
        queued = true;
    }
    queued
}

fn install_drop_targets(state: Rc<RefCell<State>>, index: usize, list: &gtk::ListBox) {
    let files = gtk::DropTarget::new(
        gtk::gdk::FileList::static_type(),
        gtk::gdk::DragAction::COPY,
    );
    {
        let s = state.clone();
        files.connect_drop(move |_, value, _, y| {
            let Ok(files) = value.get::<gtk::gdk::FileList>() else {
                return false;
            };
            let sources: Vec<Location> = files
                .files()
                .iter()
                .filter_map(|file| file.path().map(Location::Local))
                .collect();
            let destination = drop_destination(&s, index, y);
            queue_dropped(s.clone(), sources, destination)
        });
    }
    list.add_controller(files);
    let remote = gtk::DropTarget::new(String::static_type(), gtk::gdk::DragAction::COPY);
    {
        let s = state.clone();
        remote.connect_drop(move |_, value, _, y| {
            let Ok(payload) = value.get::<String>() else {
                return false;
            };
            let Some(json) = payload.strip_prefix("nyx-transfer:") else {
                return false;
            };
            let Ok(sources) = serde_json::from_str::<Vec<Location>>(json) else {
                return false;
            };
            let destination = drop_destination(&s, index, y);
            queue_dropped(s.clone(), sources, destination)
        });
    }
    list.add_controller(remote);
}
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn queue_selected(state: Rc<RefCell<State>>, from: usize, to: usize) {
    let (source, destination, names) = {
        let s = state.borrow();
        let names: Vec<String> = s.panes[from]
            .list
            .selected_rows()
            .iter()
            .map(|r| r.widget_name().to_string())
            .collect();
        (
            s.panes[from].location.clone(),
            s.panes[to].location.clone(),
            names,
        )
    };
    if names.is_empty() {
        state
            .borrow()
            .status
            .set_text("Select a file or directory first.");
        return;
    }
    if source == destination {
        state
            .borrow()
            .status
            .set_text("Choose different source and destination directories.");
        return;
    }
    for name in names {
        let (Ok(src), Ok(dst)) = (source.child(&name), destination.child(&name)) else {
            continue;
        };
        enqueue_job(state.clone(), src, dst);
    }
}
fn enqueue_job(state: Rc<RefCell<State>>, source: Location, destination: Location) {
    let (runtime, client, sender) = {
        let s = state.borrow();
        (s.runtime.clone(), s.client.clone(), s.sender.clone())
    };
    runtime.spawn(async move {
        let result = async {
            let client = client
                .get_or_try_init(transfer_ipc::Client::connect)
                .await
                .map_err(|e| e.to_string())?;
            client
                .enqueue(&source, &destination)
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        .await;
        let _ = sender.send(Message::QueueResult(result));
    });
}

fn request_jobs(state: Rc<RefCell<State>>) {
    let (runtime, client, sender) = {
        let s = state.borrow();
        (s.runtime.clone(), s.client.clone(), s.sender.clone())
    };
    runtime.spawn(async move {
        let result = async {
            let client = client
                .get_or_try_init(transfer_ipc::Client::connect)
                .await
                .map_err(|e| e.to_string())?;
            client.jobs().await.map_err(|e| e.to_string())
        }
        .await;
        let _ = sender.send(Message::Jobs(result));
    });
}

fn control_job(state: Rc<RefCell<State>>, id: String, action: &'static str) {
    let (runtime, client, sender) = {
        let s = state.borrow();
        (s.runtime.clone(), s.client.clone(), s.sender.clone())
    };
    runtime.spawn(async move {
        let result = async {
            let client = client
                .get_or_try_init(transfer_ipc::Client::connect)
                .await
                .map_err(|e| e.to_string())?;
            match action {
                "pause" => client.pause(&id).await,
                "resume" => client.resume(&id).await,
                "cancel" => client.cancel(&id).await,
                "retry" => client.retry(&id).await,
                "clear" => client.clear_completed().await,
                _ => return Err("Unknown queue action".to_owned()),
            }
            .map_err(|e| e.to_string())?;
            Ok(())
        }
        .await;
        let _ = sender.send(Message::ControlResult(result));
    });
}

fn render_jobs(state: Rc<RefCell<State>>, snapshots: Vec<JobSnapshot>) {
    state.borrow().empty_queue.set_visible(snapshots.is_empty());
    let running = snapshots
        .iter()
        .filter(|job| job.status == JobStatus::Running)
        .count();
    let waiting = snapshots
        .iter()
        .filter(|job| matches!(job.status, JobStatus::Queued | JobStatus::Paused))
        .count();
    let ids: std::collections::HashSet<_> = snapshots.iter().map(|j| j.id.clone()).collect();
    state.borrow().clear_completed.set_sensitive(
        snapshots
            .iter()
            .any(|job| job.status == JobStatus::Completed),
    );
    {
        let mut s = state.borrow_mut();
        let gone: Vec<_> = s
            .jobs
            .keys()
            .filter(|id| !ids.contains(*id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(job) = s.jobs.remove(&id) {
                s.queue.remove(&job.row);
            }
        }
    }
    for snapshot in snapshots {
        if !state.borrow().jobs.contains_key(&snapshot.id) {
            let line = gtk::Box::new(gtk::Orientation::Vertical, 2);
            line.add_css_class("job-row");
            let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let title_text = {
                let s = state.borrow();
                format!(
                    "{}  ·  {} → {}",
                    snapshot.name,
                    location_name(&snapshot.source, &s.profiles),
                    location_name(&snapshot.destination, &s.profiles)
                )
            };
            let title = gtk::Label::new(Some(&title_text));
            title.set_xalign(0.0);
            title.set_hexpand(true);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let status = gtk::Label::new(None);
            status.add_css_class("dim-label");
            status.set_max_width_chars(42);
            status.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let pause = gtk::Button::from_icon_name("media-playback-pause-symbolic");
            square_control(&pause);
            pause.set_tooltip_text(Some("Pause transfer"));
            let cancel = gtk::Button::from_icon_name("process-stop-symbolic");
            square_control(&cancel);
            cancel.set_tooltip_text(Some("Cancel transfer"));
            let retry = gtk::Button::from_icon_name("view-refresh-symbolic");
            square_control(&retry);
            retry.set_tooltip_text(Some("Retry transfer"));
            top.append(&title);
            top.append(&status);
            top.append(&pause);
            top.append(&cancel);
            top.append(&retry);
            line.append(&top);
            let progress = gtk::ProgressBar::new();
            progress.set_show_text(false);
            line.append(&progress);
            let detail = gtk::Label::new(None);
            detail.set_xalign(0.0);
            detail.add_css_class("dim-label");
            line.append(&detail);
            {
                let s = state.clone();
                let id = snapshot.id.clone();
                pause.connect_clicked(move |_| {
                    let action = if s
                        .borrow()
                        .jobs
                        .get(&id)
                        .is_some_and(|j| j.state == JobStatus::Paused)
                    {
                        "resume"
                    } else {
                        "pause"
                    };
                    control_job(s.clone(), id.clone(), action);
                });
            }
            {
                let s = state.clone();
                let id = snapshot.id.clone();
                cancel.connect_clicked(move |_| control_job(s.clone(), id.clone(), "cancel"));
            }
            {
                let s = state.clone();
                let id = snapshot.id.clone();
                retry.connect_clicked(move |_| control_job(s.clone(), id.clone(), "retry"));
            }
            let mut s = state.borrow_mut();
            s.queue.append(&line);
            s.jobs.insert(
                snapshot.id.clone(),
                JobView {
                    row: line,
                    status,
                    detail,
                    progress,
                    retry,
                    pause,
                    cancel,
                    state: snapshot.status.clone(),
                },
            );
        }
        if let Some(view) = state.borrow_mut().jobs.get_mut(&snapshot.id) {
            view.state = snapshot.status.clone();
            let status_text = match &snapshot.status {
                JobStatus::Queued => "Queued".to_owned(),
                JobStatus::Paused => "Paused".to_owned(),
                JobStatus::Running => "Running".to_owned(),
                JobStatus::Completed => "Completed".to_owned(),
                JobStatus::Cancelled => "Cancelled".to_owned(),
                JobStatus::Failed(error) => format!("Failed: {error}"),
            };
            view.status.set_text(&status_text);
            view.status.set_tooltip_text(Some(&snapshot.progress));
            let amount = match snapshot.total_bytes {
                Some(total) if total > 0 => {
                    format!("{} / {}", format_size(snapshot.bytes), format_size(total))
                }
                _ if snapshot.bytes > 0 => format_size(snapshot.bytes),
                _ => "—".into(),
            };
            let speed = snapshot
                .speed_bytes_per_sec
                .filter(|speed| *speed > 0.0)
                .map(|speed| format!("{}/s", format_size(speed as u64)))
                .unwrap_or_else(|| "—".into());
            let eta = snapshot
                .eta_seconds
                .map(format_duration)
                .unwrap_or_else(|| "—".into());
            view.detail.set_text(&format!(
                "{amount}    {speed}    ETA {eta}    Retries {}",
                snapshot.retries
            ));
            if snapshot.status == JobStatus::Completed {
                view.progress.set_fraction(1.0);
            } else if let Some(total) = snapshot.total_bytes.filter(|total| *total > 0) {
                view.progress
                    .set_fraction((snapshot.bytes as f64 / total as f64).clamp(0.0, 1.0));
            } else if snapshot.status == JobStatus::Running {
                view.progress.pulse();
            } else {
                view.progress.set_fraction(0.0);
            }
            if snapshot.status == JobStatus::Paused {
                view.pause.set_icon_name("media-playback-start-symbolic");
                view.pause.set_tooltip_text(Some("Resume transfer"));
            } else {
                view.pause.set_icon_name("media-playback-pause-symbolic");
                view.pause.set_tooltip_text(Some("Pause transfer"));
            }
            view.pause.set_sensitive(matches!(
                snapshot.status,
                JobStatus::Queued | JobStatus::Paused
            ));
            view.cancel.set_sensitive(matches!(
                snapshot.status,
                JobStatus::Queued | JobStatus::Paused | JobStatus::Running
            ));
            view.retry
                .set_sensitive(matches!(snapshot.status, JobStatus::Failed(_)));
        }
    }
    {
        let s = state.borrow();
        if running > 0 {
            s.activity_spinner.start();
        } else {
            s.activity_spinner.stop();
        }
        s.activity
            .set_text(&format!("{running} active · {waiting} waiting"));
    }
}
fn format_duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}
fn location_name(location: &Location, profiles: &[ConnectionProfile]) -> String {
    match location {
        Location::Local(path) => path.display().to_string(),
        Location::Remote { profile_id, path } => {
            let name = profiles
                .iter()
                .find(|p| &p.id == profile_id)
                .map(|p| p.name.as_str())
                .unwrap_or("Remote");
            format!("{name}:{path}")
        }
    }
}
fn handle_message(state: Rc<RefCell<State>>, message: Message) {
    match message {
        Message::Listed(index, location, result) => {
            if state.borrow().panes[index].location != location {
                return;
            }
            match result {
                Ok(entries) => {
                    let count = entries.len();
                    {
                        let mut s = state.borrow_mut();
                        let pane = &mut s.panes[index];
                        pane.entries = entries;
                        pane.path.set_text(match &location {
                            Location::Local(p) => p.to_str().unwrap_or(""),
                            Location::Remote { path, .. } => path,
                        });
                        s.status.set_text(&format!("{count} items"));
                    }
                    render_pane(&state, index);
                }
                Err(e) => state.borrow().status.set_text(&e),
            }
        }
        Message::Jobs(Ok(jobs)) => render_jobs(state, jobs),
        Message::Jobs(Err(e)) => state
            .borrow()
            .status
            .set_text(&format!("Transfer service unavailable: {e}")),
        Message::QueueResult(result) => match result {
            Ok(()) => {
                let s = state.borrow();
                s.status.set_text("Transfer queued.");
                s.queue_revealer.set_reveal_child(true);
                s.queue_toggle.set_icon_name("pan-down-symbolic");
                drop(s);
                request_jobs(state);
            }
            Err(e) => state
                .borrow()
                .status
                .set_text(&format!("Could not queue transfer: {e}")),
        },
        Message::ControlResult(result) => match result {
            Ok(()) => request_jobs(state),
            Err(e) => state.borrow().status.set_text(&e),
        },
        Message::Saved(result) => match result {
            Ok(profile) => {
                let mut s = state.borrow_mut();
                for pane in &s.panes {
                    if let Some(model) = pane.selector.model().and_downcast::<gtk::StringList>() {
                        model.append(&format!("{} · {}", profile.name, profile.protocol.label()));
                    }
                }
                s.status
                    .set_text(&format!("Saved connection {}", profile.name));
                s.profiles.push(profile);
            }
            Err(e) => state.borrow().status.set_text(&e),
        },
        Message::Remotes(result) => match result {
            Ok((config, remotes)) => {
                if let Some(window) = state.borrow().panes[0]
                    .selector
                    .root()
                    .and_downcast::<gtk::ApplicationWindow>()
                {
                    import_remote_dialog(&window, state.clone(), config, remotes);
                }
            }
            Err(e) => state.borrow().status.set_text(&e),
        },
        Message::SyncPreview(request_id, result) => {
            let mut s = state.borrow_mut();
            let Some(dialog) = s
                .sync_dialog
                .as_mut()
                .filter(|dialog| dialog.request_id == request_id)
            else {
                return;
            };
            match result {
                Ok(preview) => {
                    let copied = preview
                        .changes
                        .iter()
                        .filter(|c| c.action == SyncAction::Copy)
                        .count();
                    let updated = preview
                        .changes
                        .iter()
                        .filter(|c| c.action == SyncAction::Update)
                        .count();
                    let deleted = preview
                        .changes
                        .iter()
                        .filter(|c| c.action == SyncAction::Delete)
                        .count();
                    dialog.status.set_text(&format!(
                        "{copied} new · {updated} changed · {deleted} to delete"
                    ));
                    let report = preview
                        .changes
                        .iter()
                        .map(|c| {
                            let verb = match c.action {
                                SyncAction::Copy => "COPY",
                                SyncAction::Update => "UPDATE",
                                SyncAction::Delete => "DELETE",
                            };
                            format!("{verb}  {}", c.path)
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    dialog.report.buffer().set_text(if report.is_empty() {
                        "No changes."
                    } else {
                        &report
                    });
                    dialog.queue.set_sensitive(!preview.changes.is_empty());
                    dialog.preview = Some(preview);
                }
                Err(e) => {
                    dialog.status.set_text(&e);
                    dialog.report.buffer().set_text("");
                    dialog.queue.set_sensitive(false);
                    dialog.preview = None;
                }
            }
        }
        Message::WorkflowPreview(request_id, result) => {
            workflows::handle_preview(&state, &request_id, result);
        }
    }
}

fn sync_dialog(parent: &gtk::ApplicationWindow, state: Rc<RefCell<State>>) {
    if let Some(existing) = &state.borrow().sync_dialog {
        existing.window.present();
        return;
    }
    let (left, right, left_name, right_name) = {
        let s = state.borrow();
        let left = s.panes[0].location.clone();
        let right = s.panes[1].location.clone();
        let left_name = location_name(&left, &s.profiles);
        let right_name = location_name(&right, &s.profiles);
        (left, right, left_name, right_name)
    };
    let window = gtk::Window::builder()
        .title("Synchronize folders")
        .transient_for(parent)
        .modal(true)
        .default_width(660)
        .default_height(540)
        .build();
    window.add_css_class("transfer-window");
    window.add_css_class("transfer-dialog");
    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    form.set_margin_top(16);
    form.set_margin_bottom(16);
    form.set_margin_start(16);
    form.set_margin_end(16);
    let source_label = gtk::Label::new(Some(&format!("Left: {left_name}\nRight: {right_name}")));
    source_label.set_xalign(0.0);
    source_label.set_wrap(true);
    form.append(&source_label);
    let direction = gtk::DropDown::from_strings(&["Left → right", "Right → left"]);
    dropdown_field(&form, "Direction", &direction);
    let mode = gtk::DropDown::from_strings(&[
        "Copy new and changed files",
        "Mirror (delete destination extras)",
    ]);
    dropdown_field(&form, "Mode", &mode);
    let preview_button = gtk::Button::with_label("Preview changes");
    form.append(&preview_button);
    let status = gtk::Label::new(Some("Choose a direction and preview the changes."));
    status.set_xalign(0.0);
    status.set_wrap(true);
    form.append(&status);
    let report = gtk::TextView::new();
    report.set_editable(false);
    report.set_cursor_visible(false);
    report.set_monospace(true);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&report)
        .vexpand(true)
        .build();
    form.append(&scroll);
    let queue = gtk::Button::with_label("Queue synchronization");
    queue.add_css_class("suggested-action");
    queue.set_sensitive(false);
    form.append(&queue);
    window.set_child(Some(&form));
    state.borrow_mut().sync_dialog = Some(SyncDialog {
        window: window.clone(),
        request_id: String::new(),
        source: left.clone(),
        destination: right.clone(),
        mode: SyncMode::Copy,
        preview: None,
        report: report.clone(),
        status: status.clone(),
        queue: queue.clone(),
    });
    for choice in [&direction, &mode] {
        let s = state.clone();
        choice.connect_selected_notify(move |_| {
            if let Some(dialog) = s.borrow_mut().sync_dialog.as_mut() {
                dialog.preview = None;
                dialog.queue.set_sensitive(false);
                dialog.status.set_text("Preview changes before queueing.");
                dialog.report.buffer().set_text("");
            }
        });
    }
    {
        let s = state.clone();
        preview_button.connect_clicked(move |_| {
            let (source, destination) = if direction.selected() == 0 {
                (left.clone(), right.clone())
            } else {
                (right.clone(), left.clone())
            };
            let sync_mode = if mode.selected() == 0 {
                SyncMode::Copy
            } else {
                SyncMode::Mirror
            };
            let request_id = uuid::Uuid::new_v4().to_string();
            let (runtime, client, sender) = {
                let mut state = s.borrow_mut();
                let Some(dialog) = state.sync_dialog.as_mut() else {
                    return;
                };
                dialog.request_id = request_id.clone();
                dialog.source = source.clone();
                dialog.destination = destination.clone();
                dialog.mode = sync_mode;
                dialog.preview = None;
                dialog.queue.set_sensitive(false);
                dialog.status.set_text("Comparing folders…");
                dialog.report.buffer().set_text("");
                (
                    state.runtime.clone(),
                    state.client.clone(),
                    state.sender.clone(),
                )
            };
            runtime.spawn(async move {
                let result = async {
                    let client = client
                        .get_or_try_init(transfer_ipc::Client::connect)
                        .await
                        .map_err(|e| e.to_string())?;
                    client
                        .preview_sync(&source, &destination, sync_mode)
                        .await
                        .map_err(|e| e.to_string())
                }
                .await;
                let _ = sender.send(Message::SyncPreview(request_id, result));
            });
        });
    }
    {
        let s = state.clone();
        let window = window.clone();
        queue.connect_clicked(move |_| {
            let chosen = {
                let state = s.borrow();
                state.sync_dialog.as_ref().and_then(|dialog| dialog.preview.as_ref().map(|preview| {
                    (dialog.source.clone(), dialog.destination.clone(), dialog.mode, preview.fingerprint.clone(),
                     preview.changes.iter().filter(|change| change.action == SyncAction::Delete).count())
                }))
            };
            let Some((source, destination, sync_mode, fingerprint, deletions)) = chosen else { return };
            let s = s.clone();
            let window = window.clone();
            glib::spawn_future_local(async move {
                if sync_mode == SyncMode::Mirror {
                    let prompt = gtk::AlertDialog::builder()
                        .message("Queue directory mirror?")
                        .detail(format!("This will delete {deletions} destination files shown in the preview. Changes to the plan will stop the job."))
                        .buttons(["Cancel", "Queue mirror"])
                        .cancel_button(0)
                        .default_button(0)
                        .build();
                    if prompt.choose_future(Some(&window)).await != Ok(1) { return; }
                }
                queue_sync_job(s, source, destination, sync_mode, fingerprint);
                window.close();
            });
        });
    }
    {
        let s = state.clone();
        window.connect_hide(move |_| {
            s.borrow_mut().sync_dialog = None;
        });
    }
    window.present();
}

fn queue_sync_job(
    state: Rc<RefCell<State>>,
    source: Location,
    destination: Location,
    mode: SyncMode,
    fingerprint: String,
) {
    let (runtime, client, sender) = {
        let s = state.borrow();
        (s.runtime.clone(), s.client.clone(), s.sender.clone())
    };
    runtime.spawn(async move {
        let result = async {
            let client = client
                .get_or_try_init(transfer_ipc::Client::connect)
                .await
                .map_err(|e| e.to_string())?;
            client
                .enqueue_sync(&source, &destination, mode, &fingerprint)
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        .await;
        let _ = sender.send(Message::QueueResult(result));
    });
}

fn request_remotes(state: Rc<RefCell<State>>) {
    state.borrow().status.set_text("Reading rclone remotes…");
    let (runtime, sender) = {
        let s = state.borrow();
        (s.runtime.clone(), s.sender.clone())
    };
    runtime.spawn(async move {
        let config = transfer_rclone::default_config_path();
        let result = transfer_rclone::available_remotes(&config)
            .await
            .map(|remotes| (config, remotes))
            .map_err(|e| e.to_string());
        let _ = sender.send(Message::Remotes(result));
    });
}

fn import_remote_dialog(
    parent: &gtk::ApplicationWindow,
    state: Rc<RefCell<State>>,
    config: PathBuf,
    remotes: Vec<transfer_rclone::AvailableRemote>,
) {
    if remotes.is_empty() {
        state
            .borrow()
            .status
            .set_text("No remotes found in the rclone config.");
        return;
    }
    let dialog = gtk::Window::builder()
        .title("Import rclone remote")
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .build();
    dialog.add_css_class("transfer-window");
    dialog.add_css_class("transfer-dialog");
    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    form.set_margin_top(16);
    form.set_margin_bottom(16);
    form.set_margin_start(16);
    form.set_margin_end(16);
    let config_label = gtk::Label::new(Some(&format!("Config: {}", config.display())));
    config_label.set_xalign(0.0);
    config_label.set_wrap(true);
    form.append(&config_label);
    let names: Vec<String> = remotes
        .iter()
        .map(|r| format!("{} · {}", r.name, r.kind))
        .collect();
    let labels: Vec<&str> = names.iter().map(String::as_str).collect();
    let selector = gtk::DropDown::from_strings(&labels);
    dropdown_field(&form, "Remote", &selector);
    let name = field(&form, "Display name", "");
    name.set_text(&remotes[0].name);
    let directory = field(&form, "Starting directory (optional)", "");
    let detail = gtk::Label::new(Some(
        "Transfer uses this rclone config and its authentication. rclone may refresh provider tokens in that file.",
    ));
    detail.set_wrap(true);
    detail.set_xalign(0.0);
    form.append(&detail);
    let save = gtk::Button::with_label("Import remote");
    save.add_css_class("suggested-action");
    form.append(&save);
    dialog.set_child(Some(&form));
    {
        let names: Vec<String> = remotes.iter().map(|r| r.name.clone()).collect();
        let name = name.clone();
        selector.connect_selected_notify(move |selector| {
            if let Some(remote) = names.get(selector.selected() as usize) {
                name.set_text(remote);
            }
        });
    }
    let close = dialog.clone();
    save.connect_clicked(move |_| {
        let Some(remote) = remotes.get(selector.selected() as usize) else {
            return;
        };
        let display = name.text().trim().to_owned();
        let start = directory.text().trim_matches('/').to_owned();
        if display.is_empty() {
            state.borrow().status.set_text("Enter a display name.");
            return;
        }
        let probe = Location::Remote {
            profile_id: "import".into(),
            path: if start.is_empty() {
                "probe".into()
            } else {
                start.clone()
            },
        };
        if probe.validate().is_err() {
            state
                .borrow()
                .status
                .set_text("Enter a valid starting directory.");
            return;
        }
        let profile = ConnectionProfile {
            id: uuid::Uuid::new_v4().to_string(),
            name: display,
            protocol: Protocol::Rclone,
            host: String::new(),
            port: 0,
            username: String::new(),
            auth: Auth::Agent,
            default_dir: start,
            timeout_secs: 30,
            ssh_key_file: None,
            known_hosts_file: None,
            ca_cert: None,
            allow_plaintext: false,
            rclone_remote: Some(remote.name.clone()),
            rclone_config: Some(config.clone()),
        };
        let result = store::save(&profile)
            .map(|_| profile)
            .map_err(|e| e.to_string());
        let _ = state.borrow().sender.send(Message::Saved(result));
        close.close();
    });
    dialog.present();
}

fn connection_dialog(parent: &gtk::ApplicationWindow, state: Rc<RefCell<State>>) {
    let dialog = gtk::Window::builder()
        .title("New connection")
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .default_height(650)
        .build();
    dialog.add_css_class("transfer-window");
    dialog.add_css_class("transfer-dialog");
    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    form.set_margin_top(16);
    form.set_margin_bottom(16);
    form.set_margin_start(16);
    form.set_margin_end(16);
    let name = field(&form, "Display name", "My server");
    let protocol = gtk::DropDown::from_strings(&[
        "SFTP",
        "FTP",
        "FTPS (explicit)",
        "FTPS (implicit)",
        "WebDAV",
    ]);
    dropdown_field(&form, "Protocol", &protocol);
    let host = field(&form, "Host or WebDAV URL", "example.com");
    let port = field(&form, "Port", "22");
    port.set_text("22");
    let user = field(&form, "Username", "");
    let auth = gtk::DropDown::from_strings(&["SSH agent", "SSH key", "Password"]);
    dropdown_field(&form, "Authentication", &auth);
    let key = field(&form, "SSH key file (if selected)", "");
    let known = field(&form, "SSH known_hosts file", "");
    known.set_text(&format!(
        "{}/.ssh/known_hosts",
        std::env::var("HOME").unwrap_or_default()
    ));
    let trust = gtk::Button::with_label("Trust a new SSH host key…");
    form.append(&trust);
    let ca = field(&form, "Custom TLS CA certificate (optional)", "");
    let password = field(&form, "Password (if selected)", "");
    password.set_visibility(false);
    let directory = field(&form, "Starting directory", "");
    let plaintext = gtk::CheckButton::with_label("Allow plaintext FTP or HTTP WebDAV");
    form.append(&plaintext);
    let save = gtk::Button::with_label("Save connection");
    save.add_css_class("suggested-action");
    form.append(&save);
    dialog.set_child(Some(&gtk::ScrolledWindow::builder().child(&form).build()));
    {
        let port = port.clone();
        let auth = auth.clone();
        protocol.connect_selected_notify(move |choice| match choice.selected() {
            0 => {
                port.set_text("22");
                auth.set_selected(0);
            }
            1 | 2 => {
                port.set_text("21");
                auth.set_selected(2);
            }
            3 => {
                port.set_text("990");
                auth.set_selected(2);
            }
            _ => {
                port.set_text("443");
                auth.set_selected(2);
            }
        });
    }
    {
        let host = host.clone();
        let port = port.clone();
        let known = known.clone();
        let dialog = dialog.clone();
        trust.connect_clicked(move |_| {
            trust_host(
                &dialog,
                host.text().to_string(),
                port.text().to_string(),
                known.clone(),
            )
        });
    }
    let close = dialog.clone();
    save.connect_clicked(move |_| {
        let protocol = match protocol.selected() {
            0 => Protocol::Sftp,
            1 => Protocol::Ftp,
            2 => Protocol::FtpsExplicit,
            3 => Protocol::FtpsImplicit,
            _ => Protocol::Webdav,
        };
        let auth = match auth.selected() {
            0 => Auth::Agent,
            1 => Auth::KeyFile,
            _ => Auth::Password,
        };
        let host_value = host.text().trim().to_string();
        let Ok(port_value) = port.text().parse::<u16>() else {
            state.borrow().status.set_text("Enter a valid port.");
            return;
        };
        if port_value == 0
            || host_value.starts_with('-')
            || host_value.contains(char::is_whitespace)
        {
            state
                .borrow()
                .status
                .set_text("Enter a valid host and port.");
            return;
        }
        let profile = ConnectionProfile {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.text().to_string(),
            protocol,
            host: host_value,
            port: port_value,
            username: user.text().to_string(),
            auth: auth.clone(),
            default_dir: directory.text().to_string(),
            timeout_secs: 30,
            ssh_key_file: if key.text().is_empty() {
                None
            } else {
                Some(PathBuf::from(key.text().as_str()))
            },
            known_hosts_file: if known.text().is_empty() {
                None
            } else {
                Some(PathBuf::from(known.text().as_str()))
            },
            ca_cert: if ca.text().is_empty() {
                None
            } else {
                Some(PathBuf::from(ca.text().as_str()))
            },
            allow_plaintext: plaintext.is_active(),
            rclone_remote: None,
            rclone_config: None,
        };
        if profile.name.trim().is_empty() || profile.host.trim().is_empty() {
            state.borrow().status.set_text("Enter a name and host.");
            return;
        }
        if profile.protocol == Protocol::Ftp && !profile.allow_plaintext {
            state
                .borrow()
                .status
                .set_text("Confirm plaintext FTP before saving.");
            return;
        }
        if profile.protocol == Protocol::Webdav
            && !profile.host.starts_with("https://")
            && !profile.host.starts_with("http://")
        {
            state
                .borrow()
                .status
                .set_text("Enter a WebDAV URL starting with https:// or http://.");
            return;
        }
        if profile.protocol != Protocol::Webdav && profile.host.contains('/') {
            state
                .borrow()
                .status
                .set_text("Enter a host name without a path.");
            return;
        }
        if profile.host.contains('@') || profile.host.contains('?') || profile.host.contains('#') {
            state
                .borrow()
                .status
                .set_text("Put credentials in the password field, not the host URL.");
            return;
        }
        if profile.protocol == Protocol::Webdav
            && profile.host.starts_with("http://")
            && !profile.allow_plaintext
        {
            state
                .borrow()
                .status
                .set_text("Confirm plaintext HTTP WebDAV before saving.");
            return;
        }
        let secret = password.text().to_string();
        if auth == Auth::Password && (secret.is_empty() || secret.contains(['\n', '\r'])) {
            state
                .borrow()
                .status
                .set_text("Enter a password without line breaks.");
            return;
        }
        let (runtime, sender) = {
            let s = state.borrow();
            (s.runtime.clone(), s.sender.clone())
        };
        runtime.spawn(async move {
            let result = async {
                if auth == Auth::Password {
                    store::save_password(&profile.id, &secret)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                store::save(&profile).map_err(|e| e.to_string())?;
                Ok(profile)
            }
            .await;
            let _ = sender.send(Message::Saved(result));
        });
        close.close();
    });
    dialog.present();
}
fn trust_host(parent: &gtk::Window, host: String, port: String, known: gtk::Entry) {
    if host.trim().is_empty() {
        return;
    }
    let parent = parent.clone();
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || -> Result<(String, String), String> {
            use std::{
                io::Write,
                process::{Command, Stdio},
            };
            let output = Command::new("ssh-keyscan")
                .args(["-T", "5", "-p", &port, "-t", "ed25519,rsa,ecdsa", &host])
                .output()
                .map_err(|e| e.to_string())?;
            let keys = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
            if keys
                .lines()
                .all(|line| line.starts_with('#') || line.trim().is_empty())
            {
                return Err("No SSH host key was returned".into());
            }
            let mut fingerprint = Command::new("ssh-keygen")
                .args(["-lf", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?;
            fingerprint
                .stdin
                .take()
                .ok_or("Cannot inspect key")?
                .write_all(keys.as_bytes())
                .map_err(|e| e.to_string())?;
            let shown = fingerprint.wait_with_output().map_err(|e| e.to_string())?;
            Ok((keys, String::from_utf8_lossy(&shown.stdout).to_string()))
        })
        .await;
        match result {
            Ok(Ok((keys, fingerprints))) => {
                let prompt = gtk::AlertDialog::builder()
                    .message("Trust this SSH host key?")
                    .detail(fingerprints)
                    .buttons(["Cancel", "Trust"])
                    .cancel_button(0)
                    .default_button(0)
                    .build();
                if prompt.choose_future(Some(&parent)).await == Ok(1) {
                    let path = nyx_core::paths::data_dir().join("ssh/known_hosts");
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    use std::io::Write;
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
                        if file.write_all(keys.as_bytes()).is_ok() {
                            known.set_text(&path.to_string_lossy());
                        }
                    }
                }
            }
            _ => gtk::AlertDialog::builder()
                .message("Could not inspect the SSH host key")
                .buttons(["Close"])
                .build()
                .show(Some(&parent)),
        }
    });
}
fn field(parent: &gtk::Box, label: &str, placeholder: &str) -> gtk::Entry {
    let group = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let title = gtk::Label::new(Some(label));
    title.set_xalign(0.0);
    title.add_css_class("dim-label");
    group.append(&title);
    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some(placeholder));
    group.append(&entry);
    parent.append(&group);
    entry
}

fn dropdown_field(parent: &gtk::Box, label: &str, dropdown: &gtk::DropDown) {
    let group = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let title = gtk::Label::new(Some(label));
    title.set_xalign(0.0);
    title.add_css_class("dim-label");
    group.append(&title);
    dropdown.set_hexpand(true);
    group.append(dropdown);
    parent.append(&group);
}
