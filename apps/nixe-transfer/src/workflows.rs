use super::*;
use transfer_core::{ConflictPolicy, SyncWorkflow, WorkflowPreview};

pub(super) struct WorkflowDialog {
    window: gtk::Window,
    request_id: String,
    workflow: Option<SyncWorkflow>,
    preview: Option<WorkflowPreview>,
    report: gtk::TextView,
    status: gtk::Label,
    queue: gtk::Button,
}

struct EditorFields {
    base: SyncWorkflow,
    name: gtk::Entry,
    direction: gtk::DropDown,
    mode: gtk::DropDown,
    include: gtk::TextView,
    exclude: gtk::TextView,
    conflict: gtk::DropDown,
    checksum: gtk::CheckButton,
    bandwidth: gtk::Entry,
    retry_limit: gtk::Entry,
}

impl EditorFields {
    fn read(&self) -> Result<SyncWorkflow, String> {
        let mut workflow = self.base.clone();
        workflow.name = self.name.text().trim().to_owned();
        if self.direction.selected() == 1 {
            std::mem::swap(&mut workflow.source, &mut workflow.destination);
        }
        workflow.mode = match self.mode.selected() {
            0 => SyncMode::Copy,
            1 => SyncMode::Mirror,
            _ => SyncMode::Bidirectional,
        };
        workflow.include = patterns(&self.include);
        workflow.exclude = patterns(&self.exclude);
        workflow.conflict = match self.conflict.selected() {
            1 => ConflictPolicy::PreferLeft,
            2 => ConflictPolicy::PreferRight,
            3 => ConflictPolicy::PreferNewer,
            _ => ConflictPolicy::KeepBoth,
        };
        workflow.checksum = self.checksum.is_active();
        workflow.bandwidth_kib = if self.bandwidth.text().trim().is_empty() {
            None
        } else {
            Some(
                self.bandwidth
                    .text()
                    .trim()
                    .parse()
                    .map_err(|_| "Enter a bandwidth limit in KiB/s".to_owned())?,
            )
        };
        workflow.retry_limit = self
            .retry_limit
            .text()
            .trim()
            .parse()
            .map_err(|_| "Enter a retry count from 0 to 10".to_owned())?;
        workflow.validate().map_err(|e| e.to_string())?;
        Ok(workflow)
    }
}

fn patterns(view: &gtk::TextView) -> Vec<String> {
    let buffer = view.buffer();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(super) fn manager(parent: &gtk::ApplicationWindow, state: Rc<RefCell<State>>) {
    let workflows = match store::load_workflows() {
        Ok(values) => values,
        Err(e) => {
            state.borrow().status.set_text(&e.to_string());
            return;
        }
    };
    let window = gtk::Window::builder()
        .title("Workflows")
        .transient_for(parent)
        .modal(true)
        .default_width(500)
        .default_height(440)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    root.set_margin_start(16);
    root.set_margin_end(16);
    let intro = gtk::Label::new(Some("Saved folder synchronization jobs"));
    intro.set_xalign(0.0);
    root.append(&intro);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    for workflow in &workflows {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&workflow.id);
        let label = gtk::Label::new(Some(&format!("{} · {:?}", workflow.name, workflow.mode)));
        label.set_xalign(0.0);
        label.set_margin_top(8);
        label.set_margin_bottom(8);
        row.set_child(Some(&label));
        list.append(&row);
    }
    root.append(
        &gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .build(),
    );
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let create = gtk::Button::with_label("New from panes");
    let open = gtk::Button::with_label("Open");
    let delete = gtk::Button::with_label("Delete");
    actions.append(&create);
    actions.append(&open);
    actions.append(&delete);
    root.append(&actions);
    window.set_child(Some(&root));
    {
        let parent = parent.clone();
        let s = state.clone();
        let window = window.clone();
        create.connect_clicked(move |_| {
            window.close();
            editor(&parent, s.clone(), None);
        });
    }
    {
        let parent = parent.clone();
        let s = state.clone();
        let window = window.clone();
        let list = list.clone();
        open.connect_clicked(move |_| {
            let Some(row) = list.selected_row() else {
                return;
            };
            match store::get_workflow(row.widget_name().as_str()) {
                Ok(Some(workflow)) => {
                    window.close();
                    editor(&parent, s.clone(), Some(workflow));
                }
                Ok(None) => s.borrow().status.set_text("Workflow no longer exists."),
                Err(e) => s.borrow().status.set_text(&e.to_string()),
            }
        });
    }
    {
        let list = list.clone();
        let window = window.clone();
        let s = state.clone();
        delete.connect_clicked(move |_| {
            let Some(row) = list.selected_row() else {
                return;
            };
            let id = row.widget_name().to_string();
            let list = list.clone();
            let window = window.clone();
            let s = s.clone();
            glib::spawn_future_local(async move {
                let prompt = gtk::AlertDialog::builder()
                    .message("Delete this workflow?")
                    .detail("Queued runs of this workflow will stop. Transfer history is retained.")
                    .buttons(["Cancel", "Delete"])
                    .cancel_button(0)
                    .default_button(0)
                    .build();
                if prompt.choose_future(Some(&window)).await != Ok(1) {
                    return;
                }
                match store::delete_workflow(&id) {
                    Ok(()) => {
                        list.remove(&row);
                        s.borrow().status.set_text("Workflow deleted.");
                    }
                    Err(e) => s.borrow().status.set_text(&e.to_string()),
                }
            });
        });
    }
    window.present();
}

