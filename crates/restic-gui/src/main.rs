use adw::prelude::*;
use gtk::glib;
use restic_core::{Engine, Event, Outcome, Profile, Repository, Snapshot};
use std::{
    cell::{Cell, RefCell},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

type SelectedDestination = Rc<RefCell<Option<(PathBuf, u64)>>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Operation {
    Init,
    Snapshots,
    Backup,
    Check,
    Restore,
    Preview,
}
impl Operation {
    /// Long-running operations get a modal progress window with this title.
    fn progress_title(self) -> Option<&'static str> {
        match self {
            Self::Backup => Some("Backing up"),
            Self::Restore => Some("Restoring"),
            Self::Preview => Some("Previewing backup"),
            Self::Init | Self::Snapshots | Self::Check => None,
        }
    }
}

/// The profile a backup runs: the loaded one while its repository and first source are
/// unchanged, so its saved sources, excludes and options apply; otherwise a one-off profile.
fn backup_profile(loaded: Option<&Profile>, repository: &str, source: &str) -> Profile {
    if let Some(profile) = loaded
        && profile.repository.location == repository
        && profile
            .sources
            .first()
            .is_some_and(|first| first.as_os_str() == source)
    {
        return profile.clone();
    }
    Profile {
        id: "manual".into(),
        name: "Manual backup".into(),
        repository: Repository {
            location: repository.into(),
        },
        sources: vec![source.into()],
        excludes: vec![],
        one_file_system: false,
    }
}

enum OperationUpdate {
    Progress(String, Option<f64>),
    Done(String, Vec<Snapshot>, Option<Outcome>),
}

fn progress_status(value: &serde_json::Value, restoring: bool) -> Option<(String, Option<f64>)> {
    if value["message_type"] != "status" {
        return None;
    }
    let files_done = value[if restoring {
        "files_restored"
    } else {
        "files_done"
    }]
    .as_u64()
    .unwrap_or(0);
    let bytes_done = value[if restoring {
        "bytes_restored"
    } else {
        "bytes_done"
    }]
    .as_u64()
    .unwrap_or(0);
    let total_files = value["total_files"].as_u64().filter(|n| *n > 0);
    let total_bytes = value["total_bytes"].as_u64().filter(|n| *n > 0);
    let files = total_files.map_or_else(
        || files_done.to_string(),
        |total| format!("{files_done} of {total}"),
    );
    let bytes = total_bytes.map_or_else(
        || glib::format_size(bytes_done).to_string(),
        |total| {
            format!(
                "{} of {}",
                glib::format_size(bytes_done),
                glib::format_size(total)
            )
        },
    );
    let fraction = total_bytes
        .and_then(|_| value["percent_done"].as_f64())
        .filter(|n| n.is_finite() && (0.0..=1.0).contains(n));
    Some((
        format!(
            "{} {files} files • {bytes}",
            if restoring { "Restored" } else { "Processed" }
        ),
        fraction,
    ))
}

struct ProgressWindow {
    window: gtk::Window,
    status: gtk::Label,
    bar: gtk::ProgressBar,
    stop: gtk::Button,
    close: gtk::Button,
    finished: Rc<Cell<bool>>,
}

fn progress_window(
    parent: &adw::ApplicationWindow,
    title: &str,
    cancel: Arc<AtomicBool>,
) -> ProgressWindow {
    let window = gtk::Window::builder()
        .title(title)
        .transient_for(parent)
        .modal(true)
        .default_width(480)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    let status = gtk::Label::new(Some("Preparing…"));
    status.set_wrap(true);
    status.set_xalign(0.0);
    status.set_selectable(true);
    let status_scroll = gtk::ScrolledWindow::builder()
        .max_content_height(240)
        .propagate_natural_height(true)
        .child(&status)
        .build();
    content.append(&status_scroll);
    let bar = gtk::ProgressBar::new();
    bar.set_show_text(true);
    bar.set_text(Some("Waiting for Restic…"));
    content.append(&bar);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let stop = gtk::Button::with_label("Stop safely");
    let close = gtk::Button::with_label("Close");
    close.set_sensitive(false);
    actions.append(&stop);
    actions.append(&close);
    content.append(&actions);
    window.set_child(Some(&content));
    let finished = Rc::new(Cell::new(false));
    let can_close = finished.clone();
    window.connect_close_request(move |_| {
        if can_close.get() {
            glib::Propagation::Proceed
        } else {
            glib::Propagation::Stop
        }
    });
    let stop_status = status.clone();
    stop.connect_clicked(move |button| {
        cancel.store(true, Ordering::Relaxed);
        button.set_sensitive(false);
        stop_status.set_text("Stopping safely… waiting for Restic to finish cleanup.");
    });
    let dismiss = window.clone();
    close.connect_clicked(move |_| dismiss.close());
    window.present();
    ProgressWindow {
        window,
        status,
        bar,
        stop,
        close,
        finished,
    }
}

