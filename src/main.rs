//! Folder Lens GTK application entry point and presentation layer.
//!
//! This module builds the desktop interface, coordinates background scans,
//! renders result and filesystem views, persists preferences, and provides
//! navigation and file actions. Filesystem traversal itself lives in
//! [`scanner`], while JSON preference storage lives in [`settings`].

/// Background filesystem scanner and its worker messages.
mod scanner;
/// Persistent user and window preferences.
mod settings;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use scanner::{ScanEntry, ScanMessage, ScanStats};
use settings::Settings;
use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

/// Reverse-DNS identifier used for GTK application uniqueness and activation.
const APP_ID: &str = "dev.folderlens.FolderLens";
/// Fixed logical-pixel width shared by Type header and body cells.
const TYPE_COLUMN_WIDTH: i32 = 180;
/// Fixed logical-pixel width shared by Size header and body cells.
const SIZE_COLUMN_WIDTH: i32 = 112;
/// Fixed logical-pixel width shared by Open header and body cells.
const OPEN_COLUMN_WIDTH: i32 = 64;

/// Decodes the application icon embedded in the executable.
///
/// # Returns
///
/// A GTK texture backed by the bundled `assets/folder-lens.png` bytes.
///
/// # Panics
///
/// Panics when the bundled asset is not a valid PNG, which indicates a broken
/// application build rather than a recoverable runtime condition.
fn app_logo() -> gtk::gdk::Texture {
    let bytes = glib::Bytes::from_static(include_bytes!("../assets/folder-lens.png"));
    gtk::gdk::Texture::from_bytes(&bytes).expect("embedded application icon must be a valid PNG")
}

/// Installs the static, theme-aware application stylesheet.
///
/// # Arguments
///
/// * `display` - Display whose GTK style cascade receives the provider.
///
/// # Side effects
///
/// Registers application-priority CSS controlling result tables, filesystem
/// cards, hover states, borders, padding, and light table colors.
fn install_styles(display: &gtk::gdk::Display) {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        r#"
        .results-list,
        .results-list row {
            background: transparent;
        }

        .results-table {
            background-color: #ffffff;
            border: 1px solid #dedede;
            border-radius: 0;
            color: #2e3436;
        }

        .result-header {
            background-color: #f5f5f5;
            border-bottom: 1px solid #dedede;
            border-radius: 0;
            color: #2e3436;
        }

        .result-row {
            background: transparent;
            border: 0;
            border-radius: 0;
            margin: 0;
        }

        .results-list row:hover .result-row {
            background-color: #f4f7fb;
        }

        .result-row .dim-label {
            color: #6b7280;
        }

        .table-cell {
            padding: 10px 12px;
        }

        .result-header .table-cell {
            padding: 7px 12px;
        }

        .column-divider {
            border-left: 1px solid #e4e4e4;
        }

        .filesystem-list {
            border: 1px solid alpha(@theme_fg_color, 0.18);
            border-radius: 4px;
        }
        "#,
    );
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Creates and installs the replaceable column-heading stylesheet.
///
/// # Arguments
///
/// * `display` - Display receiving the style provider.
/// * `font` - Requested heading family; unsupported values fall back to Sans.
/// * `size` - Heading size in CSS pixels.
///
/// # Returns
///
/// The installed provider, retained so settings changes can reload its CSS.
///
/// # Side effects
///
/// Adds a provider just above the base application stylesheet priority.
fn install_column_styles(display: &gtk::gdk::Display, font: &str, size: i32) -> gtk::CssProvider {
    let provider = gtk::CssProvider::new();
    update_column_styles(&provider, font, size);
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    provider
}

/// Replaces the dynamic CSS used by result-table headings.
///
/// # Arguments
///
/// * `provider` - Existing provider installed for the application display.
/// * `font` - One of Sans, Serif, or Monospace; other values become Sans.
/// * `size` - Heading size in CSS pixels.
///
/// # Side effects
///
/// Reloads the provider and immediately restyles matching visible widgets.
fn update_column_styles(provider: &gtk::CssProvider, font: &str, size: i32) {
    let font = match font {
        "Serif" => "Serif",
        "Monospace" => "Monospace",
        _ => "Sans",
    };
    provider.load_from_data(&format!(
        ".column-heading {{ font-family: \"{font}\"; font-size: {size}px; font-weight: 600; }}"
    ));
}

thread_local! {
    /// Strong reference keeping the active interface alive for the GTK process.
    static APP_UI: RefCell<Option<Rc<AppUi>>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, PartialEq)]
/// Supported ordering modes for scan results.
enum SortOrder {
    /// Descending recursive size.
    Largest,
    /// Ascending recursive size.
    Smallest,
    /// Case-insensitive ascending display name.
    Name,
}

/// Converts a persisted drop-down index into a sort mode.
///
/// # Arguments
///
/// * `selected` - Index from the default-sort or active-sort drop-down.
///
/// # Returns
///
/// The matching sort order; unknown indices safely fall back to largest first.
fn sort_order(selected: u32) -> SortOrder {
    match selected {
        1 => SortOrder::Smallest,
        2 => SortOrder::Name,
        _ => SortOrder::Largest,
    }
}

#[derive(Clone)]
/// One colored segment in the space-distribution chart.
struct ChartSlice {
    /// Entry name shown in the chart legend.
    name: String,
    /// Byte total used to calculate segment width and percentage.
    size: u64,
    /// Normalized RGB components used by Cairo.
    color: (f64, f64, f64),
}

