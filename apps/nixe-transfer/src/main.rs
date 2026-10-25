use gtk::prelude::*;
use gtk::{gio, glib};
use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, sync::Arc, time::Duration};
use transfer_core::{
    Auth, ConnectionProfile, FileEntry, JobSnapshot, JobStatus, Location, Protocol, StorageBackend,
    SyncAction, SyncMode, SyncPreview,
};
use transfer_storage as store;
mod workflows;

const APP_ID: &str = "io.github.design_nexus.nixe.Transfer";

struct Pane {
    location: Location,
    entries: Vec<FileEntry>,
    selector: gtk::DropDown,
    path: gtk::Entry,
    filter: gtk::SearchEntry,
    sort: gtk::DropDown,
    list: gtk::ListBox,
    up: gtk::Button,
    refresh: gtk::Button,
}
struct State {
    panes: Vec<Pane>,
    profiles: Vec<ConnectionProfile>,
    queue: gtk::Box,
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
    nixe_core::app::init(nixe_core::app::App {
        slug: "nixe-transfer",
        name: "Transfer",
    });
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build);
    app.run()
}

fn build(app: &gtk::Application) {
    nixe_theme::install(nixe_theme::Style {
        window_class: "transfer-window",
        base_skip: &[],
        css: include_str!("style.css"),
        no_glow_css: "",
        extra_colors: None,
        prefs: || nixe_theme::ThemePrefs {
            mode: nixe_theme::ThemeMode::Omarchy,
            theme: "tokyo-night".into(),
            glow: false,
            reduce_motion: false,
        },
        set_theme: None,
    });
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Transfer")
        .default_width(1150)
        .default_height(750)
        .build();
    window.add_css_class("transfer-window");
    window.set_titlebar(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    window.set_icon_name(Some(APP_ID));
    nixe_ui::keys::install(
        &window,
        nixe_ui::keys::Common {
            close: true,
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
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar.add_css_class("transfer-toolbar");
    let title = gtk::Label::new(Some("TRANSFER"));
    title.add_css_class("dim-label");
    toolbar.append(&title);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    toolbar.append(&spacer);
    let new_connection = gtk::Button::with_label("New connection");
    toolbar.append(&new_connection);
    let import_remote = gtk::Button::with_label("Import rclone remote");
    toolbar.append(&import_remote);
    let sync_folders = gtk::Button::with_label("Sync folders");
    toolbar.append(&sync_folders);
    let workflows = gtk::Button::with_label("Workflows");
    toolbar.append(&workflows);
    root.append(&toolbar);
    let browser = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    browser.set_hexpand(true);
    browser.set_vexpand(true);
    let (left_ui, left) = make_pane(&profiles);
    let (right_ui, right) = make_pane(&profiles);
    browser.append(&left_ui);
    let actions = gtk::Box::new(gtk::Orientation::Vertical, 8);
    actions.add_css_class("transfer-actions");
    let to_right = gtk::Button::with_label("→");
    to_right.set_tooltip_text(Some("Copy selected items to the right"));
    let to_left = gtk::Button::with_label("←");
    to_left.set_tooltip_text(Some("Copy selected items to the left"));
    actions.append(&to_right);
    actions.append(&to_left);
    browser.append(&actions);
    browser.append(&right_ui);
    root.append(&browser);
    let queue_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    queue_header.add_css_class("queue-head");
    let queue_head = gtk::Label::new(Some("TRANSFERS"));
    queue_head.set_xalign(0.0);
    queue_head.set_hexpand(true);
    queue_header.append(&queue_head);
    let clear_completed = gtk::Button::with_label("Clear completed");
    queue_header.append(&clear_completed);
    root.append(&queue_header);
    let queue = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let queue_scroll = gtk::ScrolledWindow::builder()
        .child(&queue)
        .min_content_height(80)
        .max_content_height(160)
        .vexpand(false)
        .build();
    queue_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
    root.append(&queue_scroll);
    let status = gtk::Label::new(Some("Choose a location in each pane."));
    status.add_css_class("transfer-status");
    status.set_xalign(0.0);
    root.append(&status);
    window.set_child(Some(&root));
    let state = Rc::new(RefCell::new(State {
        panes: vec![left, right],
        profiles,
        queue,
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
    selector.set_hexpand(true);
    controls.append(&selector);
    let up = gtk::Button::with_label("↑");
    up.set_tooltip_text(Some("Parent directory"));
    let refresh = gtk::Button::with_label("↻");
    refresh.set_tooltip_text(Some("Refresh"));
    controls.append(&up);
    controls.append(&refresh);
    box_.append(&controls);
    let path = gtk::Entry::new();
    path.set_text(&std::env::var("HOME").unwrap_or_else(|_| "/".into()));
    box_.append(&path);
    let filter_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let filter = gtk::SearchEntry::new();
    filter.set_placeholder_text(Some("Filter files"));
    filter.set_hexpand(true);
    let sort = gtk::DropDown::from_strings(&["Name", "Size", "Modified"]);
    sort.set_tooltip_text(Some("Sort directory"));
    filter_row.append(&filter);
    filter_row.append(&sort);
    box_.append(&filter_row);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Multiple);
    list.add_css_class("file-list");
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .build();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
    box_.append(&scroll);
    (
        box_,
        Pane {
            location: Location::Local(PathBuf::from(path.text().as_str())),
            entries: Vec::new(),
            selector,
            path,
            filter,
            sort,
            list,
            up,
            refresh,
        },
    )
}

fn connect_pane(state: Rc<RefCell<State>>, index: usize) {
    let (selector, path, filter, sort, list, up, reload) = {
        let s = state.borrow();
        let p = &s.panes[index];
        (
            p.selector.clone(),
            p.path.clone(),
            p.filter.clone(),
            p.sort.clone(),
            p.list.clone(),
            p.up.clone(),
            p.refresh.clone(),
        )
    };
    {
        let s = state.clone();
        selector.connect_selected_notify(move |selector| {
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
        });
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
        up.connect_clicked(move |_| {
            let next = s.borrow().panes[index].location.parent();
            navigate(s.clone(), index, next);
        });
    }
    {
        let s = state.clone();
        reload.connect_clicked(move |_| refresh(s.clone(), index));
    }
}

fn navigate(state: Rc<RefCell<State>>, index: usize, location: Location) {
    state.borrow_mut().panes[index].location = location;
    refresh(state, index);
}
fn refresh(state: Rc<RefCell<State>>, index: usize) {
    let (location, profiles, runtime, sender) = {
        let s = state.borrow();
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
        .filter(|e| e.name.to_lowercase().contains(&filter))
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
        let name = gtk::Label::new(Some(&entry.name));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&name);
        let size = gtk::Label::new(Some(&entry.size.map(format_size).unwrap_or_default()));
        size.add_css_class("dim-label");
        line.append(&size);
        let date_text = entry
            .modified
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(10)
            .collect::<String>();
        let date = gtk::Label::new(Some(&date_text));
        date.add_css_class("dim-label");
        line.append(&date);
        row.set_child(Some(&line));
        pane.list.append(&row);
    }
}
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
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
    let ids: std::collections::HashSet<_> = snapshots.iter().map(|j| j.id.clone()).collect();
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
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line.add_css_class("job-row");
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
            let pause = gtk::Button::with_label("Pause");
            let cancel = gtk::Button::with_label("Cancel");
            let retry = gtk::Button::with_label("Retry");
            line.append(&title);
            line.append(&status);
            line.append(&pause);
            line.append(&cancel);
            line.append(&retry);
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
                    retry,
                    pause,
                    cancel,
                    state: snapshot.status.clone(),
                },
            );
        }
        if let Some(view) = state.borrow_mut().jobs.get_mut(&snapshot.id) {
            view.state = snapshot.status.clone();
            view.status.set_text(&format!(
                "{} · retry {}",
                snapshot.progress, snapshot.retries
            ));
            view.pause
                .set_label(if snapshot.status == JobStatus::Paused {
                    "Resume"
                } else {
                    "Pause"
                });
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
                state.borrow().status.set_text("Transfer queued.");
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
    form.append(&direction);
    let mode = gtk::DropDown::from_strings(&[
        "Copy new and changed files",
        "Mirror (delete destination extras)",
    ]);
    form.append(&mode);
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
    form.append(&selector);
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
        .build();
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
    form.append(&protocol);
    let host = field(&form, "Host or WebDAV URL", "example.com");
    let port = field(&form, "Port", "22");
    port.set_text("22");
    let user = field(&form, "Username", "");
    let auth = gtk::DropDown::from_strings(&["SSH agent", "SSH key", "Password"]);
    form.append(&auth);
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
    form.append(&save);
    dialog.set_child(Some(&form));
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
                    let path = nixe_core::paths::data_dir().join("ssh/known_hosts");
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
    let title = gtk::Label::new(Some(label));
    title.set_xalign(0.0);
    title.add_css_class("dim-label");
    parent.append(&title);
    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some(placeholder));
    parent.append(&entry);
    entry
}