fn destination_available(selection: &SelectedDestination) -> bool {
    selection.borrow().as_ref().is_none_or(|(path, device)| {
        std::fs::metadata(path).is_ok_and(|metadata| metadata.dev() == *device)
    })
}

fn source_repository_overlap(source: &str, repository: &str) -> bool {
    let Some(source) = PathBuf::from(source).canonicalize().ok() else {
        return false;
    };
    let repository = PathBuf::from(repository);
    let repository = repository.canonicalize().ok().or_else(|| {
        Some(
            repository
                .parent()?
                .canonicalize()
                .ok()?
                .join(repository.file_name()?),
        )
    });
    repository.is_some_and(|repo| {
        !(source == std::path::Path::new("/") && repo != source)
            && (source.starts_with(&repo) || repo.starts_with(&source))
    })
}

fn repository_in(folder: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if folder.join("config").is_file() {
        return Ok(folder.to_path_buf());
    }
    let mut repositories = std::fs::read_dir(folder)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("config").is_file());
    match (repositories.next(), repositories.next()) {
        (Some(repository), None) => Ok(repository),
        (Some(_), Some(_)) => {
            Err("Several repositories found. Select the repository folder itself.".into())
        }
        _ => Ok(folder.join("restic-studio")),
    }
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.resticstudio.Studio")
        .build();
    app.connect_activate(build);
    app.run()
}
/// A file-safe profile ID from a display name; None when nothing usable is left.
fn profile_id(name: &str) -> Option<String> {
    let mut id = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c.to_ascii_lowercase());
        } else if !id.ends_with('-') {
            id.push('-');
        }
    }
    let id: String = id.trim_matches('-').chars().take(64).collect();
    let id = id.trim_end_matches('-').to_string();
    (!id.is_empty()).then_some(id)
}
/// Reload the saved-profile list; entry 0 is always "No profile selected".
fn refresh_profiles(
    store: &std::path::Path,
    profiles: &RefCell<Vec<Profile>>,
    names: &gtk::StringList,
    dropdown: &gtk::DropDown,
    select: Option<&str>,
) -> Option<String> {
    let listed = restic_core::list_profiles(store);
    let (found, problem) = match listed {
        Ok(list) => (
            list.profiles,
            (!list.problems.is_empty())
                .then(|| format!("{} profile file(s) could not be read.", list.problems.len())),
        ),
        Err(error) => (Vec::new(), Some(format!("Profiles unavailable: {error:#}"))),
    };
    let labels: Vec<&str> = std::iter::once("No profile selected")
        .chain(found.iter().map(|p| p.name.as_str()))
        .collect();
    names.splice(0, names.n_items(), &labels);
    let position = select
        .and_then(|id| found.iter().position(|p| p.id == id))
        .map_or(0, |i| i as u32 + 1);
    *profiles.borrow_mut() = found;
    dropdown.set_selected(position);
    problem
}
fn field(container: &gtk::Box, title: &str, placeholder: &str) -> gtk::Entry {
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    let entry = gtk::Entry::builder()
        .placeholder_text(placeholder)
        .hexpand(true)
        .build();
    container.append(&label);
    container.append(&entry);
    entry
}
fn choose(parent: &adw::ApplicationWindow, entry: &gtk::Entry) {
    let chooser = gtk::FileDialog::builder().title("Select folder").build();
    let entry = entry.clone();
    chooser.select_folder(
        Some(parent),
        None::<&gtk::gio::Cancellable>,
        move |result| {
            if let Ok(file) = result
                && let Some(path) = file.path()
            {
                entry.set_text(&path.to_string_lossy());
            }
        },
    );
}
fn choose_destination(
    parent: &adw::ApplicationWindow,
    entry: &gtk::Entry,
    selected: SelectedDestination,
) {
    let chooser = gtk::FileDialog::builder()
        .title("Choose a folder on your external drive")
        .build();
    let entry = entry.clone();
    chooser.select_folder(
        Some(parent),
        None::<&gtk::gio::Cancellable>,
        move |result| {
            if let Ok(file) = result
                && let Some(folder) = file.path()
            {
                match restic_core::ensure_mounted(&folder)
                    .map_err(|error| error.to_string())
                    .and_then(|_| repository_in(&folder))
                {
                    Ok(repository) => {
                        entry.set_text(&repository.to_string_lossy());
                        *selected.borrow_mut() = std::fs::metadata(&folder)
                            .ok()
                            .map(|metadata| (folder, metadata.dev()));
                    }
                    Err(error) => {
                        entry.set_text("");
                        entry.set_placeholder_text(Some(&error));
                    }
                }
            }
        },
    );
}
fn build(app: &adw::Application) {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("Restic Studio must not run as root");
        return;
    }
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Restic Studio")
        .default_width(1000)
        .default_height(760)
        .build();
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    let theme = gtk::DropDown::from_strings(&["System theme", "Light", "Dark"]);
    theme.connect_selected_notify(|theme| {
        adw::StyleManager::default().set_color_scheme(match theme.selected() {
            1 => adw::ColorScheme::ForceLight,
            2 => adw::ColorScheme::ForceDark,
            _ => adw::ColorScheme::Default,
        })
    });
    header.pack_end(&theme);
    outer.append(&header);
    // One quick `restic version` call; a hung Restic would stall startup (ponytail: move off-thread if so).
    let problems = Engine::default().probe().problems;
    let status = gtk::Label::new(Some(&if problems.is_empty() {
        "Ready. Health has not been verified.".to_string()
    } else {
        format!("Cannot back up on this system:\n{}", problems.join("\n"))
    }));
    status.set_wrap(true);
    status.set_xalign(0.0);
    status.set_selectable(true);
    status.set_margin_start(20);
    status.set_margin_end(20);
    status.set_margin_top(8);
    status.set_margin_bottom(8);
    outer.append(&status);
    let scroll = gtk::ScrolledWindow::builder().vexpand(true).build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_start(28);
    content.set_margin_end(28);
    content.set_margin_top(20);
    content.set_margin_bottom(20);
    let title = gtk::Label::new(Some("Back up. Verify. Recover."));
    title.add_css_class("title-1");
    title.set_xalign(0.0);
    content.append(&title);
    let notice = gtk::Label::new(Some(
        "Local backup and recovery • Restore always creates a new folder",
    ));
    notice.set_xalign(0.0);
    content.append(&notice);
    let profile_store = restic_core::state_dir().map(|dir| dir.join("profiles"));
    let profiles: Rc<RefCell<Vec<Profile>>> = Rc::default();
    let loaded_profile: Rc<RefCell<Option<Profile>>> = Rc::default();
    let profile_names = gtk::StringList::new(&[]);
    let profile_menu = gtk::DropDown::new(Some(profile_names.clone()), gtk::Expression::NONE);
    profile_menu.set_hexpand(true);
    let profile_save = gtk::Button::with_label("Save profile");
    let profile_delete = gtk::Button::with_label("Delete profile");
    let profile_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let profile_label = gtk::Label::new(Some(
        "Saved profile (settings only; passwords are never saved)",
    ));
    profile_label.set_xalign(0.0);
    profile_row.append(&profile_menu);
    profile_row.append(&profile_save);
    profile_row.append(&profile_delete);
    content.append(&profile_label);
    content.append(&profile_row);
    let profile_name = field(
        &content,
        "Profile name",
        "Name to save these settings under",
    );
    let repo = field(&content, "Repository folder", "/path/to/backup-repository");
    let selected_destination: SelectedDestination = Rc::new(RefCell::new(None));
    let destination_picker = gtk::Button::with_label("Choose external drive or destination…");
    destination_picker.set_tooltip_text(Some("Select a mounted drive folder. Restic Studio uses a dedicated restic-studio folder unless you select an existing repository."));
    let w = window.clone();
    let r = repo.clone();
    let chosen = selected_destination.clone();
    destination_picker.connect_clicked(move |_| choose_destination(&w, &r, chosen.clone()));
    content.append(&destination_picker);
    let source_placeholder = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let source = field(
        &source_placeholder,
        "Folder to back up",
        "/path/to/documents",
    );
    let pick = gtk::Button::with_label("Choose source folder…");
    let w = window.clone();
    let s = source.clone();
    pick.connect_clicked(move |_| choose(&w, &s));
    source_placeholder.append(&pick);
    match &profile_store {
        Ok(store) => {
            if let Some(problem) =
                refresh_profiles(store, &profiles, &profile_names, &profile_menu, None)
            {
                status.set_text(&problem);
            }
            let loading = Rc::new(Cell::new(false));
            {
                let (profiles, loaded_profile, repo, source, profile_name, status, loading) = (
                    profiles.clone(),
                    loaded_profile.clone(),
                    repo.clone(),
                    source.clone(),
                    profile_name.clone(),
                    status.clone(),
                    loading.clone(),
                );
                profile_menu.connect_selected_notify(move |menu| {
                    let index = menu.selected();
                    if loading.get() {
                        return;
                    }
                    if index == 0 {
                        *loaded_profile.borrow_mut() = None;
                        return;
                    }
                    if let Some(profile) = profiles.borrow().get(index as usize - 1) {
                        *loaded_profile.borrow_mut() = Some(profile.clone());
                        repo.set_text(&profile.repository.location);
                        source.set_text(
                            &profile
                                .sources
                                .first()
                                .map(|p| p.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        );
                        profile_name.set_text(&profile.name);
                        status.set_text(&if profile.sources.len() > 1 {
                            format!("Loaded profile {}. Back Up Now includes all {} of its sources; only the first is shown, and editing these fields switches to a one-off manual backup.", profile.name, profile.sources.len())
                        } else {
                            format!("Loaded profile {}. Enter the repository password to use it.", profile.name)
                        });
                    }
                });
            }
            {
                let loaded_profile = loaded_profile.clone();
                let (store, profiles, names, menu, repo, source, profile_name, status, loading) = (
                    store.clone(),
                    profiles.clone(),
                    profile_names.clone(),
                    profile_menu.clone(),
                    repo.clone(),
                    source.clone(),
                    profile_name.clone(),
                    status.clone(),
                    loading.clone(),
                );
                profile_save.connect_clicked(move |_| {
                    let name = profile_name.text().trim().to_string();
                    let Some(id) = profile_id(&name) else {
                        status.set_text("Give the profile a name with at least one letter or digit.");
                        return;
                    };
                    let existing = profiles.borrow().iter().find(|p| p.id == id).cloned();
                    if existing.as_ref().is_some_and(|p| p.sources.len() > 1) {
                        status.set_text("That profile has several sources; saving here would drop them. Edit it with the runner.");
                        return;
                    }
                    let profile = Profile {
                        id: id.clone(),
                        name,
                        repository: Repository {
                            location: repo.text().to_string(),
                        },
                        sources: vec![PathBuf::from(source.text().as_str())],
                        excludes: existing.as_ref().map(|p| p.excludes.clone()).unwrap_or_default(),
                        one_file_system: existing.as_ref().is_some_and(|p| p.one_file_system),
                    };
                    match restic_core::save_profile(&profile, &store) {
                        Ok(()) => {
                            loading.set(true);
                            refresh_profiles(&store, &profiles, &names, &menu, Some(&id));
                            loading.set(false);
                            status.set_text(&format!("Saved profile {}. The password was not saved.", profile.name));
                            *loaded_profile.borrow_mut() = Some(profile);
                        }
                        Err(error) => status.set_text(&format!("Could not save profile: {error:#}")),
                    }
                });
            }
            {
                let loaded_profile = loaded_profile.clone();
                let (store, profiles, names, menu, status, loading, window) = (
                    store.clone(),
                    profiles.clone(),
                    profile_names.clone(),
                    profile_menu.clone(),
                    status.clone(),
                    loading.clone(),
                    window.clone(),
                );
                profile_delete.connect_clicked(move |_| {
                    let index = menu.selected();
                    let Some(profile) = (index > 0)
                        .then(|| profiles.borrow().get(index as usize - 1).cloned())
                        .flatten()
                    else {
                        status.set_text("Select a saved profile to delete.");
                        return;
                    };
                    let dialog = adw::AlertDialog::new(
                        Some(&format!("Delete profile “{}”?", profile.name)),
                        Some("Only the saved settings are removed. The repository and its snapshots are not touched."),
                    );
                    dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete profile")]);
                    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
                    dialog.set_default_response(Some("cancel"));
                    dialog.set_close_response("cancel");
                    let (store, profiles, loaded_profile, names, menu, status, loading) = (
                        store.clone(),
                        profiles.clone(),
                        loaded_profile.clone(),
                        names.clone(),
                        menu.clone(),
                        status.clone(),
                        loading.clone(),
                    );
                    dialog.choose(Some(&window), None::<&gtk::gio::Cancellable>, move |response| {
                        if response != "delete" {
                            return;
                        }
                        match restic_core::delete_profile(&store, &profile.id) {
                            Ok(()) => {
                                loading.set(true);
                                refresh_profiles(&store, &profiles, &names, &menu, None);
                                loading.set(false);
                                *loaded_profile.borrow_mut() = None;
                                status.set_text(&format!("Deleted profile {}.", profile.name));
                            }
                            Err(error) => status.set_text(&format!("Could not delete profile: {error:#}")),
                        }
                    });
                });
            }
        }
        Err(error) => {
            profile_menu.set_sensitive(false);
            profile_save.set_sensitive(false);
            profile_delete.set_sensitive(false);
            status.set_text(&format!("Profiles unavailable: {error:#}"));
        }
    }
    let password = gtk::PasswordEntry::builder()
        .placeholder_text("Repository password — kept only for this operation")
        .show_peek_icon(true)
        .build();
    password.set_tooltip_text(Some(
        "Lost passwords can make recovery impossible. No password is saved.",
    ));
    content.append(&password);
    let init_button = gtk::Button::with_label("Initialize empty repository");
    let snapshots_button = gtk::Button::with_label("Open / refresh snapshots");
    let backup_button = gtk::Button::with_label("Back Up Now");
    let check_button = gtk::Button::with_label("Check repository");
    backup_button.add_css_class("suggested-action");
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    let switcher = gtk::StackSwitcher::builder()
        .stack(&stack)
        .halign(gtk::Align::Center)
        .build();
    content.append(&switcher);
    let backup_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    backup_page.append(&source_placeholder);
    let root_backup_note = gtk::Label::new(Some(
        "When backing up /, the repository folder plus virtual and temporary system folders are skipped. /home is skipped only when this repository already has a home snapshot from this host.",
    ));
    root_backup_note.set_wrap(true);
    root_backup_note.set_xalign(0.0);
    backup_page.append(&root_backup_note);
    let restore_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let backup_actions = gtk::Box::new(gtk::Orientation::Vertical, 8);
    for button in [&init_button, &backup_button, &check_button] {
        backup_actions.append(button);
    }
    let preview = gtk::Button::with_label("Preview backup (no snapshot)");
    backup_actions.append(&preview);
    backup_page.append(&backup_actions);
    restore_page.append(&snapshots_button);
    stack.add_titled(&backup_page, Some("backup"), "Backup");
    stack.add_titled(&restore_page, Some("restore"), "Restore");
    content.append(&stack);
    let snapshot = field(&restore_page, "Snapshot ID", "Select a snapshot below");
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    restore_page.append(&list);
    let selected_path = field(
        &restore_page,
        "File or directory inside snapshot",
        "/absolute/path/as/shown/in/snapshot",
    );
    let directory = field(&restore_page, "Browse snapshot directory", "/");
    directory.set_text("/");
    let browse = gtk::Button::with_label("Browse directory");
    restore_page.append(&browse);
    let files = gtk::ListBox::new();
    files.set_selection_mode(gtk::SelectionMode::Single);
    restore_page.append(&files);
    let selected = selected_path.clone();
    files.connect_row_selected(move |_, row| {
        if let Some(label) = row.and_then(|row| row.child()).and_downcast::<gtk::Label>() {
            selected.set_text(&label.text());
        }
    });
    let open_directory = gtk::Button::with_label("Browse selected directory");
    let next_directory = directory.clone();
    let current_selection = selected_path.clone();
    let browse_selected = browse.clone();
    open_directory.connect_clicked(move |_| {
        next_directory.set_text(&current_selection.text());
        browse_selected.emit_clicked();
    });
    restore_page.append(&open_directory);
    let destination = field(
        &restore_page,
        "New restore folder",
        "/existing/parent/new-folder",
    );
    let restore = gtk::Button::with_label("Restore selected path to new folder");
    restore_page.append(&restore);
    scroll.set_child(Some(&content));
    outer.append(&scroll);
    window.set_content(Some(&outer));
    let busy = Rc::new(RefCell::new(false));
    let closing_busy = busy.clone();
    window.connect_close_request(move |_| {
        if *closing_busy.borrow() {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    {
        let (repo, password, snapshot, directory, selected_path, status, files, busy) = (
            repo.clone(),
            password.clone(),
            snapshot.clone(),
            directory.clone(),
            selected_path.clone(),
            status.clone(),
            files.clone(),
            busy.clone(),
        );
        let stack = stack.clone();
        let selected_destination = selected_destination.clone();
        browse.connect_clicked(move |_| {
            if *busy.borrow() {
                return;
            }
            if !destination_available(&selected_destination) {
                status.set_text("The selected drive is unavailable or has changed. Reconnect and select it again.");
                return;
            }
            *busy.borrow_mut() = true;
            selected_path.set_text("");
            while let Some(child) = files.first_child() {
                files.remove(&child);
            }
            let repository = Repository {
                location: repo.text().to_string(),
            };
            let secret = password.text().to_string();
            password.set_text("");
            let id = snapshot.text().to_string();
            let path = directory.text().to_string();
            let (tx, rx) = mpsc::sync_channel(1);
            repo.set_editable(false);
            snapshot.set_editable(false);
            stack.set_sensitive(false);
            status.set_text("Loading directory…");
            std::thread::spawn(move || {
                let mut paths = Vec::new();
                let mut total = 0usize;
                let result =
                    Engine::default().list_directory(&repository, &secret, &id, &path, |entry| {
                        total += 1;
                        if paths.len() < 500 {
                            paths.push(entry.path);
                        }
                    });
                let _ = tx.send((result.map_err(|error| error.to_string()), paths, total));
            });
            let (status, files, busy, repo, snapshot) = (
                status.clone(),
                files.clone(),
                busy.clone(),
                repo.clone(),
                snapshot.clone(),
            );
            let stack = stack.clone();
            glib::timeout_add_local(Duration::from_millis(100), move || match rx.try_recv() {
                Ok((result, paths, total)) => {
                    *busy.borrow_mut() = false;
                    repo.set_editable(true);
                    snapshot.set_editable(true);
                    stack.set_sensitive(true);
                    match result {
                        Ok(()) => {
                            while let Some(child) = files.first_child() {
                                files.remove(&child);
                            }
                            for path in paths {
                                let label = gtk::Label::new(Some(&path));
                                label.set_xalign(0.0);
                                files.append(&label);
                            }
                            status.set_text(&format!(
                                "Showing {} of {total} entries",
                                total.min(500)
                            ));
                        }
                        Err(error) => {
                            status.set_text(&format!("Directory listing failed: {error}"))
                        }
                    }
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    *busy.borrow_mut() = false;
                    repo.set_editable(true);
                    snapshot.set_editable(true);
                    stack.set_sensitive(true);
                    status.set_text("Directory listing failed unexpectedly");
                    glib::ControlFlow::Break
                }
            });
        });
    }
    {
        let (snapshot, selected_path, list, files) = (
            snapshot.clone(),
            selected_path.clone(),
            list.clone(),
            files.clone(),
        );
        let selected_destination = selected_destination.clone();
        repo.connect_changed(move |_| {
            *selected_destination.borrow_mut() = None;
            snapshot.set_text("");
            selected_path.set_text("");
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            while let Some(child) = files.first_child() {
                files.remove(&child);
            }
        });
    }
    {
        let (selected_path, files) = (selected_path.clone(), files.clone());
        snapshot.connect_changed(move |_| {
            selected_path.set_text("");
            while let Some(child) = files.first_child() {
                files.remove(&child);
            }
        });
    }
    let snap = snapshot.clone();
    list.connect_row_selected(move |_, row| {
        if let Some(row) = row
            && let Some(label) = row.child().and_downcast::<gtk::Label>()
            && let Some(id) = label.text().split_whitespace().next()
        {
            snap.set_text(id);
        }
    });
    for (operation, button) in [
        (Operation::Init, init_button),
        (Operation::Snapshots, snapshots_button),
        (Operation::Backup, backup_button),
        (Operation::Check, check_button),
        (Operation::Restore, restore),
        (Operation::Preview, preview),
    ] {
        let loaded_profile = loaded_profile.clone();
        let (repo, source, password, snapshot, selected_path, destination, status, list) = (
            repo.clone(),
            source.clone(),
            password.clone(),
            snapshot.clone(),
            selected_path.clone(),
            destination.clone(),
            status.clone(),
            list.clone(),
        );
        let busy = busy.clone();
        let stack = stack.clone();
        let selected_destination = selected_destination.clone();
        let parent = window.clone();
        button.connect_clicked(move |_| {
            if *busy.borrow() {
                return;
            }
            if !destination_available(&selected_destination) {
                status.set_text("The selected drive is unavailable or has changed. Reconnect and select it again.");
                return;
            }
            if matches!(operation, Operation::Backup | Operation::Preview)
                && source_repository_overlap(&source.text(), &repo.text())
            {
                let warning = adw::AlertDialog::new(
                    Some("The source and repository folders overlap"),
                    Some("The selected source appears to be the external drive or a folder inside the repository. Select the folder you want to back up as the source, and set the external drive's Restic repository in the repository field. Swap them if you selected the drive as the source."),
                );
                warning.add_response("ok", "OK");
                warning.set_close_response("ok");
                warning.present(Some(&parent));
                return;
            }
            if operation == Operation::Snapshots {
                while let Some(child) = list.first_child() {
                    list.remove(&child);
                }
                snapshot.set_text("");
                selected_path.set_text("");
            }
            *busy.borrow_mut() = true;
            let token = Arc::new(AtomicBool::new(false));
            let progress_ui = operation
                .progress_title()
                .map(|title| progress_window(&parent, title, token.clone()));
            let repository = Repository {
                location: repo.text().to_string(),
            };
            let profile = backup_profile(
                loaded_profile.borrow().as_ref(),
                &repository.location,
                &source.text(),
            );
            let secret = password.text().to_string();
            password.set_text("");
            let id = snapshot.text().to_string();
            let selected = selected_path.text().to_string();
            let target = destination.text().to_string();
            let (tx, rx) = mpsc::sync_channel::<OperationUpdate>(16);
            repo.set_editable(false);
            snapshot.set_editable(false);
            stack.set_sensitive(false);
            status.set_text(if progress_ui.is_some() { "" } else { "Working…" });
            std::thread::spawn(move || {
                let engine = Engine::default();
                let sender = &tx;
                let progress = |restoring: bool| {
                    move |event: Event| {
                        if let Event::Json(value) = event
                            && let Some((message, fraction)) = progress_status(&value, restoring)
                        {
                            let _ = sender.try_send(OperationUpdate::Progress(message, fraction));
                        }
                    }
                };
                let result = (|| -> anyhow::Result<(String, Vec<Snapshot>, Option<Outcome>)> {
                    let mut activity_problem = None;
                    let result = match operation {
                        Operation::Snapshots => {
                            return Ok((
                                "Snapshots loaded".into(),
                                engine.snapshots(&repository, &secret)?,
                                None,
                            ));
                        }
                        Operation::Init => engine.init(&repository, &secret)?,
                        Operation::Check => engine.check(&repository, &secret, token, |_| {})?,
                        Operation::Backup => {
                            let result = engine.backup(&profile, &secret, token, progress(false))?;
                            if let Err(error) = restic_core::state_dir()
                                .and_then(|state| restic_core::record(&state, &profile.id, &result))
                            {
                                activity_problem = Some(format!("Activity history was not updated: {error:#}"));
                            }
                            result
                        }
                        Operation::Preview => {
                            engine.preview_backup(&profile, &secret, token, progress(false))?
                        }
                        Operation::Restore => engine.restore_new_with_progress(
                            &repository,
                            &secret,
                            &id,
                            &selected,
                            std::path::Path::new(&target),
                            token,
                            progress(true),
                        )?,
                    };
                    let outcome = result.outcome.clone();
                    let (seconds, name) = (result.elapsed_seconds, &profile.name);
                    let mut message = match operation {
                        Operation::Preview => format!("{name} preview: {outcome:?} • {seconds:.1} seconds • no snapshot created"),
                        Operation::Backup => format!("{name}: {outcome:?} • {seconds:.1} seconds"),
                        _ => format!("Outcome: {outcome:?} • {seconds:.1} seconds"),
                    };
                    for line in result.errors.iter().chain(&activity_problem) {
                        message.push('\n');
                        message.push_str(line);
                    }
                    if outcome == Outcome::Cancelled {
                        message.push_str(if operation == Operation::Restore {
                            "\nRestore stopped; no destination was published."
                        } else {
                            "\nBackup stopped; refresh snapshots to check whether Restic created one before stopping."
                        });
                    }
                    Ok((message, vec![], Some(outcome)))
                })();
                let (message, snapshots, outcome) = result
                    .unwrap_or_else(|e| (format!("Operation failed: {e}"), vec![], None));
                let _ = tx.send(OperationUpdate::Done(message, snapshots, outcome));
            });
            let status = status.clone();
            let list = list.clone();
            let busy = busy.clone();
            let repo = repo.clone();
            let snapshot = snapshot.clone();
            let stack = stack.clone();
            let mut determinate = false;
            glib::timeout_add_local(Duration::from_millis(100), move || {
                loop {
                    let update = match rx.try_recv() {
                        Ok(value) => value,
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            *busy.borrow_mut() = false;
                            repo.set_editable(true);
                            snapshot.set_editable(true);
                            stack.set_sensitive(true);
                            if let Some(ui) = &progress_ui {
                                ui.window.set_title(Some("Operation failed"));
                                ui.status.set_text("Operation ended unexpectedly");
                                ui.bar.set_text(Some("Failed"));
                                ui.stop.set_sensitive(false);
                                ui.close.set_sensitive(true);
                                ui.finished.set(true);
                            } else {
                                status.set_text("Operation ended unexpectedly");
                            }
                            return glib::ControlFlow::Break;
                        }
                    };
                    match update {
                        OperationUpdate::Progress(message, fraction) => {
                            if let Some(ui) = &progress_ui {
                                ui.status.set_text(&message);
                                if let Some(fraction) = fraction {
                                    determinate = true;
                                    ui.bar.set_fraction(fraction);
                                    ui.bar.set_text(Some(&format!("{:.0}%", fraction * 100.0)));
                                }
                            }
                        }
                        OperationUpdate::Done(message, snapshots, outcome) => {
                            if let Some(ui) = &progress_ui {
                                ui.window.set_title(Some("Operation finished"));
                                ui.status.set_text(&message);
                                ui.bar.set_text(Some(match outcome {
                                    Some(Outcome::Success) => "Complete",
                                    Some(Outcome::Cancelled) => "Stopped",
                                    _ => "Finished with errors",
                                }));
                                if outcome == Some(Outcome::Success) {
                                    ui.bar.set_fraction(1.0);
                                }
                                ui.stop.set_sensitive(false);
                                ui.close.set_sensitive(true);
                                ui.finished.set(true);
                            } else {
                                status.set_text(&message);
                            }
                            if !snapshots.is_empty() {
                                while let Some(child) = list.first_child() {
                                    list.remove(&child);
                                }
                                for snapshot in snapshots {
                                    let label = gtk::Label::new(Some(&format!(
                                        "{}  {}",
                                        snapshot.id, snapshot.time
                                    )));
                                    label.set_xalign(0.0);
                                    list.append(&label);
                                }
                            }
                            *busy.borrow_mut() = false;
                            repo.set_editable(true);
                            snapshot.set_editable(true);
                            stack.set_sensitive(true);
                            return glib::ControlFlow::Break;
                        }
                    }
                }
                if !determinate && let Some(ui) = &progress_ui {
                    ui.bar.pulse();
                }
                glib::ControlFlow::Continue
            });
        });
    }
    window.present();
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, os::unix::fs::MetadataExt, rc::Rc};
    #[test]
    fn root_source_allows_repository_but_other_overlaps_warn() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let repository = temp.path().join("repo");
        std::fs::create_dir(&repository)?;
        let repo = repository.to_str().unwrap();
        assert!(!super::source_repository_overlap("/", repo));
        assert!(super::source_repository_overlap(
            temp.path().to_str().unwrap(),
            repo
        ));
        assert!(super::source_repository_overlap(repo, repo));
        assert!(super::source_repository_overlap("/", "/"));
        Ok(())
    }
    #[test]
    fn backup_uses_the_loaded_profile_until_its_fields_are_edited() {
        let saved = restic_core::Profile {
            id: "documents".into(),
            name: "Documents".into(),
            repository: restic_core::Repository {
                location: "/media/drive/repo".into(),
            },
            sources: vec!["/home/me/docs".into(), "/home/me/photos".into()],
            excludes: vec!["*.tmp".into()],
            one_file_system: true,
        };
        let used = super::backup_profile(Some(&saved), "/media/drive/repo", "/home/me/docs");
        assert_eq!(used.id, "documents");
        assert_eq!(used.sources.len(), 2);
        assert_eq!(used.excludes, ["*.tmp"]);
        assert!(used.one_file_system);
        for (repository, source) in [
            ("/media/drive/repo", "/home/me/music"),
            ("/media/other/repo", "/home/me/docs"),
        ] {
            let manual = super::backup_profile(Some(&saved), repository, source);
            assert_eq!(manual.id, "manual");
            assert!(manual.excludes.is_empty() && !manual.one_file_system);
            assert_eq!(manual.sources, [std::path::PathBuf::from(source)]);
        }
        assert_eq!(super::backup_profile(None, "/r", "/s").id, "manual");
    }
    #[test]
    fn profile_ids_are_file_safe() {
        assert_eq!(
            super::profile_id("  My Documents! ").as_deref(),
            Some("my-documents")
        );
        assert_eq!(super::profile_id("a/../b").as_deref(), Some("a-b"));
        assert_eq!(super::profile_id("日本語"), None);
        assert_eq!(super::profile_id("---"), None);
        let long = super::profile_id(&"x".repeat(100)).unwrap();
        assert_eq!(long.len(), 64);
        assert!(restic_core::valid_id(&long).is_ok());
    }
    #[test]
    fn backup_and_restore_progress_use_reported_fields() {
        let backup = serde_json::json!({
            "message_type": "status", "files_done": 2, "total_files": 4,
            "bytes_done": 50, "total_bytes": 100, "percent_done": 0.5
        });
        let restore = serde_json::json!({
            "message_type": "status", "files_restored": 1, "total_files": 2,
            "bytes_restored": 25, "total_bytes": 100, "percent_done": 0.25
        });
        let (backup_text, backup_fraction) = super::progress_status(&backup, false).unwrap();
        let (restore_text, restore_fraction) = super::progress_status(&restore, true).unwrap();
        assert!(backup_text.contains("Processed 2 of 4 files"));
        assert!(restore_text.contains("Restored 1 of 2 files"));
        assert_eq!(backup_fraction, Some(0.5));
        assert_eq!(restore_fraction, Some(0.25));
        assert_eq!(
            super::progress_status(&serde_json::json!({"message_type":"summary"}), false),
            None
        );
        assert_eq!(
            super::progress_status(&serde_json::json!({"message_type":"status"}), true)
                .unwrap()
                .1,
            None
        );
    }
    #[test]
    fn destination_selection_preserves_existing_repository() -> std::io::Result<()> {
        let parent = tempfile::tempdir()?;
        assert_eq!(
            super::repository_in(parent.path()).unwrap(),
            parent.path().join("restic-studio")
        );
        std::fs::write(parent.path().join("config"), "existing repository")?;
        assert_eq!(super::repository_in(parent.path()).unwrap(), parent.path());
        std::fs::remove_file(parent.path().join("config"))?;
        let existing = parent.path().join("DebianBackup");
        std::fs::create_dir(&existing)?;
        std::fs::write(existing.join("config"), "existing repository")?;
        assert_eq!(super::repository_in(parent.path()).unwrap(), existing);
        Ok(())
    }
    #[test]
    fn selected_drive_disappearing_blocks_operations() -> std::io::Result<()> {
        let parent = tempfile::tempdir()?;
        let mount = parent.path().join("mounted");
        std::fs::create_dir(&mount)?;
        let device = std::fs::metadata(&mount)?.dev();
        let selected = Rc::new(RefCell::new(Some((mount.clone(), device))));
        assert!(super::destination_available(&selected));
        std::fs::remove_dir(&mount)?;
        assert!(!super::destination_available(&selected));
        Ok(())
    }
}