/// Display-ready capacity data for one mounted filesystem.
struct FilesystemInfo {
    /// Human-readable filesystem or mount name.
    name: String,
    /// Mount point offered to the scanner.
    path: PathBuf,
    /// Filesystem format reported by GIO or the Linux mount table.
    fs_type: String,
    /// Total filesystem capacity in bytes.
    size: u64,
    /// Currently available capacity in bytes.
    free: u64,
    /// Whether the mount appears under a conventional removable-media path.
    removable: bool,
}

/// Mutable runtime state that is independent of concrete GTK widgets.
struct AppState {
    /// Immediate scan-root entries received from the worker.
    entries: Vec<ScanEntry>,
    /// Receiver for the currently active scan, when one exists.
    receiver: Option<mpsc::Receiver<ScanMessage>>,
    /// Cooperative cancellation flag shared with the scan worker.
    cancellation: Option<Arc<AtomicBool>>,
    /// Directory currently displayed or most recently selected.
    current_directory: Option<PathBuf>,
    /// Parent locations used by in-application Back navigation.
    history: Vec<PathBuf>,
    /// Ordering currently applied to visible result rows.
    sort: SortOrder,
}

/// Long-lived GTK widgets, preferences, and mutable application state.
///
/// The interface is reference counted because GTK signal closures need weak
/// handles back to shared state without creating ownership cycles.
struct AppUi {
    /// Primary application window.
    window: gtk::ApplicationWindow,
    /// Top-level switcher between filesystem overview and scan pages.
    page_stack: gtk::Stack,
    /// Navigation button returning through history or to the overview.
    back_button: gtk::Button,
    /// Header action opening the folder chooser.
    choose_button: gtk::Button,
    /// Header action requesting cooperative scan cancellation.
    cancel_button: gtk::Button,
    /// Custom About window with visible application title and metadata.
    about_dialog: gtk::Window,
    /// Modal preferences window.
    settings_window: gtk::Window,
    /// Column-heading font-family selector.
    font_combo: gtk::DropDown,
    /// Column-heading CSS pixel-size selector.
    font_size: gtk::SpinButton,
    /// Preference controlling secondary path labels.
    show_paths: gtk::CheckButton,
    /// Preference controlling startup and applied result ordering.
    default_sort: gtk::DropDown,
    /// Reloadable provider implementing font preferences immediately.
    column_style_provider: gtk::CssProvider,
    /// Text field filtering rows by name, path, or MIME description.
    search_entry: gtk::SearchEntry,
    /// Active result-order selector on the scan page.
    sort_combo: gtk::DropDown,
    /// Ellipsized label showing the directory being scanned.
    current_path: gtk::Label,
    /// Live scan or completion summary.
    status: gtk::Label,
    /// Indeterminate progress indicator displayed while scanning.
    progress: gtk::ProgressBar,
    /// List containing the current result rows.
    results: gtk::ListBox,
    /// Placeholder shown before entries arrive or when filtering finds none.
    empty_state: gtk::Box,
    /// Container holding the chart title, separator, and drawing area.
    distribution_panel: gtk::Box,
    /// Cairo-backed visualization of largest-entry proportions.
    distribution_chart: gtk::DrawingArea,
    /// Shared data consumed by the drawing callback.
    chart_data: Rc<RefCell<Vec<ChartSlice>>>,
    /// Persistable preferences and window state.
    preferences: RefCell<Settings>,
    /// Scan data, channel handles, history, and active sort order.
    state: RefCell<AppState>,
}

