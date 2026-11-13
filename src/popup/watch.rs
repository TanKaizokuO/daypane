//! Live reload: watch the SQLite file and its `-wal` so writes from the CLI, MCP server, and
//! daemon show up while the popup is open.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use notify::{EventKind, RecursiveMode, Watcher};

/// Bursts of file events (a transaction touches the db and the WAL several times) collapse into one reload.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// True for the database file itself and its `-wal` / `-journal` siblings; `-shm` is excluded
/// because it is memory-mapped and changes on every read transaction.
pub fn is_db_file(path: &std::path::Path, db_name: &str) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name == db_name
        || name
            .strip_prefix(db_name)
            .is_some_and(|rest| rest == "-wal" || rest == "-journal")
}

fn is_write(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Start watching `db_path`. Triggers `win.reload` on `window` (main thread) after the debounce.
/// The returned watcher must be kept alive.
pub fn watch(
    window: &gtk::ApplicationWindow,
    db_path: PathBuf,
) -> Result<notify::RecommendedWatcher> {
    let db_name = db_path
        .file_name()
        .and_then(|n| n.to_str())
        .context("database path has no file name")?
        .to_string();
    let dir = db_path
        .parent()
        .context("database path has no parent directory")?
        .to_path_buf();
    let weak: glib::SendWeakRef<gtk::ApplicationWindow> = window.downgrade().into();
    let pending = Arc::new(AtomicBool::new(false));

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if !is_write(&event.kind) || !event.paths.iter().any(|p| is_db_file(p, &db_name)) {
            return;
        }
        if pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = weak.clone();
        let pending = pending.clone();
        // Hop to the GTK main thread, then wait out the debounce window there.
        glib::MainContext::default().invoke(move || {
            glib::timeout_add_local_once(DEBOUNCE, move || {
                pending.store(false, Ordering::SeqCst);
                if let Some(window) = weak.upgrade() {
                    let _ = WidgetExt::activate_action(&window, "win.reload", None);
                }
            });
        });
    })?;
    // Watch the directory: the WAL file is created and removed as connections come and go.
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", dir.display()))?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn matches_db_and_wal_only() {
        let n = "app.db";
        assert!(is_db_file(Path::new("/d/app.db"), n));
        assert!(is_db_file(Path::new("/d/app.db-wal"), n));
        assert!(is_db_file(Path::new("/d/app.db-journal"), n));
        assert!(!is_db_file(Path::new("/d/app.db-shm"), n));
        assert!(!is_db_file(Path::new("/d/other.db"), n));
        assert!(!is_db_file(Path::new("/d/app.db.bak"), n));
    }

    #[test]
    fn only_content_changes_count() {
        use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};
        assert!(is_write(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_write(&EventKind::Create(CreateKind::File)));
        assert!(is_write(&EventKind::Remove(RemoveKind::File)));
        assert!(!is_write(&EventKind::Access(AccessKind::Any)));
    }
}
