//! Persistent application preferences.
//!
//! This module resolves Folder Lens' platform-appropriate configuration path,
//! supplies backward-compatible defaults, and serializes settings as readable
//! JSON. Read and write failures intentionally fall back or return silently so
//! a damaged preferences file never prevents the application from starting.

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
/// User preferences and restorable window state.
///
/// Missing JSON properties are populated through [`Default`], allowing newer
/// Folder Lens versions to read settings files created by older releases.
pub struct Settings {
    /// Last saved width of the main window in logical pixels.
    pub window_width: i32,
    /// Last saved height of the main window in logical pixels.
    pub window_height: i32,
    /// Most recently scanned directory, when one has been selected.
    pub last_directory: Option<PathBuf>,
    /// CSS font family used by the result-table column headings.
    pub column_font: String,
    /// CSS pixel size used by the result-table column headings.
    pub column_font_size: i32,
    /// Whether result rows display the full path beneath each item name.
    pub show_paths: bool,
    /// Selected default sort index: largest, smallest, or name.
    pub default_sort: u32,
}

/// Supplies safe first-run values and defaults for newly introduced fields.
impl Default for Settings {
    /// Creates the initial Folder Lens preference set.
    ///
    /// # Returns
    ///
    /// A configuration using a 920×640 window, 12-pixel Sans column headings,
    /// visible paths, and largest-first sorting.
    fn default() -> Self {
        Self {
            window_width: 920,
            window_height: 640,
            last_directory: None,
            column_font: "Sans".into(),
            column_font_size: 12,
            show_paths: true,
            default_sort: 0,
        }
    }
}

impl Settings {
    /// Loads settings from the platform-specific JSON configuration file.
    ///
    /// # Returns
    ///
    /// The decoded settings, or [`Settings::default`] when the configuration
    /// directory cannot be resolved, the file does not exist, or JSON parsing
    /// fails.
    ///
    /// # Side effects
    ///
    /// Reads the settings file from disk when it is available.
    pub fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        fs::read_to_string(path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default()
    }

    /// Persists the complete settings value as pretty-printed JSON.
    ///
    /// # Side effects
    ///
    /// Creates the application configuration directory when necessary and
    /// replaces the settings file. Filesystem and serialization failures are
    /// ignored because preferences must not interrupt normal application use.
    pub fn save(&self) {
        let Some(path) = settings_path() else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_ok() {
            if let Ok(contents) = serde_json::to_string_pretty(self) {
                let _ = fs::write(path, contents);
            }
        }
    }
}

/// Resolves the platform-appropriate Folder Lens settings file.
///
/// # Returns
///
/// The configuration path ending in `settings.json`, or `None` when the host
/// platform cannot provide a user configuration directory.
fn settings_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "FolderLens", "Folder Lens")
        .map(|dirs| dirs.config_dir().join("settings.json"))
}

#[cfg(test)]
/// Unit tests for preference defaults and compatibility behavior.
mod tests {
    use super::Settings;

    #[test]
    /// Verifies that new configurations use the documented 12-pixel heading.
    fn default_column_font_size_is_twelve_pixels() {
        assert_eq!(Settings::default().column_font_size, 12);
    }
}