impl AppUi {
    /// Builds, connects, presents, and retains the complete application UI.
    ///
    /// # Arguments
    ///
    /// * `app` - Active GTK application that owns every top-level window.
    ///
    /// # Returns
    ///
    /// A reference-counted interface containing widgets and runtime state.
    ///
    /// # Side effects
    ///
    /// Loads preferences, installs CSS, discovers mounted filesystems, creates
    /// GTK windows and controls, starts the message timer, and presents the main
    /// window.
    fn new(app: &gtk::Application) -> Rc<Self> {
        let settings = Settings::load();
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Folder Lens")
            .default_width(settings.window_width)
            .default_height(settings.window_height)
            .build();
        let display = gtk::prelude::WidgetExt::display(&window);
        install_styles(&display);
        let column_style_provider =
            install_column_styles(&display, &settings.column_font, settings.column_font_size);

        let header = gtk::HeaderBar::new();
        let back_button = gtk::Button::from_icon_name("go-previous-symbolic");
        back_button.set_tooltip_text(Some("Back to the previous folder"));
        back_button.set_sensitive(false);
        let choose_button = gtk::Button::with_label("Choose Folder");
        choose_button.add_css_class("suggested-action");
        let cancel_button = gtk::Button::with_label("Cancel");
        cancel_button.set_sensitive(false);
        let settings_button = gtk::MenuButton::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text("Settings and About")
            .build();
        let settings_popover = gtk::Popover::new();
        let settings_menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        settings_menu.set_margin_top(6);
        settings_menu.set_margin_bottom(6);
        settings_menu.set_margin_start(6);
        settings_menu.set_margin_end(6);
        let open_settings = gtk::Button::new();
        open_settings.set_halign(gtk::Align::Fill);
        open_settings.add_css_class("flat");
        let settings_item = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        settings_item.append(&gtk::Image::from_icon_name("emblem-system-symbolic"));
        settings_item.append(&gtk::Label::new(Some("Settings")));
        open_settings.set_child(Some(&settings_item));

        let open_about = gtk::Button::new();
        open_about.set_halign(gtk::Align::Fill);
        open_about.add_css_class("flat");
        let about_item = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        about_item.append(&gtk::Image::from_icon_name("help-about-symbolic"));
        about_item.append(&gtk::Label::new(Some("About Folder Lens")));
        open_about.set_child(Some(&about_item));
        settings_menu.append(&open_settings);
        settings_menu.append(&open_about);
        settings_popover.set_child(Some(&settings_menu));
        settings_button.set_popover(Some(&settings_popover));
        let logo = app_logo();
        let title = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title_icon = gtk::Image::from_paintable(Some(&logo));
        title_icon.set_pixel_size(28);
        let title_label = gtk::Label::new(Some("Folder Lens"));
        title_label.add_css_class("title");
        title.append(&title_icon);
        title.append(&title_label);
        header.set_title_widget(Some(&title));
        header.pack_start(&back_button);
        header.pack_start(&choose_button);
        header.pack_start(&cancel_button);
        header.pack_end(&settings_button);
        window.set_titlebar(Some(&header));

        let about_dialog = gtk::Window::builder()
            .application(app)
            .transient_for(&window)
            .modal(true)
            .hide_on_close(true)
            .title("About Folder Lens")
            .default_width(390)
            .resizable(false)
            .build();
        let about_content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        about_content.set_margin_top(24);
        about_content.set_margin_bottom(18);
        about_content.set_margin_start(24);
        about_content.set_margin_end(24);
        about_content.set_halign(gtk::Align::Center);
        let about_logo = gtk::Image::from_paintable(Some(&logo));
        about_logo.set_pixel_size(112);
        let about_title = gtk::Label::new(Some("About Folder Lens"));
        about_title.add_css_class("title-1");
        let about_version =
            gtk::Label::new(Some(&format!("Version {}", env!("CARGO_PKG_VERSION"))));
        about_version.add_css_class("dim-label");
        let about_description = gtk::Label::new(Some(
            "Find what uses disk space with a fast, visual folder scanner.",
        ));
        about_description.set_wrap(true);
        about_description.set_justify(gtk::Justification::Center);
        about_description.set_max_width_chars(44);
        let about_license = gtk::Label::new(Some("Released under the MIT License"));
        about_license.add_css_class("dim-label");
        about_license.add_css_class("caption");
        let close_about = gtk::Button::with_label("Close");
        close_about.set_halign(gtk::Align::Center);
        about_content.append(&about_logo);
        about_content.append(&about_title);
        about_content.append(&about_version);
        about_content.append(&about_description);
        about_content.append(&about_license);
        about_content.append(&close_about);
        about_dialog.set_child(Some(&about_content));
        let about_window = about_dialog.clone();
        close_about.connect_clicked(move |_| about_window.set_visible(false));

        let settings_window = gtk::Window::builder()
            .application(app)
            .transient_for(&window)
            .modal(true)
            .hide_on_close(true)
            .title("Folder Lens Settings")
            .default_width(420)
            .build();
        let settings_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let settings_grid = gtk::Grid::new();
        settings_grid.set_row_spacing(14);
        settings_grid.set_column_spacing(18);
        settings_grid.set_margin_top(18);
        settings_grid.set_margin_bottom(18);
        settings_grid.set_margin_start(18);
        settings_grid.set_margin_end(18);

        let font_combo = gtk::DropDown::from_strings(&["Sans", "Serif", "Monospace"]);
        font_combo.set_selected(match settings.column_font.as_str() {
            "Serif" => 1,
            "Monospace" => 2,
            _ => 0,
        });
        let font_size = gtk::SpinButton::with_range(8.0, 18.0, 1.0);
        font_size.set_value(f64::from(settings.column_font_size));
        let show_paths = gtk::CheckButton::with_label("Show full paths beneath names");
        show_paths.set_active(settings.show_paths);
        let default_sort =
            gtk::DropDown::from_strings(&["Largest first", "Smallest first", "Name"]);
        default_sort.set_selected(settings.default_sort.min(2));

        let font_label = gtk::Label::new(Some("Column font"));
        font_label.set_xalign(0.0);
        let size_label = gtk::Label::new(Some("Column font size"));
        size_label.set_xalign(0.0);
        let sort_label = gtk::Label::new(Some("Default sorting"));
        sort_label.set_xalign(0.0);
        settings_grid.attach(&font_label, 0, 0, 1, 1);
        settings_grid.attach(&font_combo, 1, 0, 1, 1);
        settings_grid.attach(&size_label, 0, 1, 1, 1);
        settings_grid.attach(&font_size, 1, 1, 1, 1);
        settings_grid.attach(&sort_label, 0, 2, 1, 1);
        settings_grid.attach(&default_sort, 1, 2, 1, 1);
        settings_grid.attach(&show_paths, 0, 3, 2, 1);
        settings_content.append(&settings_grid);

        let settings_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        settings_actions.set_halign(gtk::Align::End);
        settings_actions.set_margin_bottom(12);
        settings_actions.set_margin_end(12);
        let cancel_settings = gtk::Button::with_label("Cancel");
        let apply_settings = gtk::Button::with_label("Apply");
        apply_settings.add_css_class("suggested-action");
        settings_actions.append(&cancel_settings);
        settings_actions.append(&apply_settings);
        settings_content.append(&settings_actions);
        settings_window.set_child(Some(&settings_content));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.set_margin_top(16);
        root.set_margin_bottom(16);
        root.set_margin_start(16);
        root.set_margin_end(16);

        let page_stack = gtk::Stack::new();
        page_stack.set_vexpand(true);
        let scan_page = gtk::Box::new(gtk::Orientation::Vertical, 12);

        let current_path = gtk::Label::new(Some("Choose a folder to see what is using its space"));
        current_path.set_xalign(0.0);
        current_path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        current_path.add_css_class("title-3");
        scan_page.append(&current_path);

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Filter by name, path, or type")
            .hexpand(true)
            .build();
        let sort_combo = gtk::DropDown::from_strings(&["Largest first", "Smallest first", "Name"]);
        sort_combo.set_selected(settings.default_sort.min(2));
        controls.append(&search_entry);
        controls.append(&sort_combo);
        scan_page.append(&controls);

        let progress = gtk::ProgressBar::new();
        progress.set_visible(false);
        progress.set_show_text(false);
        scan_page.append(&progress);

        let status = gtk::Label::new(Some("No scan yet"));
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        scan_page.append(&status);

        let results = gtk::ListBox::new();
        results.set_selection_mode(gtk::SelectionMode::None);
        results.add_css_class("results-list");

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
        let results_page = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let column_header = gtk::Grid::new();
        column_header.add_css_class("result-header");

        let icon_spacer = gtk::Label::new(None);
        icon_spacer.set_width_request(32);
        icon_spacer.add_css_class("table-cell");
        column_header.attach(&icon_spacer, 0, 0, 1, 1);

        let name_header = gtk::Label::new(Some("Name"));
        name_header.set_xalign(0.0);
        name_header.set_hexpand(true);
        name_header.add_css_class("column-heading");
        name_header.add_css_class("table-cell");
        column_header.attach(&name_header, 1, 0, 1, 1);

        let type_header = gtk::Label::new(Some("Type"));
        type_header.set_width_request(TYPE_COLUMN_WIDTH);
        type_header.set_xalign(0.0);
        type_header.add_css_class("column-heading");
        type_header.add_css_class("table-cell");
        type_header.add_css_class("column-divider");
        column_header.attach(&type_header, 2, 0, 1, 1);

        let size_header = gtk::Label::new(Some("Size"));
        size_header.set_width_request(SIZE_COLUMN_WIDTH);
        size_header.set_xalign(1.0);
        size_header.add_css_class("column-heading");
        size_header.add_css_class("table-cell");
        size_header.add_css_class("column-divider");
        column_header.attach(&size_header, 3, 0, 1, 1);

        let actions_header = gtk::Label::new(Some("Open"));
        actions_header.set_width_request(OPEN_COLUMN_WIDTH);
        actions_header.set_xalign(0.5);
        actions_header.add_css_class("column-heading");
        actions_header.add_css_class("table-cell");
        actions_header.add_css_class("column-divider");
        column_header.attach(&actions_header, 4, 0, 1, 1);
        let table_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        table_content.append(&column_header);
        table_content.append(&results);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&table_content)
            .build();
        let results_table = gtk::Box::new(gtk::Orientation::Vertical, 0);
        results_table.set_vexpand(true);
        results_table.add_css_class("results-table");
        results_table.append(&scroll);
        results_page.append(&results_table);
        stack.add_named(&empty_state, Some("empty"));
        stack.add_named(&results_page, Some("results"));
        stack.set_visible_child_name("empty");
        scan_page.append(&stack);

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
        scan_page.append(&distribution_panel);

