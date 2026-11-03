mod scanner;
mod settings;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use scanner::{ScanEntry, ScanMessage, ScanStats};
use settings::Settings;
use std::cell::RefCell;
use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

const APP_ID: &str = "dev.folderlens.FolderLens";

thread_local! {
    static APP_UI: RefCell<Option<Rc<AppUi>>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, PartialEq)]
enum SortOrder {
    Largest,
    Smallest,
    Name,
}

#[derive(Clone)]
struct ChartSlice {
    name: String,
    size: u64,
    color: (f64, f64, f64),
}

struct AppState {
    entries: Vec<ScanEntry>,
    receiver: Option<mpsc::Receiver<ScanMessage>>,
    cancellation: Option<Arc<AtomicBool>>,
    current_directory: Option<PathBuf>,
    history: Vec<PathBuf>,
    sort: SortOrder,
}

struct AppUi {
    window: gtk::ApplicationWindow,
    back_button: gtk::Button,
    choose_button: gtk::Button,
    cancel_button: gtk::Button,
    about_button: gtk::Button,
    about_dialog: gtk::AboutDialog,
    search_entry: gtk::SearchEntry,
    sort_combo: gtk::DropDown,
    current_path: gtk::Label,
    status: gtk::Label,
    progress: gtk::ProgressBar,
    results: gtk::ListBox,
    empty_state: gtk::Box,
    distribution_panel: gtk::Box,
    distribution_chart: gtk::DrawingArea,
    chart_data: Rc<RefCell<Vec<ChartSlice>>>,
    state: RefCell<AppState>,
}

impl AppUi {
    fn new(app: &gtk::Application) -> Rc<Self> {
        let settings = Settings::load();
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Folder Lens")
            .default_width(settings.window_width)
            .default_height(settings.window_height)
            .build();

        let header = gtk::HeaderBar::new();
        let back_button = gtk::Button::from_icon_name("go-previous-symbolic");
        back_button.set_tooltip_text(Some("Back to the previous folder"));
        back_button.set_sensitive(false);
        let choose_button = gtk::Button::with_label("Choose Folder");
        choose_button.add_css_class("suggested-action");
        let cancel_button = gtk::Button::with_label("Cancel");
        cancel_button.set_sensitive(false);
        let about_button = gtk::Button::from_icon_name("help-about-symbolic");
        about_button.set_tooltip_text(Some("About Folder Lens"));
        header.pack_start(&back_button);
        header.pack_start(&choose_button);
        header.pack_start(&cancel_button);
        header.pack_end(&about_button);
        window.set_titlebar(Some(&header));

        let about_dialog = gtk::AboutDialog::builder()
            .transient_for(&window)
            .modal(true)
            .hide_on_close(true)
            .program_name("Folder Lens")
            .version(env!("CARGO_PKG_VERSION"))
            .comments("Find what uses disk space with a fast, visual folder scanner.")
            .logo_icon_name("folder-symbolic")
            .license_type(gtk::License::MitX11)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.set_margin_top(16);
        root.set_margin_bottom(16);
        root.set_margin_start(16);
        root.set_margin_end(16);

        let current_path = gtk::Label::new(Some("Choose a folder to see what is using its space"));
        current_path.set_xalign(0.0);
        current_path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        current_path.add_css_class("title-3");
        root.append(&current_path);

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Filter by name or path")
            .hexpand(true)
            .build();
        let sort_combo = gtk::DropDown::from_strings(&["Largest first", "Smallest first", "Name"]);
        controls.append(&search_entry);
        controls.append(&sort_combo);
        root.append(&controls);

        let progress = gtk::ProgressBar::new();
        progress.set_visible(false);
        progress.set_show_text(false);
        root.append(&progress);

        let status = gtk::Label::new(Some("No scan yet"));
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        root.append(&status);

        let results = gtk::ListBox::new();
        results.set_selection_mode(gtk::SelectionMode::None);
        results.add_css_class("boxed-list");

        let empty_state = gtk::Box::new(gtk::Orientation::Vertical, 8);
        empty_state.set_valign(gtk::Align::Center);
        empty_state.set_vexpand(true);
        let empty_icon = gtk::Image::from_icon_name("folder-symbolic");
        empty_icon.set_pixel_size(64);
        empty_icon.add_css_class("dim-label");
        let empty_title = gtk::Label::new(Some("Your largest items will appear here"));
        empty_title.add_css_class("title-2");
        let empty_help = gtk::Label::new(Some(
            "Folder Lens only reads file sizes. It never changes your files.",
        ));
        empty_help.add_css_class("dim-label");
        empty_state.append(&empty_icon);
        empty_state.append(&empty_title);
        empty_state.append(&empty_help);

        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&results)
            .build();
        stack.add_named(&empty_state, Some("empty"));
        stack.add_named(&scroll, Some("results"));
        stack.set_visible_child_name("empty");
        root.append(&stack);

