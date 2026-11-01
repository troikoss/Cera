// SPDX-License-Identifier: GPL-3.0-or-later
//! A folder's entries as a list model, kept current by a file monitor; the vault tree is made
//! of these.
//!
//! Not `GtkDirectoryList`: it handles monitor events strictly in order, and when a file is gone
//! before its info is read, that event never completes and the list stops updating for good
//! (GTK 4.22). Notes are written to a hidden temporary file that is renamed into place a few
//! milliseconds later (a new note, every compaction), which is exactly that case.

use std::path::Path;

use gtk::prelude::*;
use gtk::{gio, glib};

/// What the tree needs to know about an entry.
pub const ATTRIBUTES: &str = "standard::name,standard::display-name,standard::type";

/// The entries of `folder` as `gio::FileInfo`s, each with its `GFile` in the
/// `standard::file` attribute (as `GtkDirectoryList` provides it). Filled asynchronously.
pub fn model(folder: &gio::File) -> gio::ListStore {
    let store = gio::ListStore::new::<gio::FileInfo>();
    match folder.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE) {
        Ok(monitor) => {
            let weak = store.downgrade();
            monitor.connect_changed(move |_, file, other, event| {
                if let Some(store) = weak.upgrade() {
                    changed(&store, file, other, event);
                }
            });
            // SAFETY: stored once under this key and never read back with another type; it
            // only ties the monitor's lifetime to the list's.
            unsafe { store.set_data("cera-folder-monitor", monitor) };
        }
        Err(e) => tracing::warn!(folder = %folder.uri(), "cannot watch folder: {e}"),
    }

    let (folder, weak) = (folder.clone(), store.downgrade());
    glib::spawn_future_local(async move {
        let enumerated = folder
            .enumerate_children_future(
                ATTRIBUTES,
                gio::FileQueryInfoFlags::NONE,
                glib::Priority::DEFAULT,
            )
            .await;
        let children = match enumerated {
            Ok(children) => children,
            Err(e) => {
                tracing::warn!(folder = %folder.uri(), "cannot list folder: {e}");
                return;
            }
        };
        while let Ok(infos) = children
            .next_files_future(64, glib::Priority::DEFAULT)
            .await
        {
            let Some(store) = weak.upgrade() else { return };
            if infos.is_empty() {
                break;
            }
            for info in infos {
                let file = folder.child(info.name());
                upsert(&store, &file, info);
            }
        }
    });
    store
}

fn changed(
    store: &gio::ListStore,
    file: &gio::File,
    other: Option<&gio::File>,
    event: gio::FileMonitorEvent,
) {
    use gio::FileMonitorEvent as E;
    match event {
        E::Created | E::MovedIn | E::AttributeChanged => refresh(store, file),
        E::Deleted | E::MovedOut => remove(store, file),
        E::Renamed => {
            remove(store, file);
            if let Some(other) = other {
                refresh(store, other);
            }
        }
        _ => {}
    }
}

/// Reads the entry again; a file that is already gone (a temporary one) is simply not shown.
fn refresh(store: &gio::ListStore, file: &gio::File) {
    match file.query_info(
        ATTRIBUTES,
        gio::FileQueryInfoFlags::NONE,
        gio::Cancellable::NONE,
    ) {
        Ok(info) => upsert(store, file, info),
        Err(_) => remove(store, file),
    }
}

fn position(store: &gio::ListStore, name: &Path) -> Option<u32> {
    (0..store.n_items()).find(|&i| {
        store
            .item(i)
            .and_downcast::<gio::FileInfo>()
            .is_some_and(|info| info.name() == name)
    })
}

fn upsert(store: &gio::ListStore, file: &gio::File, info: gio::FileInfo) {
    info.set_attribute_object("standard::file", file);
    match position(store, &info.name()) {
        Some(i) => store.splice(i, 1, &[info]),
        None => store.append(&info),
    }
}

fn remove(store: &gio::ListStore, file: &gio::File) {
    if let Some(i) = file.basename().and_then(|name| position(store, &name)) {
        store.remove(i);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn names(store: &gio::ListStore) -> Vec<String> {
        let mut v: Vec<String> = (0..store.n_items())
            .filter_map(|i| store.item(i).and_downcast::<gio::FileInfo>())
            .map(|info| info.name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// Runs the main context until `done` holds, or fails after a few seconds.
    fn wait_for(ctx: &glib::MainContext, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out");
            ctx.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn keeps_up_with_files_written_by_temp_and_rename() {
        let dir = std::env::temp_dir().join(format!("cera-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.cera"), b"").unwrap();
        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| {
            let store = model(&gio::File::for_path(&dir));
            wait_for(&ctx, || names(&store) == ["a.cera"]);

            // How a new note is saved: a hidden temporary file, renamed at once.
            for n in 0..3 {
                let tmp = dir.join(format!(".note {n}.cera.tmp"));
                std::fs::write(&tmp, b"x").unwrap();
                std::fs::rename(&tmp, dir.join(format!("note {n}.cera"))).unwrap();
            }
            wait_for(&ctx, || names(&store).len() == 4);
            assert_eq!(
                names(&store),
                ["a.cera", "note 0.cera", "note 1.cera", "note 2.cera"]
            );

            // And it still follows later changes.
            std::fs::remove_file(dir.join("a.cera")).unwrap();
            wait_for(&ctx, || names(&store).len() == 3);
            let info = store.item(0).and_downcast::<gio::FileInfo>().unwrap();
            let file = info
                .attribute_object("standard::file")
                .and_downcast::<gio::File>();
            assert!(file.is_some_and(|f| f.path().unwrap().starts_with(&dir)));
        })
        .unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