        let (overview, filesystem_buttons) = filesystem_overview();
        page_stack.add_named(&overview, Some("overview"));
        page_stack.add_named(&scan_page, Some("scan"));
        page_stack.set_visible_child_name("overview");
        root.append(&page_stack);
        window.set_child(Some(&root));

        let ui = Rc::new(Self {
            window,
            page_stack,
            back_button,
            choose_button,
            cancel_button,
            about_dialog,
            settings_window,
            font_combo,
            font_size,
            show_paths,
            default_sort,
            column_style_provider,
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
            preferences: RefCell::new(settings.clone()),
            state: RefCell::new(AppState {
                entries: Vec::new(),
                receiver: None,
                cancellation: None,
                current_directory: settings.last_directory,
                history: Vec::new(),
                sort: sort_order(settings.default_sort),
            }),
        });

        let weak = Rc::downgrade(&ui);
        let popover = settings_popover.clone();
        open_about.connect_clicked(move |_| {
            popover.popdown();
            if let Some(ui) = weak.upgrade() {
                ui.about_dialog.present();
            }
        });

        let weak = Rc::downgrade(&ui);
        let popover = settings_popover.clone();
        open_settings.connect_clicked(move |_| {
            popover.popdown();
            if let Some(ui) = weak.upgrade() {
                ui.sync_settings_controls();
                ui.settings_window.present();
            }
        });