        let distribution_panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
        distribution_panel.set_visible(false);
        distribution_panel.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let distribution_title = gtk::Label::new(Some("Space distribution"));
        distribution_title.set_xalign(0.0);
        distribution_title.add_css_class("title-4");
        distribution_panel.append(&distribution_title);

        let chart_data = Rc::new(RefCell::new(Vec::<ChartSlice>::new()));
        let distribution_chart = gtk::DrawingArea::builder()
            .content_height(158)
            .hexpand(true)
            .build();
        distribution_chart.set_tooltip_text(Some(
            "The largest seven items are shown separately; smaller items are grouped as Other.",
        ));
        let draw_data = Rc::clone(&chart_data);
        distribution_chart.set_draw_func(move |area, context, width, height| {
            draw_distribution(area, context, width, height, &draw_data.borrow());
        });
        distribution_panel.append(&distribution_chart);
        root.append(&distribution_panel);
        window.set_child(Some(&root));

        let ui = Rc::new(Self {
            window,
            back_button,
            choose_button,
            cancel_button,
            about_button,
            about_dialog,
            search_entry,
            sort_combo,
            current_path,
            status,
            progress,
            results,
            empty_state,
            distribution_panel,
            distribution_chart,
            chart_data,
            state: RefCell::new(AppState {
                entries: Vec::new(),
                receiver: None,
                cancellation: None,
                current_directory: settings.last_directory,
                history: Vec::new(),
                sort: SortOrder::Largest,
            }),
        });

