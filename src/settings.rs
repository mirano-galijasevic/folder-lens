use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub window_width: i32,
    pub window_height: i32,
    pub last_directory: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            window_width: 920,
            window_height: 640,
            last_directory: None,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        fs::read_to_string(path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default()
    }

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

fn settings_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "FolderLens", "Folder Lens")
        .map(|dirs| dirs.config_dir().join("settings.json"))
}
