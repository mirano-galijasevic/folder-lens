use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};

#[derive(Clone, Debug)]
pub struct ScanEntry {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanStats {
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub errors: u64,
}

#[derive(Debug)]
pub enum ScanMessage {
    Progress(ScanStats),
    Entry(ScanEntry),
    Finished(ScanStats),
    Cancelled(ScanStats),
    RootError(String),
}

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
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc;

    static TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("folder-lens-test-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
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