        // The stack is found through the empty-state parent when results are refreshed.
        ui.connect_signals();
        ui.start_message_pump();
        ui.window.present();
        ui
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.back_button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.navigate_back();
            }
        });

        let weak = Rc::downgrade(self);
        self.choose_button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.show_folder_chooser();
            }
        });

        let weak = Rc::downgrade(self);
        self.cancel_button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.cancel_scan();
            }
        });

        let weak = Rc::downgrade(self);
        self.about_button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.about_dialog.present();
            }
        });

        let weak = Rc::downgrade(self);
        self.search_entry.connect_search_changed(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.refresh_results();
            }
        });

        let weak = Rc::downgrade(self);
        self.sort_combo.connect_selected_notify(move |combo| {
            if let Some(ui) = weak.upgrade() {
                ui.state.borrow_mut().sort = match combo.selected() {
                    1 => SortOrder::Smallest,
                    2 => SortOrder::Name,
                    _ => SortOrder::Largest,
                };
                ui.refresh_results();
            }
        });

        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |window| {
            if let Some(ui) = weak.upgrade() {
                ui.cancel_scan();
                Settings {
                    window_width: window.width(),
                    window_height: window.height(),
                    last_directory: ui.state.borrow().current_directory.clone(),
                }
                .save();
            }
            APP_UI.with(|slot| {
                slot.borrow_mut().take();
            });
            glib::Propagation::Proceed
        });
    }

    fn show_folder_chooser(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose a folder to scan")
            .accept_label("Scan")
            .modal(true)
            .build();

        if let Some(path) = self.state.borrow().current_directory.as_ref() {
            dialog.set_initial_folder(Some(&gio::File::for_path(path)));
        }

        let weak = Rc::downgrade(self);
        dialog.select_folder(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |result| {
                let Ok(folder) = result else {
                    return;
                };
                if let (Some(ui), Some(path)) = (weak.upgrade(), folder.path()) {
                    ui.start_fresh_scan(path);
                }
            },
        );
    }

    fn start_fresh_scan(self: &Rc<Self>, path: PathBuf) {
        self.state.borrow_mut().history.clear();
        self.back_button.set_sensitive(false);
        self.start_scan(path);
    }

    fn navigate_to(self: &Rc<Self>, path: PathBuf) {
        let current = self.state.borrow().current_directory.clone();
        if current.as_ref() == Some(&path) {
            return;
        }
        if let Some(current) = current {
            self.state.borrow_mut().history.push(current);
        }
        self.back_button.set_sensitive(true);
        self.start_scan(path);
    }

    fn navigate_back(self: &Rc<Self>) {
        let (previous, has_more_history) = {
            let mut state = self.state.borrow_mut();
            let previous = state.history.pop();
            (previous, !state.history.is_empty())
        };
        if let Some(previous) = previous {
            self.back_button.set_sensitive(has_more_history);
            self.start_scan(previous);
        }
    }

    fn start_scan(self: &Rc<Self>, path: PathBuf) {
        self.cancel_scan();
        let (sender, receiver) = mpsc::channel();
        let cancellation = Arc::new(AtomicBool::new(false));
        let thread_cancellation = Arc::clone(&cancellation);
        let thread_path = path.clone();

        {
            let mut state = self.state.borrow_mut();
            state.entries.clear();
            state.receiver = Some(receiver);
            state.cancellation = Some(cancellation);
            state.current_directory = Some(path.clone());
        }

        self.current_path.set_text(&path.to_string_lossy());
        self.status.set_text("Starting scan…");
        self.progress.set_visible(true);
        self.progress.pulse();
        self.cancel_button.set_sensitive(true);
        self.refresh_results();

        thread::spawn(move || scanner::scan_root(thread_path, sender, thread_cancellation));
    }

    fn cancel_scan(&self) {
        if let Some(cancellation) = self.state.borrow().cancellation.as_ref() {
            cancellation.store(true, Ordering::Relaxed);
        }
    }

    fn start_message_pump(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(80), move || {
            if let Some(ui) = weak.upgrade() {
                ui.poll_messages();
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }

    fn poll_messages(self: &Rc<Self>) {
        let messages: Vec<_> = {
            let state = self.state.borrow();
            state
                .receiver
                .as_ref()
                .map(|receiver| receiver.try_iter().collect())
                .unwrap_or_default()
        };
        if messages.is_empty() {
            if self.progress.is_visible() {
                self.progress.pulse();
            }
            return;
        }

        let mut refresh = false;
        for message in messages {
            match message {
                ScanMessage::Progress(stats) => self.show_progress(stats),
                ScanMessage::Entry(entry) => {
                    self.state.borrow_mut().entries.push(entry);
                    refresh = true;
                }
                ScanMessage::Finished(stats) => {
                    self.finish_scan(stats, false);
                    refresh = true;
                }
                ScanMessage::Cancelled(stats) => {
                    self.finish_scan(stats, true);
                    refresh = true;
                }
                ScanMessage::RootError(error) => {
                    self.status.set_text(&error);
                    self.progress.set_visible(false);
                    self.cancel_button.set_sensitive(false);
                    self.state.borrow_mut().receiver = None;
                }
            }
        }
        if refresh {
            self.refresh_results();
        }
    }

    fn show_progress(&self, stats: ScanStats) {
        self.status.set_text(&format!(
            "Scanning… {} files, {} folders, {} found{}",
            stats.files,
            stats.directories,
            format_size(stats.bytes),
            error_suffix(stats.errors)
        ));
    }

    fn finish_scan(&self, stats: ScanStats, cancelled: bool) {
        let prefix = if cancelled {
            "Scan cancelled"
        } else {
            "Scan complete"
        };
        self.status.set_text(&format!(
            "{prefix} — {} files, {} folders, {}{}",
            stats.files,
            stats.directories,
            format_size(stats.bytes),
            error_suffix(stats.errors)
        ));
        self.progress.set_visible(false);
        self.cancel_button.set_sensitive(false);
        let mut state = self.state.borrow_mut();
        state.receiver = None;
        state.cancellation = None;
    }

    fn refresh_results(self: &Rc<Self>) {
        while let Some(child) = self.results.first_child() {
            self.results.remove(&child);
        }

        let query = self.search_entry.text().to_lowercase();
        let state = self.state.borrow();
        let mut entries: Vec<&ScanEntry> = state
            .entries
            .iter()
            .filter(|entry| {
                query.is_empty()
                    || entry.name.to_lowercase().contains(&query)
                    || entry.path.to_string_lossy().to_lowercase().contains(&query)
            })
            .collect();

        match state.sort {
            SortOrder::Largest => entries.sort_by_key(|entry| Reverse(entry.size)),
            SortOrder::Smallest => entries.sort_by_key(|entry| entry.size),
            SortOrder::Name => entries.sort_by_key(|entry| entry.name.to_lowercase()),
        }

        for entry in entries {
            self.results.append(&result_row(entry, Rc::downgrade(self)));
        }
        let has_results = self.results.first_child().is_some();
        let slices = chart_slices(&state.entries);
        drop(state);

        let has_distribution = !slices.is_empty();
        *self.chart_data.borrow_mut() = slices;
        self.distribution_panel.set_visible(has_distribution);
        self.distribution_chart.queue_draw();

        if let Some(parent) = self.empty_state.parent() {
            if let Ok(stack) = parent.downcast::<gtk::Stack>() {
                stack.set_visible_child_name(if has_results { "results" } else { "empty" });
            }
        }
    }
}

fn chart_slices(entries: &[ScanEntry]) -> Vec<ChartSlice> {
    const COLORS: [(f64, f64, f64); 7] = [
        (0.20, 0.52, 0.89),
        (0.93, 0.39, 0.29),
        (0.30, 0.69, 0.40),
        (0.62, 0.42, 0.82),
        (0.95, 0.64, 0.20),
        (0.16, 0.68, 0.69),
        (0.87, 0.37, 0.64),
    ];

    let mut sorted: Vec<&ScanEntry> = entries.iter().filter(|entry| entry.size > 0).collect();
    sorted.sort_by_key(|entry| Reverse(entry.size));

    let mut slices: Vec<ChartSlice> = sorted
        .iter()
        .take(COLORS.len())
        .enumerate()
        .map(|(index, entry)| ChartSlice {
            name: entry.name.clone(),
            size: entry.size,
            color: COLORS[index],
        })
        .collect();

    let other_size = sorted
        .iter()
        .skip(COLORS.len())
        .fold(0_u64, |total, entry| total.saturating_add(entry.size));
    if other_size > 0 {
        slices.push(ChartSlice {
            name: "Other".into(),
            size: other_size,
            color: (0.48, 0.50, 0.54),
        });
    }
    slices
}

fn draw_distribution(
    area: &gtk::DrawingArea,
    context: &gtk::cairo::Context,
    width: i32,
    _height: i32,
    slices: &[ChartSlice],
) {
    let total = slices
        .iter()
        .fold(0_u64, |sum, slice| sum.saturating_add(slice.size));
    if total == 0 || width <= 32 {
        return;
    }

    let padding = 12.0;
    let available_width = f64::from(width) - padding * 2.0;
    let bar_y = 8.0;
    let bar_height = 30.0;
    let mut x = padding;

    for (index, slice) in slices.iter().enumerate() {
        let segment_width = if index == slices.len() - 1 {
            padding + available_width - x
        } else {
            available_width * slice.size as f64 / total as f64
        };
        context.set_source_rgb(slice.color.0, slice.color.1, slice.color.2);
        context.rectangle(x, bar_y, segment_width.max(0.5), bar_height);
        let _ = context.fill();
        x += segment_width;
    }

    let foreground = area.color();
    context.select_font_face(
        "Sans",
        gtk::cairo::FontSlant::Normal,
        gtk::cairo::FontWeight::Normal,
    );
    context.set_font_size(12.5);

    let columns = if slices.len() > 4 { 2 } else { 1 };
    let rows = slices.len().div_ceil(columns);
    let column_width = available_width / columns as f64;
    for (index, slice) in slices.iter().enumerate() {
        let column = index / rows;
        let row = index % rows;
        let item_x = padding + column as f64 * column_width;
        let item_y = 58.0 + row as f64 * 23.0;

        context.set_source_rgb(slice.color.0, slice.color.1, slice.color.2);
        context.rectangle(item_x, item_y - 10.0, 12.0, 12.0);
        let _ = context.fill();

        let percentage = slice.size as f64 * 100.0 / total as f64;
        let max_name_chars = if columns == 2 {
            if width >= 720 {
                24
            } else {
                12
            }
        } else {
            38
        };
        let label = format!(
            "{} — {} ({percentage:.1}%)",
            elide(&slice.name, max_name_chars),
            format_size(slice.size)
        );
        context.set_source_rgba(
            f64::from(foreground.red()),
            f64::from(foreground.green()),
            f64::from(foreground.blue()),
            f64::from(foreground.alpha()),
        );
        context.move_to(item_x + 20.0, item_y);
        let _ = context.show_text(&label);
    }
}

fn elide(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut shortened: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    shortened.push('…');
    shortened
}

fn result_row(entry: &ScanEntry, ui: Weak<AppUi>) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let icon_name = if entry.is_dir {
        "folder-symbolic"
    } else {
        "text-x-generic-symbolic"
    };
    content.append(&gtk::Image::from_icon_name(icon_name));

    let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
    labels.set_hexpand(true);
    let name = gtk::Label::new(Some(&entry.name));
    name.set_xalign(0.0);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let path_label = gtk::Label::new(Some(&entry.path.to_string_lossy()));
    path_label.set_xalign(0.0);
    path_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    path_label.add_css_class("dim-label");
    path_label.add_css_class("caption");
    labels.append(&name);
    labels.append(&path_label);
    content.append(&labels);

    let size = gtk::Label::new(Some(&format_size(entry.size)));
    size.set_width_chars(10);
    size.set_xalign(1.0);
    content.append(&size);

    let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy.set_tooltip_text(Some("Copy path"));
    let path = entry.path.clone();
    copy.connect_clicked(move |button| {
        button
            .display()
            .clipboard()
            .set_text(&path.to_string_lossy());
    });
    content.append(&copy);

    let open = gtk::Button::from_icon_name("folder-open-symbolic");
    open.set_tooltip_text(Some("Open with the default application"));
    let path = entry.path.clone();
    open.connect_clicked(move |_| open_path(&path));
    content.append(&open);

    row.set_child(Some(&content));
    attach_context_menu(&row, entry, ui);
    row
}

