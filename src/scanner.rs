//! Recursive filesystem scanning and progress reporting.
//!
//! Scans run on a worker thread and communicate with the GTK interface through
//! [`ScanMessage`] values. The scanner reads directory entries and metadata but
//! deliberately does not follow symbolic links or inspect regular-file content.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};

#[derive(Clone, Debug)]
/// One immediate child of the directory selected for scanning.
pub struct ScanEntry {
    /// Absolute or caller-provided path to the filesystem entry.
    pub path: PathBuf,
    /// Display name derived from the final path component.
    pub name: String,
    /// Regular-file length or recursively accumulated directory length.
    pub size: u64,
    /// Whether the entry is a directory and can be navigated into.
    pub is_dir: bool,
}

#[derive(Clone, Copy, Debug, Default)]
/// Running counters collected during one scan operation.
pub struct ScanStats {
    /// Number of regular files whose metadata was successfully considered.
    pub files: u64,
    /// Number of directories visited below the selected root.
    pub directories: u64,
    /// Saturating sum of regular-file lengths in bytes.
    pub bytes: u64,
    /// Number of entries or metadata records that could not be read.
    pub errors: u64,
}

#[derive(Debug)]
/// Worker-to-interface events emitted during a scan.
pub enum ScanMessage {
    /// Updated counters for refreshing the live status line.
    Progress(ScanStats),
    /// Completed size information for one immediate child of the scan root.
    Entry(ScanEntry),
    /// Successful completion with final counters.
    Finished(ScanStats),
    /// Cooperative cancellation with counters collected before stopping.
    Cancelled(ScanStats),
    /// Fatal failure to open the selected root directory.
    RootError(String),
}