fn text_area(root: &gtk::Box, label: &str, initial: &[String]) -> gtk::TextView {
    let title = gtk::Label::new(Some(label));
    title.set_xalign(0.0);
    root.append(&title);
    let view = gtk::TextView::new();
    view.set_monospace(true);
    view.buffer().set_text(&initial.join("\n"));
    root.append(
        &gtk::ScrolledWindow::builder()
            .child(&view)
            .min_content_height(65)
            .build(),
    );
    view
}

fn editor(parent: &gtk::ApplicationWindow, state: Rc<RefCell<State>>, saved: Option<SyncWorkflow>) {
    if let Some(existing) = &state.borrow().workflow_dialog {
        existing.window.present();
        return;
    }
    let base = saved.unwrap_or_else(|| {
        let s = state.borrow();
        SyncWorkflow {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            source: s.panes[0].location.clone(),
            destination: s.panes[1].location.clone(),
            mode: SyncMode::Copy,
            include: Vec::new(),
            exclude: Vec::new(),
            conflict: ConflictPolicy::KeepBoth,
            checksum: false,
            bandwidth_kib: None,
            baseline_ready: false,
            retry_limit: 2,
        }
    });
    let window = gtk::Window::builder()
        .title("Workflow")
        .transient_for(parent)
        .modal(true)
        .default_width(670)
        .default_height(790)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    root.set_margin_start(16);
    root.set_margin_end(16);
    let name = field(&root, "Workflow name", "Homelab backup");
    name.set_text(&base.name);
    let labels = gtk::Label::new(Some(&format!(
        "A: {}\nB: {}",
        location_name(&base.source, &state.borrow().profiles),
        location_name(&base.destination, &state.borrow().profiles)
    )));
    labels.set_xalign(0.0);
    labels.set_wrap(true);
    root.append(&labels);
    let direction = gtk::DropDown::from_strings(&["A → B", "B → A"]);
    root.append(&direction);
    let mode = gtk::DropDown::from_strings(&[
        "Copy new and changed files",
        "Mirror destination",
        "Bidirectional",
    ]);
    mode.set_selected(match base.mode {
        SyncMode::Copy => 0,
        SyncMode::Mirror => 1,
        SyncMode::Bidirectional => 2,
    });
    root.append(&mode);
    let include = text_area(
        &root,
        "Include patterns (one rclone pattern per line)",
        &base.include,
    );
    let exclude = text_area(
        &root,
        "Exclude patterns (one rclone pattern per line)",
        &base.exclude,
    );
    let conflict_label = gtk::Label::new(Some("Bidirectional conflict handling"));
    conflict_label.set_xalign(0.0);
    root.append(&conflict_label);
    let conflict =
        gtk::DropDown::from_strings(&["Keep both", "Prefer A", "Prefer B", "Prefer newer"]);
    conflict.set_selected(match base.conflict {
        ConflictPolicy::KeepBoth => 0,
        ConflictPolicy::PreferLeft => 1,
        ConflictPolicy::PreferRight => 2,
        ConflictPolicy::PreferNewer => 3,
    });
    root.append(&conflict);
    let checksum = gtk::CheckButton::with_label("Compare checksums when available");
    checksum.set_active(base.checksum);
    root.append(&checksum);
    let bandwidth = field(&root, "Bandwidth limit in KiB/s (optional)", "Unlimited");
    if let Some(limit) = base.bandwidth_kib {
        bandwidth.set_text(&limit.to_string());
    }
    let retry_limit = field(&root, "Network retries (0–10)", "2");
    retry_limit.set_text(&base.retry_limit.to_string());
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let save = gtk::Button::with_label("Save workflow");
    let preview_button = gtk::Button::with_label("Preview");
    actions.append(&save);
    actions.append(&preview_button);
    root.append(&actions);
    let status = gtk::Label::new(Some("Save, then preview before queueing."));
    status.set_xalign(0.0);
    status.set_wrap(true);
    root.append(&status);
    let report = gtk::TextView::new();
    report.set_monospace(true);
    report.set_editable(false);
    report.set_cursor_visible(false);
    root.append(
        &gtk::ScrolledWindow::builder()
            .child(&report)
            .min_content_height(170)
            .vexpand(true)
            .build(),
    );
    let queue = gtk::Button::with_label("Queue workflow");
    queue.set_sensitive(false);
    root.append(&queue);
    window.set_child(Some(&gtk::ScrolledWindow::builder().child(&root).build()));
    let fields = Rc::new(EditorFields {
        base: base.clone(),
        name: name.clone(),
        direction: direction.clone(),
        mode: mode.clone(),
        include: include.clone(),
        exclude: exclude.clone(),
        conflict: conflict.clone(),
        checksum: checksum.clone(),
        bandwidth: bandwidth.clone(),
        retry_limit: retry_limit.clone(),
    });
    state.borrow_mut().workflow_dialog = Some(WorkflowDialog {
        window: window.clone(),
        request_id: String::new(),
        workflow: store::get_workflow(&base.id).ok().flatten(),
        preview: None,
        report: report.clone(),
        status: status.clone(),
        queue: queue.clone(),
    });
    let dirty: Rc<dyn Fn()> = {
        let s = state.clone();
        Rc::new(move || {
            if let Some(dialog) = s.borrow_mut().workflow_dialog.as_mut() {
                dialog.preview = None;
                dialog.queue.set_sensitive(false);
                dialog.status.set_text("Save changes, then preview again.");
                dialog.report.buffer().set_text("");
            }
        })
    };
    for entry in [&name, &bandwidth, &retry_limit] {
        let dirty = dirty.clone();
        entry.connect_changed(move |_| dirty());
    }
    for dropdown in [&direction, &mode, &conflict] {
        let dirty = dirty.clone();
        dropdown.connect_selected_notify(move |_| dirty());
    }
    for view in [&include, &exclude] {
        let dirty = dirty.clone();
        view.buffer().connect_changed(move |_| dirty());
    }
    {
        let dirty = dirty.clone();
        checksum.connect_toggled(move |_| dirty());
    }
    {
        let s = state.clone();
        let fields = fields.clone();
        save.connect_clicked(move |_| {
            let candidate = match fields.read() {
                Ok(value) => value,
                Err(e) => {
                    s.borrow()
                        .workflow_dialog
                        .as_ref()
                        .unwrap()
                        .status
                        .set_text(&e);
                    return;
                }
            };
            match store::save_workflow(&candidate) {
                Ok(saved) => {
                    let mut state = s.borrow_mut();
                    let dialog = state.workflow_dialog.as_mut().unwrap();
                    dialog.workflow = Some(saved);
                    dialog.preview = None;
                    dialog.queue.set_sensitive(false);
                    dialog.report.buffer().set_text("");
                    dialog
                        .status
                        .set_text("Saved. Preview the workflow before queueing.");
                }
                Err(e) => s
                    .borrow()
                    .workflow_dialog
                    .as_ref()
                    .unwrap()
                    .status
                    .set_text(&e.to_string()),
            }
        });
    }
    {
        let s = state.clone();
        let fields = fields.clone();
        preview_button.connect_clicked(move |_| {
            let candidate = match fields.read() {
                Ok(value) => value,
                Err(e) => {
                    s.borrow()
                        .workflow_dialog
                        .as_ref()
                        .unwrap()
                        .status
                        .set_text(&e);
                    return;
                }
            };
            let (id, request_id, runtime, client, sender) = {
                let mut state = s.borrow_mut();
                let Some(dialog) = state.workflow_dialog.as_mut() else {
                    return;
                };
                let Some(saved) = &dialog.workflow else {
                    dialog.status.set_text("Save the workflow first.");
                    return;
                };
                if !candidate.same_plan(saved) || candidate.name != saved.name {
                    dialog.status.set_text("Save changes before previewing.");
                    return;
                }
                let id = saved.id.clone();
                let request_id = uuid::Uuid::new_v4().to_string();
                dialog.request_id = request_id.clone();
                dialog.preview = None;
                dialog.queue.set_sensitive(false);
                dialog.status.set_text("Comparing folders…");
                dialog.report.buffer().set_text("");
                (
                    id,
                    request_id,
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
                        .preview_workflow(&id)
                        .await
                        .map_err(|e| e.to_string())
                }
                .await;
                let _ = sender.send(Message::WorkflowPreview(request_id, result));
            });
        });
    }
    {
        let s = state.clone();
        let window = window.clone();
        queue.connect_clicked(move |_| {
            let chosen = s.borrow().workflow_dialog.as_ref().and_then(|dialog| {
                dialog.workflow.as_ref().zip(dialog.preview.as_ref()).map(|(workflow, preview)| (workflow.clone(), preview.clone()))
            });
            let Some((workflow, preview)) = chosen else { return };
            let s = s.clone(); let window = window.clone();
            glib::spawn_future_local(async move {
                if workflow.mode != SyncMode::Copy {
                    let detail = if workflow.mode == SyncMode::Bidirectional {
                        if preview.initial { "First run establishes change tracking and may replace same-name files according to the selected preference. Review the preview carefully.".to_owned() }
                        else { "Edits and deletions can propagate in both directions. Conflicts follow the saved policy.".to_owned() }
                    } else { format!("The preview lists {} destination files to delete.", preview.deletions) };
                    let prompt = gtk::AlertDialog::builder().message("Queue synchronization?")
                        .detail(detail).buttons(["Cancel", "Queue workflow"])
                        .cancel_button(0).default_button(0).build();
                    if prompt.choose_future(Some(&window)).await != Ok(1) { return; }
                }
                let (runtime, client, sender) = { let state = s.borrow(); (state.runtime.clone(), state.client.clone(), state.sender.clone()) };
                let id = workflow.id; let fingerprint = preview.fingerprint;
                runtime.spawn(async move {
                    let result = async {
                        let client = client.get_or_try_init(transfer_ipc::Client::connect).await.map_err(|e| e.to_string())?;
                        client.enqueue_workflow(&id, &fingerprint).await.map_err(|e| e.to_string())?;
                        Ok(())
                    }.await;
                    let _ = sender.send(Message::QueueResult(result));
                });
                window.close();
            });
        });
    }
    {
        let s = state.clone();
        window.connect_hide(move |_| {
            s.borrow_mut().workflow_dialog = None;
        });
    }
    window.present();
}

pub(super) fn handle_preview(
    state: &Rc<RefCell<State>>,
    request_id: &str,
    result: Result<WorkflowPreview, String>,
) {
    let mut state = state.borrow_mut();
    let Some(dialog) = state
        .workflow_dialog
        .as_mut()
        .filter(|dialog| dialog.request_id == request_id)
    else {
        return;
    };
    match result {
        Ok(preview) => {
            dialog.status.set_text(&format!(
                "{} preview entries{}",
                preview.lines.len(),
                if preview.initial {
                    " · initial bidirectional run"
                } else {
                    ""
                }
            ));
            dialog.report.buffer().set_text(&preview.lines.join("\n"));
            dialog.queue.set_sensitive(true);
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