fn attach_context_menu(row: &gtk::ListBoxRow, entry: &ScanEntry, ui: Weak<AppUi>) {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(true);
    popover.set_parent(row);

    let actions = gtk::Box::new(gtk::Orientation::Vertical, 2);
    actions.set_margin_top(6);
    actions.set_margin_bottom(6);
    actions.set_margin_start(6);
    actions.set_margin_end(6);

    if entry.is_dir {
        let scan = gtk::Button::with_label("Scan this folder");
        scan.set_halign(gtk::Align::Fill);
        scan.add_css_class("flat");
        let path = entry.path.clone();
        let menu = popover.clone();
        let weak = ui.clone();
        scan.connect_clicked(move |_| {
            menu.popdown();
            if let Some(ui) = weak.upgrade() {
                ui.navigate_to(path.clone());
            }
        });
        actions.append(&scan);
        actions.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    }

    let open = gtk::Button::with_label("Open");
    open.set_halign(gtk::Align::Fill);
    open.add_css_class("flat");
    let path = entry.path.clone();
    let menu = popover.clone();
    open.connect_clicked(move |_| {
        menu.popdown();
        open_path(&path);
    });
    actions.append(&open);

    let copy = gtk::Button::with_label("Copy path");
    copy.set_halign(gtk::Align::Fill);
    copy.add_css_class("flat");
    let path = entry.path.clone();
    let menu = popover.clone();
    copy.connect_clicked(move |button| {
        menu.popdown();
        button
            .display()
            .clipboard()
            .set_text(&path.to_string_lossy());
    });
    actions.append(&copy);
    popover.set_child(Some(&actions));

    let gesture = gtk::GestureClick::new();
    gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
    gesture.set_exclusive(true);
    gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
    let menu = popover.clone();
    gesture.connect_pressed(move |gesture, _, x, y| {
        menu.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        menu.popup();
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    row.add_controller(gesture);
}

fn open_path(path: &Path) {
    let uri = gio::File::for_path(path).uri();
    if let Err(error) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        eprintln!("Could not open {}: {error}", path.display());
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn error_suffix(errors: u64) -> String {
    match errors {
        0 => String::new(),
        1 => " (1 unreadable item)".into(),
        count => format!(" ({count} unreadable items)"),
    }
}

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        APP_UI.with(|slot| {
            let existing = slot.borrow().as_ref().cloned();
            if let Some(ui) = existing {
                ui.window.present();
            } else {
                *slot.borrow_mut() = Some(AppUi::new(app));
            }
        });
    });
    app.run()
}

#[cfg(test)]
mod tests {
    use super::{chart_slices, elide, format_size};
    use crate::scanner::ScanEntry;
    use std::path::PathBuf;

    #[test]
    fn formats_sizes_for_people() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1_572_864), "1.5 MiB");
    }

    #[test]
    fn chart_groups_items_after_the_largest_seven() {
        let entries: Vec<_> = (1..=9)
            .map(|index| ScanEntry {
                path: PathBuf::from(format!("item-{index}")),
                name: format!("item-{index}"),
                size: index,
                is_dir: false,
            })
            .collect();
        let slices = chart_slices(&entries);

        assert_eq!(slices.len(), 8);
        assert_eq!(slices[0].name, "item-9");
        assert_eq!(slices.last().unwrap().name, "Other");
        assert_eq!(slices.last().unwrap().size, 3);
    }

    #[test]
    fn long_chart_labels_are_shortened() {
        assert_eq!(elide("documents", 20), "documents");
        assert_eq!(elide("very-long-directory", 10), "very-long…");
    }
}