/// Scans the immediate children of a root and recursively sizes directories.
///
/// # Arguments
///
/// * `root` - Directory whose immediate children should become result rows.
/// * `sender` - Channel used to stream entries, progress, and terminal status.
/// * `cancelled` - Shared flag checked before and during recursive traversal.
///
/// # Side effects
///
/// Reads directory entries and file metadata, updates no filesystem content,
/// and sends [`ScanMessage`] values until the scan finishes, is cancelled, or
/// the root cannot be opened.
pub fn scan_root(root: PathBuf, sender: Sender<ScanMessage>, cancelled: Arc<AtomicBool>) {
    let mut stats = ScanStats::default();
    let children = match fs::read_dir(&root) {
        Ok(children) => children,
        Err(error) => {
            let _ = sender.send(ScanMessage::RootError(format!(
                "Could not read {}: {error}",
                root.display()
            )));
            return;
        }
    };

    for child in children {
        if cancelled.load(Ordering::Relaxed) {
            let _ = sender.send(ScanMessage::Cancelled(stats));
            return;
        }

        let child = match child {
            Ok(child) => child,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        let path = child.path();
        let metadata = match child.file_type() {
            Ok(metadata) => metadata,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };

        let size = if metadata.is_dir() {
            stats.directories += 1;
            directory_size(&path, &mut stats, &sender, &cancelled)
        } else if metadata.is_file() {
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or_else(|_| {
                stats.errors += 1;
                0
            });
            stats.files += 1;
            stats.bytes = stats.bytes.saturating_add(size);
            size
        } else {
            // Symlinks and special files are deliberately not followed.
            0
        };

        if cancelled.load(Ordering::Relaxed) {
            let _ = sender.send(ScanMessage::Cancelled(stats));
            return;
        }

        let name = child.file_name().to_string_lossy().into_owned();
        let _ = sender.send(ScanMessage::Entry(ScanEntry {
            path,
            name,
            size,
            is_dir: metadata.is_dir(),
        }));
        let _ = sender.send(ScanMessage::Progress(stats));
    }

    let _ = sender.send(ScanMessage::Finished(stats));
}

/// Recursively totals regular files beneath one directory.
///
/// # Arguments
///
/// * `path` - Directory currently being traversed.
/// * `stats` - Shared counters updated for every visited file and directory.
/// * `sender` - Channel receiving periodic progress snapshots.
/// * `cancelled` - Cooperative cancellation flag checked between entries.
///
/// # Returns
///
/// The saturating sum of regular-file lengths reachable below `path`.
///
/// # Side effects
///
/// Reads directory entries and metadata, increments error counters for
/// unreadable items, and periodically sends progress messages.
fn directory_size(
    path: &Path,
    stats: &mut ScanStats,
    sender: &Sender<ScanMessage>,
    cancelled: &AtomicBool,
) -> u64 {
    let children = match fs::read_dir(path) {
        Ok(children) => children,
        Err(_) => {
            stats.errors += 1;
            return 0;
        }
    };
    let mut total = 0_u64;

    for child in children {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let child = match child {
            Ok(child) => child,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        let file_type = match child.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };

        let size = if file_type.is_dir() {
            stats.directories += 1;
            directory_size(&child.path(), stats, sender, cancelled)
        } else if file_type.is_file() {
            stats.files += 1;
            match child.metadata() {
                Ok(metadata) => {
                    let size = metadata.len();
                    stats.bytes = stats.bytes.saturating_add(size);
                    size
                }
                Err(_) => {
                    stats.errors += 1;
                    0
                }
            }
        } else {
            0
        };

        total = total.saturating_add(size);
        if stats.files % 250 == 0 {
            let _ = sender.send(ScanMessage::Progress(*stats));
        }
    }

    total
}

#[cfg(test)]
/// Behavioral tests for recursive totals and cancellation.
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc;

    /// Monotonic suffix used to keep temporary test paths distinct.
    static TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

    /// Automatically removed directory used to construct scanner fixtures.
    struct TestDirectory(PathBuf);

    impl TestDirectory {
        /// Creates a process-specific empty temporary directory.
        ///
        /// # Returns
        ///
        /// An owned fixture whose directory is removed when dropped.
        ///
        /// # Panics
        ///
        /// Panics when the fixture directory cannot be created.
        fn new() -> Self {
            let id = TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("folder-lens-test-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        /// Borrows the fixture directory path.
        ///
        /// # Returns
        ///
        /// The path backing this temporary fixture.
        fn path(&self) -> &Path {
            &self.0
        }
    }

    /// Removes the temporary fixture and everything created beneath it.
    impl Drop for TestDirectory {
        /// Performs best-effort recursive cleanup.
        ///
        /// # Side effects
        ///
        /// Deletes the fixture directory tree; cleanup errors are ignored.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    /// Confirms immediate entries and recursive byte totals are reported.
    fn reports_top_level_entries_and_recursive_sizes() {
        let temp = TestDirectory::new();
        fs::create_dir(temp.path().join("documents")).unwrap();
        fs::File::create(temp.path().join("documents/notes.txt"))
            .unwrap()
            .write_all(&[0; 12])
            .unwrap();
        fs::File::create(temp.path().join("photo.bin"))
            .unwrap()
            .write_all(&[0; 7])
            .unwrap();

        let (sender, receiver) = mpsc::channel();
        scan_root(
            temp.path().to_path_buf(),
            sender,
            Arc::new(AtomicBool::new(false)),
        );
        let messages: Vec<_> = receiver.into_iter().collect();
        let mut entries: Vec<_> = messages
            .iter()
            .filter_map(|message| match message {
                ScanMessage::Entry(entry) => Some((entry.name.as_str(), entry.size)),
                _ => None,
            })
            .collect();
        entries.sort_unstable();

        assert_eq!(entries, vec![("documents", 12), ("photo.bin", 7)]);
        assert!(messages.iter().any(|message| matches!(
            message,
            ScanMessage::Finished(ScanStats {
                files: 2,
                bytes: 19,
                ..
            })
        )));
    }

    #[test]
    /// Confirms a pre-set cancellation flag stops before traversal begins.
    fn cancellation_stops_before_scanning() {
        let temp = TestDirectory::new();
        fs::write(temp.path().join("ignored"), [1, 2, 3]).unwrap();
        let (sender, receiver) = mpsc::channel();
        scan_root(
            temp.path().to_path_buf(),
            sender,
            Arc::new(AtomicBool::new(true)),
        );

        assert!(matches!(
            receiver.recv().unwrap(),
            ScanMessage::Cancelled(_)
        ));
    }
}