        let weak = Rc::downgrade(&ui);
        cancel_settings.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.settings_window.set_visible(false);
            }
        });

        let weak = Rc::downgrade(&ui);
        apply_settings.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.apply_settings();
                ui.settings_window.set_visible(false);
            }
        });

        for (button, path) in filesystem_buttons {
            let weak = Rc::downgrade(&ui);
            button.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.start_fresh_scan(path.clone());
                }
            });
        }

        // The stack is found through the empty-state parent when results are refreshed.
        ui.connect_signals();
        ui.start_message_pump();
        ui.window.present();
        ui
    }

    /// Connects user-interface events to application behavior.
    ///
    /// # Side effects
    ///
    /// Registers callbacks for navigation, choosing and cancelling scans,
    /// filtering, sorting, applying settings, and clean application shutdown.
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
                ui.save_settings(window.width(), window.height());
            }
            APP_UI.with(|slot| {
                slot.borrow_mut().take();
            });
            glib::Propagation::Proceed
        });
    }

    /// Copies persisted preference values into the settings controls.
    ///
    /// # Side effects
    ///
    /// Updates the font, size, path-visibility, and sorting widgets so Cancel
    /// can discard un-applied edits from a previous settings-window visit.
    fn sync_settings_controls(&self) {
        let settings = self.preferences.borrow();
        self.font_combo
            .set_selected(match settings.column_font.as_str() {
                "Serif" => 1,
                "Monospace" => 2,
                _ => 0,
            });
        self.font_size
            .set_value(f64::from(settings.column_font_size));
        self.show_paths.set_active(settings.show_paths);
        self.default_sort.set_selected(settings.default_sort.min(2));
    }

    /// Applies settings-window values to the live interface and persistence.
    ///
    /// # Side effects
    ///
    /// Updates the dynamic heading stylesheet, active sort order, result rows,
    /// in-memory preferences, and the JSON settings file.
    fn apply_settings(self: &Rc<Self>) {
        let font = match self.font_combo.selected() {
            1 => "Serif",
            2 => "Monospace",
            _ => "Sans",
        };
        let font_size = self.font_size.value_as_int().clamp(8, 18);
        let default_sort = self.default_sort.selected().min(2);
        {
            let mut settings = self.preferences.borrow_mut();
            settings.column_font = font.into();
            settings.column_font_size = font_size;
            settings.show_paths = self.show_paths.is_active();
            settings.default_sort = default_sort;
        }

        update_column_styles(&self.column_style_provider, font, font_size);
        self.state.borrow_mut().sort = sort_order(default_sort);
        self.sort_combo.set_selected(default_sort);
        self.refresh_results();
        self.save_settings(self.window.width(), self.window.height());
    }

    /// Saves preferences together with restorable window and directory state.
    ///
    /// # Arguments
    ///
    /// * `width` - Current main-window width in logical pixels.
    /// * `height` - Current main-window height in logical pixels.
    ///
    /// # Side effects
    ///
    /// Writes the merged settings value to the user configuration directory.
    fn save_settings(&self, width: i32, height: i32) {
        let mut settings = self.preferences.borrow().clone();
        settings.window_width = width;
        settings.window_height = height;
        settings.last_directory = self.state.borrow().current_directory.clone();
        settings.save();
    }

    /// Opens a native directory chooser and starts a scan on acceptance.
    ///
    /// # Side effects
    ///
    /// Presents an asynchronous GTK file dialog. A successful selection clears
    /// navigation history and begins scanning the selected path.
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

    /// Starts a scan that becomes the root of a new navigation history.
    ///
    /// # Arguments
    ///
    /// * `path` - Directory selected from the overview or folder chooser.
    ///
    /// # Side effects
    ///
    /// Clears previous history, enables Back for returning to the overview, and
    /// delegates worker creation to [`AppUi::start_scan`].
    fn start_fresh_scan(self: &Rc<Self>, path: PathBuf) {
        self.state.borrow_mut().history.clear();
        self.back_button.set_sensitive(true);
        self.start_scan(path);
    }

    /// Navigates into a child directory while retaining the current location.
    ///
    /// # Arguments
    ///
    /// * `path` - Child directory that should become the new scan root.
    ///
    /// # Side effects
    ///
    /// Pushes the previous location onto history and starts a replacement scan.
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

    /// Returns to the preceding directory or the filesystem overview.
    ///
    /// # Side effects
    ///
    /// Pops navigation history and rescans that path. When history is empty,
    /// cancels active work and switches to the overview page.
    fn navigate_back(self: &Rc<Self>) {
        let previous = self.state.borrow_mut().history.pop();
        if let Some(previous) = previous {
            // Keep Back available so the overview remains reachable from the scan root.
            self.back_button.set_sensitive(true);
            self.start_scan(previous);
        } else {
            self.cancel_scan();
            self.back_button.set_sensitive(false);
            self.page_stack.set_visible_child_name("overview");
        }
    }

    /// Initializes UI state and launches a background scan worker.
    ///
    /// # Arguments
    ///
    /// * `path` - Directory whose immediate entries should be measured.
    ///
    /// # Side effects
    ///
    /// Cancels prior work, replaces the message channel and cancellation flag,
    /// clears results, updates scan controls, and spawns a worker thread.
    fn start_scan(self: &Rc<Self>, path: PathBuf) {
        self.cancel_scan();
        self.page_stack.set_visible_child_name("scan");
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

    /// Requests cancellation of the active worker, if one exists.
    ///
    /// # Side effects
    ///
    /// Atomically sets the cooperative cancellation flag. The worker observes
    /// the request between filesystem entries.
    fn cancel_scan(&self) {
        if let Some(cancellation) = self.state.borrow().cancellation.as_ref() {
            cancellation.store(true, Ordering::Relaxed);
        }
    }

    /// Starts the GTK-main-loop timer that consumes worker messages.
    ///
    /// # Side effects
    ///
    /// Registers an 80-millisecond repeating callback. The callback stops when
    /// the weak interface reference can no longer be upgraded.
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

    /// Drains available worker messages and updates presentation state.
    ///
    /// # Side effects
    ///
    /// Pulses progress, appends entries, updates counters and terminal status,
    /// handles root errors, and refreshes the result table when required.
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

    /// Displays a live scan-status summary.
    ///
    /// # Arguments
    ///
    /// * `stats` - Latest worker counters.
    ///
    /// # Side effects
    ///
    /// Replaces the status-label text with formatted counts and byte totals.
    fn show_progress(&self, stats: ScanStats) {
        self.status.set_text(&format!(
            "Scanning… {} files, {} folders, {} found{}",
            stats.files,
            stats.directories,
            format_size(stats.bytes),
            error_suffix(stats.errors)
        ));
    }

    /// Finalizes scan controls and reports completion or cancellation.
    ///
    /// # Arguments
    ///
    /// * `stats` - Final worker counters.
    /// * `cancelled` - Whether the worker stopped because cancellation won.
    ///
    /// # Side effects
    ///
    /// Updates status text, hides progress, disables Cancel, and releases the
    /// receiver and shared cancellation flag.
    fn finish_scan(&self, stats: ScanStats, cancelled: bool) {
        let prefix = if cancelled {
            "Scan cancelled"
        } else {
            "Scan complete"
        };
        self.status.set_text(&format!(
            "{prefix} - {} files, {} folders, {}{}",
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

    /// Rebuilds filtered and sorted result rows plus chart data.
    ///
    /// # Side effects
    ///
    /// Removes existing list rows, applies search and ordering, creates new GTK
    /// rows, updates chart slices, queues redraw, and selects the empty or result
    /// stack page.
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
                    || entry_type(entry).to_lowercase().contains(&query)
            })
            .collect();

        match state.sort {
            SortOrder::Largest => entries.sort_by_key(|entry| Reverse(entry.size)),
            SortOrder::Smallest => entries.sort_by_key(|entry| entry.size),
            SortOrder::Name => entries.sort_by_key(|entry| entry.name.to_lowercase()),
        }

        let show_paths = self.preferences.borrow().show_paths;
        for entry in entries {
            self.results
                .append(&result_row(entry, Rc::downgrade(self), show_paths));
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

/// Builds the initial filesystem-capacity page and its deferred scan actions.
///
/// # Returns
///
/// A scrollable overview widget plus `(button, mount_path)` pairs that the
/// caller connects after [`AppUi`] has been placed in an [`Rc`].
///
/// # Side effects
///
/// Reads the current mount table and queries capacity metadata through GIO.
fn filesystem_overview() -> (gtk::ScrolledWindow, Vec<(gtk::Button, PathBuf)>) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(8);
    content.set_margin_bottom(8);

    let title = gtk::Label::new(Some("Filesystems"));
    title.set_xalign(0.0);
    title.add_css_class("title-1");
    content.append(&title);

    let description = gtk::Label::new(Some(
        "Choose a mounted filesystem to inspect, or choose a specific folder from the header.",
    ));
    description.set_xalign(0.0);
    description.set_wrap(true);
    description.add_css_class("dim-label");
    content.append(&description);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    list.add_css_class("filesystem-list");
    let mut buttons = Vec::new();

    for filesystem in mounted_filesystems() {
        let row = gtk::ListBoxRow::new();
        let row_content = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        row_content.set_margin_top(12);
        row_content.set_margin_bottom(12);
        row_content.set_margin_start(14);
        row_content.set_margin_end(14);

        let icon_name = if filesystem.removable {
            "drive-removable-media-symbolic"
        } else {
            "drive-harddisk-symbolic"
        };
        let icon = gtk::Image::from_icon_name(icon_name);
        icon.set_pixel_size(32);
        row_content.append(&icon);

        let identity = gtk::Box::new(gtk::Orientation::Vertical, 3);
        identity.set_hexpand(true);
        let name = gtk::Label::new(Some(&filesystem.name));
        name.set_xalign(0.0);
        name.add_css_class("heading");
        let details = gtk::Label::new(Some(&format!(
            "{}  •  {}",
            filesystem.path.display(),
            filesystem.fs_type
        )));
        details.set_xalign(0.0);
        details.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        details.add_css_class("dim-label");
        details.add_css_class("caption");
        identity.append(&name);
        identity.append(&details);
        row_content.append(&identity);

        let used = filesystem.size.saturating_sub(filesystem.free);
        let capacity = gtk::Box::new(gtk::Orientation::Vertical, 5);
        capacity.set_width_request(250);
        let summary = gtk::Label::new(Some(&format!(
            "{} used of {}  •  {} free",
            format_size(used),
            format_size(filesystem.size),
            format_size(filesystem.free)
        )));
        summary.set_xalign(0.0);
        summary.add_css_class("caption");
        let usage = gtk::ProgressBar::new();
        let fraction = if filesystem.size == 0 {
            0.0
        } else {
            used as f64 / filesystem.size as f64
        };
        usage.set_fraction(fraction.clamp(0.0, 1.0));
        capacity.append(&summary);
        capacity.append(&usage);
        row_content.append(&capacity);

        let scan = gtk::Button::with_label("Scan");
        scan.set_valign(gtk::Align::Center);
        row_content.append(&scan);
        buttons.push((scan, filesystem.path));

        row.set_child(Some(&row_content));
        list.append(&row);
    }

    if list.first_child().is_none() {
        let unavailable = gtk::Label::new(Some("No mounted filesystems could be read."));
        unavailable.set_margin_top(24);
        unavailable.set_margin_bottom(24);
        unavailable.add_css_class("dim-label");
        list.append(&unavailable);
    }

    content.append(&list);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .build();
    (scroll, buttons)
}

/// Discovers user-relevant mounted filesystems and queries their capacities.
///
/// # Returns
///
/// Display-ready mount records ordered with the system root first, followed by
/// case-insensitive names.
///
/// # Side effects
///
/// Reads `/proc/self/mountinfo` and performs read-only GIO filesystem metadata
/// queries. Package loop mounts and zero-capacity entries are omitted.
fn mounted_filesystems() -> Vec<FilesystemInfo> {
    let mut seen = HashSet::new();
    let mut filesystems = Vec::new();
    let mount_table = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();

    for line in mount_table.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        if fields.len() <= separator + 2 || fields.len() <= 4 {
            continue;
        }

        let path = PathBuf::from(decode_mount_field(fields[4]));
        let fs_type = fields[separator + 1].to_owned();
        let device = fields[separator + 2];
        if is_package_mount(&fs_type, device, &path) {
            continue;
        }
        let removable = path.starts_with("/media") || path.starts_with("/run/media");
        let should_show = path == Path::new("/") || device.starts_with("/dev/") || removable;
        if !should_show || !seen.insert(path.clone()) {
            continue;
        }

        let file = gio::File::for_path(&path);
        let Ok(info) = file.query_filesystem_info(
            "filesystem::size,filesystem::free,filesystem::type",
            None::<&gio::Cancellable>,
        ) else {
            continue;
        };
        let size = info.attribute_uint64("filesystem::size");
        if size == 0 {
            continue;
        }

        let fs_type = info
            .attribute_string("filesystem::type")
            .map(|value| value.to_string())
            .unwrap_or(fs_type);
        let name = if path == Path::new("/") {
            "System".into()
        } else {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| path.display().to_string())
        };
        filesystems.push(FilesystemInfo {
            name,
            path,
            fs_type,
            size,
            free: info.attribute_uint64("filesystem::free"),
            removable,
        });
    }

    filesystems.sort_by(|left, right| {
        let left_is_root = left.path == Path::new("/");
        let right_is_root = right.path == Path::new("/");
        right_is_root
            .cmp(&left_is_root)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    filesystems
}

/// Decodes the escape sequences used in Linux mount-table path fields.
///
/// # Arguments
///
/// * `value` - Encoded mount-table token.
///
/// # Returns
///
/// A path string with escaped spaces, tabs, newlines, and backslashes restored.
fn decode_mount_field(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// Identifies package-image mounts that should not appear as physical drives.
///
/// # Arguments
///
/// * `fs_type` - Filesystem format reported in the mount table.
/// * `device` - Mount source such as `/dev/loop7`.
/// * `path` - Mounted location.
///
/// # Returns
///
/// `true` for SquashFS, loop devices, and paths beneath `/snap`.
fn is_package_mount(fs_type: &str, device: &str, path: &Path) -> bool {
    fs_type == "squashfs" || device.starts_with("/dev/loop") || path.starts_with("/snap")
}

/// Converts scan entries into the largest chart segments and an Other group.
///
/// # Arguments
///
/// * `entries` - Complete immediate-child results for the active scan root.
///
/// # Returns
///
/// Up to seven individually colored slices plus one gray aggregate for all
/// remaining non-empty entries.
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

/// Draws the proportional space bar and its item legend.
///
/// # Arguments
///
/// * `area` - GTK drawing area supplying the current foreground color.
/// * `context` - Cairo context receiving rectangles and legend text.
/// * `width` - Current drawing-area width in logical pixels.
/// * `_height` - Current height, reserved for future responsive layout.
/// * `slices` - Prepared chart segments ordered from largest to smallest.
///
/// # Side effects
///
/// Draws colored proportional segments, swatches, names, sizes, and percentages
/// into `context`. Cairo drawing failures are intentionally ignored.
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
            "{} - {} ({percentage:.1}%)",
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

/// Shortens a label to a maximum character count with a trailing ellipsis.
///
/// # Arguments
///
/// * `value` - Unicode string to display.
/// * `max_chars` - Maximum number of Unicode scalar values in the result.
///
/// # Returns
///
/// The original string when it fits, otherwise a shortened owned string ending
/// in `…`.
fn elide(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut shortened: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    shortened.push('…');
    shortened
}

/// Builds one interactive row in the scan-results table.
///
/// # Arguments
///
/// * `entry` - Scan result represented by the row.
/// * `ui` - Weak interface reference used by folder context-menu navigation.
/// * `show_path` - Whether the secondary full-path label is visible.
///
/// # Returns
///
/// A list-box row containing icon, name/path, type, size, and Open action.
///
/// # Side effects
///
/// Connects the Open button and installs a secondary-click context menu.
fn result_row(entry: &ScanEntry, ui: Weak<AppUi>, show_path: bool) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let content = gtk::Grid::new();
    content.add_css_class("result-row");

    let icon_name = if entry.is_dir {
        "folder-symbolic"
    } else {
        "text-x-generic-symbolic"
    };
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_width_request(32);
    icon.add_css_class("table-cell");
    content.attach(&icon, 0, 0, 1, 1);

    let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
    labels.set_hexpand(true);
    labels.add_css_class("table-cell");
    let name = gtk::Label::new(Some(&entry.name));
    name.set_xalign(0.0);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let path_label = gtk::Label::new(Some(&entry.path.to_string_lossy()));
    path_label.set_xalign(0.0);
    path_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    path_label.add_css_class("dim-label");
    path_label.add_css_class("caption");
    path_label.set_visible(show_path);
    labels.append(&name);
    labels.append(&path_label);
    content.attach(&labels, 1, 0, 1, 1);

    let file_type = entry_type(entry);
    let type_label = gtk::Label::new(Some(&file_type));
    type_label.set_width_request(TYPE_COLUMN_WIDTH);
    type_label.set_max_width_chars(18);
    type_label.set_xalign(0.0);
    type_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    type_label.set_tooltip_text(Some(&file_type));
    type_label.add_css_class("dim-label");
    type_label.add_css_class("table-cell");
    content.attach(&type_label, 2, 0, 1, 1);

    let size = gtk::Label::new(Some(&format_size(entry.size)));
    size.set_width_request(SIZE_COLUMN_WIDTH);
    size.set_xalign(1.0);
    size.add_css_class("table-cell");
    content.attach(&size, 3, 0, 1, 1);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_width_request(OPEN_COLUMN_WIDTH);
    actions.set_halign(gtk::Align::Center);
    actions.add_css_class("table-cell");

    let open = gtk::Button::from_icon_name("folder-open-symbolic");
    open.set_tooltip_text(Some("Open with the default application"));
    let path = entry.path.clone();
    open.connect_clicked(move |_| open_path(&path));
    actions.append(&open);
    content.attach(&actions, 4, 0, 1, 1);

    row.set_child(Some(&content));
    attach_context_menu(&row, entry, ui);
    row
}

/// Produces a human-readable type description without reading file contents.
///
/// # Arguments
///
/// * `entry` - Result whose directory flag or filename should be classified.
///
/// # Returns
///
/// `Folder` for directories, otherwise the desktop MIME database description
/// inferred from the path and extension.
fn entry_type(entry: &ScanEntry) -> String {
    if entry.is_dir {
        return "Folder".into();
    }

    let (content_type, _) = gio::content_type_guess(Some(&entry.path), None::<&[u8]>);
    gio::content_type_get_description(&content_type).into()
}

/// Attaches right-click actions to a result row.
///
/// # Arguments
///
/// * `row` - Row receiving the secondary-click gesture and popover.
/// * `entry` - Entry supplying action paths and directory behavior.
/// * `ui` - Weak application reference used by Scan this folder.
///
/// # Side effects
///
/// Creates a popover with navigation, Open, and Copy path actions and registers
/// a capture-phase secondary mouse gesture.
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

/// Opens a path with the desktop's default application.
///
/// # Arguments
///
/// * `path` - File or directory to launch.
///
/// # Side effects
///
/// Requests URI handling through GIO and writes an error to standard error when
/// no application can open the path.
fn open_path(path: &Path) {
    let uri = gio::File::for_path(path).uri();
    if let Err(error) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        eprintln!("Could not open {}: {error}", path.display());
    }
}

/// Formats a byte count with binary units suitable for compact UI labels.
///
/// # Arguments
///
/// * `bytes` - Raw byte count.
///
/// # Returns
///
/// Bytes as an integer below 1 KiB, otherwise one decimal place using KiB, MiB,
/// GiB, or TiB.
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

/// Formats the unreadable-item suffix appended to scan status messages.
///
/// # Arguments
///
/// * `errors` - Number of unreadable entries encountered by the scanner.
///
/// # Returns
///
/// An empty string for zero, or a correctly singular/plural parenthetical note.
fn error_suffix(errors: u64) -> String {
    match errors {
        0 => String::new(),
        1 => " (1 unreadable item)".into(),
        count => format!(" ({count} unreadable items)"),
    }
}

/// Starts the GTK application and creates one interface per process.
///
/// # Returns
///
/// GTK's process exit code after the application main loop ends.
///
/// # Side effects
///
/// Registers the application identifier, handles activation, presents an
/// existing window on repeated activation, and runs the GTK event loop.
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
/// Unit tests for formatting, chart grouping, truncation, and mount filtering.
mod tests {
    use super::{chart_slices, elide, format_size, is_package_mount};
    use crate::scanner::ScanEntry;
    use std::path::PathBuf;

    #[test]
    /// Verifies byte values use the expected binary-unit presentation.
    fn formats_sizes_for_people() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1_572_864), "1.5 MiB");
    }

    #[test]
    /// Verifies entries beyond the largest seven are aggregated as Other.
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
    /// Verifies chart labels remain unchanged or gain a Unicode ellipsis.
    fn long_chart_labels_are_shortened() {
        assert_eq!(elide("documents", 20), "documents");
        assert_eq!(elide("very-long-directory", 10), "very-long…");
    }

    #[test]
    /// Verifies package images are hidden while physical filesystems remain.
    fn snap_and_loop_mounts_are_hidden_from_the_overview() {
        assert!(is_package_mount(
            "squashfs",
            "/dev/loop7",
            PathBuf::from("/snap/firefox/123").as_path()
        ));
        assert!(!is_package_mount(
            "ext4",
            "/dev/nvme0n1p2",
            PathBuf::from("/").as_path()
        ));
    }
}
