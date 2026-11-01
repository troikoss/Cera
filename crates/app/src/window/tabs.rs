// SPDX-License-Identifier: GPL-3.0-or-later
//! Notes in tabs (DESIGN.md §9). Each tab is a scrolled window around a canvas showing one
//! note. Only the selected tab renders; hidden ones drop their tiles. Every note autosaves
//! 2 s after its last edit, and when its tab closes.

use std::path::Path;
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_core::{Document, NoteFile};
use gtk::{gio, glib};

use super::{CeraWindow, display_name};
use crate::canvas::Canvas;
use crate::note::{Note, SharedNote};

/// Save this long after the last edit (DESIGN.md §7), so a crash loses at most that much.
const AUTOSAVE_DELAY: Duration = Duration::from_secs(2);
/// Workspace (open tabs, last pages) is written this long after it last changed.
const WORKSPACE_DELAY: Duration = Duration::from_secs(1);

impl CeraWindow {
    pub(super) fn setup_tabs(&self) {
        let imp = self.imp();
        imp.tab_view.connect_selected_page_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |view| {
                let selected = view.selected_page();
                for i in 0..view.n_pages() {
                    let page = view.nth_page(i);
                    if let Some(canvas) = canvas_of(&page) {
                        canvas.set_active(selected.as_ref() == Some(&page));
                    }
                }
                win.update_title();
                win.update_note_actions();
                win.update_toolbar();
                win.schedule_workspace_save();
            }
        ));
        imp.tab_view.connect_close_page(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, page| {
                win.release_tab(page);
                glib::Propagation::Proceed
            }
        ));
        imp.tab_view.connect_page_detached(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _, _| {
                win.update_stack();
                win.update_title();
                win.update_note_actions();
                win.schedule_workspace_save();
            }
        ));
    }

    /// The canvas of the selected tab.
    pub(super) fn canvas(&self) -> Option<Canvas> {
        self.imp()
            .tab_view
            .selected_page()
            .and_then(|p| canvas_of(&p))
    }

    pub(super) fn canvases(&self) -> Vec<Canvas> {
        let view = &self.imp().tab_view;
        (0..view.n_pages())
            .filter_map(|i| canvas_of(&view.nth_page(i)))
            .collect()
    }

    fn tab_for(&self, path: &Path) -> Option<adw::TabPage> {
        let view = &self.imp().tab_view;
        (0..view.n_pages()).map(|i| view.nth_page(i)).find(|page| {
            canvas_of(page)
                .and_then(|c| c.note())
                .is_some_and(|n| n.borrow().file.path() == path)
        })
    }

    /// Opens a note in a tab, or selects its tab if it is open. `page` is where to scroll to.
    pub(super) fn open_note(&self, path: &Path, page: Option<usize>, select: bool) -> bool {
        if let Some(tab) = self.tab_for(path) {
            if select {
                self.imp().tab_view.set_selected_page(&tab);
            }
            return true;
        }
        match NoteFile::open(path) {
            Ok((file, doc)) => {
                if let Some(bytes) = file.recovered_bytes() {
                    tracing::warn!(path = %path.display(), bytes, "ignored an incomplete save");
                }
                self.add_tab(file, doc, page, select);
                true
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "cannot open: {e}");
                self.toast(&format!("Cannot open “{}”: {e}", display_name(path)));
                false
            }
        }
    }

    /// Shows a note in a new tab and returns its canvas.
    pub(super) fn add_tab(
        &self,
        file: NoteFile,
        doc: Document,
        page: Option<usize>,
        select: bool,
    ) -> Canvas {
        let imp = self.imp();
        let id = imp.next_doc.get();
        imp.next_doc.set(id + 1);
        tracing::info!(path = %file.path().display(), pages = doc.page_count(), "opened note");
        let note = Note::new(id, file, doc);
        let name = note.borrow().name();
        let tooltip = note.borrow().file.path().display().to_string();

        let canvas = Canvas::default();
        canvas.set_renderer(self.renderer());
        canvas.set_tools(imp.tool_state.clone());
        canvas.set_active(false);
        canvas.set_tile_budget((crate::settings::with(|s| s.tile_budget_mib) as usize) << 20);
        canvas.set_note(Some(note));
        if let Some(page) = page {
            canvas.show_page(page);
        }
        let weak = canvas.downgrade();
        imp.sinks.borrow_mut().insert(
            id,
            std::rc::Rc::new(move |result| {
                if let Some(canvas) = weak.upgrade() {
                    canvas.deliver(result);
                }
            }),
        );
        canvas.connect_local(
            "changed",
            false,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                #[weak]
                canvas,
                #[upgrade_or_default]
                move |_| {
                    win.edited(&canvas);
                    None
                }
            ),
        );
        canvas.connect_local(
            "selection-changed",
            false,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                #[weak]
                canvas,
                #[upgrade_or_default]
                move |_| {
                    if win.canvas().as_ref() == Some(&canvas) {
                        win.update_note_actions();
                        win.update_toolbar();
                    }
                    None
                }
            ),
        );
        canvas.connect_local(
            "context-menu",
            false,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                #[weak]
                canvas,
                #[upgrade_or_default]
                move |args| {
                    let v = |i: usize| args[i].get::<f64>().unwrap_or_default();
                    let source = args[3].get::<u32>().unwrap_or_default();
                    let point = args[4].get::<bool>().unwrap_or_default();
                    win.show_context_menu(
                        &canvas,
                        (v(1), v(2)),
                        crate::canvas::InputSource::from_u32(source),
                        point,
                    );
                    None
                }
            ),
        );
        for property in ["current-page", "page-count"] {
            canvas.connect_notify_local(
                Some(property),
                glib::clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |canvas, _| {
                        if win.canvas().as_ref() == Some(canvas) {
                            win.update_title();
                        }
                        win.schedule_workspace_save();
                    }
                ),
            );
        }

        let scroller = gtk::ScrolledWindow::builder().child(&canvas).build();
        let tab = imp.tab_view.append(&scroller);
        tab.set_title(&name);
        tab.set_tooltip(&glib::markup_escape_text(&tooltip));
        if select || imp.tab_view.n_pages() == 1 {
            imp.tab_view.set_selected_page(&tab);
        }
        self.update_stack();
        self.schedule_workspace_save();
        if std::env::var_os("CERA_DEBUG_DEMO").is_some() {
            // Full screen for comparable frame times.
            self.maximize();
            let weak = canvas.downgrade();
            glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                if let Some(canvas) = weak.upgrade() {
                    canvas.run_demo();
                }
            });
        }
        canvas
    }

    /// Before a tab closes: save (and compact if due), remember its page, release its tiles.
    fn release_tab(&self, page: &adw::TabPage) {
        let imp = self.imp();
        let Some(canvas) = canvas_of(page) else {
            return;
        };
        let Some(note) = canvas.note() else { return };
        self.remember_page(&canvas);
        let id = note.borrow().id;
        if let Some(source) = imp.autosave.borrow_mut().remove(&id) {
            source.remove();
        }
        self.save_note(&note);
        {
            let mut n = note.borrow_mut();
            if n.file.should_compact() {
                let Note {
                    file, doc, history, ..
                } = &mut *n;
                let started = Instant::now();
                match file.compact(doc, history.referenced_assets()) {
                    Ok(()) => tracing::info!(
                        ms = started.elapsed().as_secs_f64() * 1e3,
                        "compacted on close"
                    ),
                    Err(e) => tracing::warn!("cannot compact: {e}"),
                }
            }
        }
        imp.sinks.borrow_mut().remove(&id);
        canvas.set_note(None);
    }

    /// Closes the tabs whose notes are `path` or inside it, saving them first.
    pub(super) fn close_tabs_under(&self, path: &Path) {
        let view = &self.imp().tab_view;
        let pages: Vec<_> = (0..view.n_pages())
            .map(|i| view.nth_page(i))
            .filter(|page| {
                canvas_of(page)
                    .and_then(|c| c.note())
                    .is_some_and(|n| n.borrow().file.path().starts_with(path))
            })
            .collect();
        for page in pages {
            view.close_page(&page);
        }
    }

    pub(super) fn close_all_tabs(&self) {
        let view = &self.imp().tab_view;
        while view.n_pages() > 0 {
            view.close_page(&view.nth_page(view.n_pages() - 1));
        }
    }

    pub(super) fn close_current_tab(&self) {
        let view = &self.imp().tab_view;
        if let Some(page) = view.selected_page() {
            view.close_page(&page);
        }
    }

    /// After an edit: refresh the undo buttons and save the note once editing pauses.
    fn edited(&self, canvas: &Canvas) {
        if self.canvas().as_ref() == Some(canvas) {
            self.update_note_actions();
        }
        let Some(note) = canvas.note() else { return };
        let id = note.borrow().id;
        let imp = self.imp();
        if let Some(source) = imp.autosave.borrow_mut().remove(&id) {
            source.remove();
        }
        let weak = self.downgrade();
        let note = std::rc::Rc::downgrade(&note);
        let source = glib::timeout_add_local_once(AUTOSAVE_DELAY, move || {
            let (Some(win), Some(note)) = (weak.upgrade(), note.upgrade()) else {
                return;
            };
            win.imp().autosave.borrow_mut().remove(&id);
            win.save_note(&note);
        });
        imp.autosave.borrow_mut().insert(id, source);
    }

    /// Saves a note if it has unsaved changes; failures become a toast.
    pub(super) fn save_note(&self, note: &SharedNote) {
        let started = Instant::now();
        let result = note.borrow_mut().save();
        match result {
            Ok(true) => tracing::debug!(ms = started.elapsed().as_secs_f64() * 1e3, "saved"),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("cannot save: {e}");
                self.toast(&format!("Cannot save “{}”: {e}", note.borrow().name()));
            }
        }
    }

    pub(super) fn save_all(&self) {
        for (_, source) in self.imp().autosave.borrow_mut().drain() {
            source.remove();
        }
        for canvas in self.canvases() {
            if let Some(note) = canvas.note() {
                self.save_note(&note);
            }
        }
    }

    pub(super) fn update_note_actions(&self) {
        let note = self.canvas().and_then(|c| c.note());
        let (can_undo, can_redo) = note.as_ref().map_or((false, false), |n| {
            let n = n.borrow();
            (n.history.can_undo(), n.history.can_redo())
        });
        let has_note = note.is_some();
        let selection = self.canvas().is_some_and(|c| c.has_selection());
        for (name, enabled) in [
            ("undo", can_undo),
            ("redo", can_redo),
            ("zoom-in", has_note),
            ("zoom-out", has_note),
            ("zoom-fit", has_note),
            ("close-tab", has_note),
            ("pages", has_note),
            ("export-pdf", has_note),
            ("export-png", has_note),
            ("add-page", has_note),
            ("cut", selection),
            ("copy", selection),
            ("delete-selection", selection),
            ("select-none", selection),
            ("arrange", selection),
            ("change-style", selection),
            ("paste", has_note && crate::canvas::has_clipboard()),
            ("paste-at", has_note && crate::canvas::has_clipboard()),
        ] {
            self.set_action_enabled(name, enabled);
        }
        self.set_action_enabled("bookmark", has_note);
        self.set_action_enabled("go-to-page", has_note);
        self.sync_bookmark();
    }

    pub(super) fn set_action_enabled(&self, name: &str, enabled: bool) {
        if let Some(action) = self.lookup_action(name).and_downcast::<gio::SimpleAction>() {
            action.set_enabled(enabled);
        }
    }

    /// Header title: the note's name and "Page 2 of 18"; the vault's name without a note.
    pub(super) fn update_title(&self) {
        let imp = self.imp();
        let (title, subtitle) = match self.canvas() {
            Some(canvas) => {
                let name = canvas.note().map(|n| n.borrow().name()).unwrap_or_default();
                let count = canvas.page_count();
                let subtitle = if count > 0 {
                    format!("Page {} of {count}", canvas.current_page() + 1)
                } else {
                    String::new()
                };
                (name, subtitle)
            }
            None => (
                imp.vault
                    .borrow()
                    .as_ref()
                    .map_or_else(|| "Cera".into(), |v| v.name()),
                String::new(),
            ),
        };
        imp.title.set_title(&title);
        imp.title.set_subtitle(&subtitle);
        self.sync_bookmark();
        self.set_title(Some(&title));
    }

    /// Notes this tab's page in the workspace.
    fn remember_page(&self, canvas: &Canvas) {
        let imp = self.imp();
        let Some(note) = canvas.note() else { return };
        let vault = imp.vault.borrow();
        let Some(rel) = vault
            .as_ref()
            .and_then(|v| v.relative(note.borrow().file.path()))
        else {
            return;
        };
        imp.workspace
            .borrow_mut()
            .last_page
            .insert(rel, canvas.current_page());
    }

    pub(super) fn schedule_workspace_save(&self) {
        let imp = self.imp();
        if imp.restoring.get() || imp.vault.borrow().is_none() {
            return;
        }
        if let Some(source) = imp.workspace_save.take() {
            source.remove();
        }
        let weak = self.downgrade();
        let source = glib::timeout_add_local_once(WORKSPACE_DELAY, move || {
            if let Some(win) = weak.upgrade() {
                win.imp().workspace_save.take();
                win.save_workspace_now();
            }
        });
        *imp.workspace_save.borrow_mut() = Some(source);
    }

    /// Writes the open tabs, the selected one and every tab's page to `.cera/workspace.json`.
    pub(super) fn save_workspace_now(&self) {
        let imp = self.imp();
        if let Some(source) = imp.workspace_save.take() {
            source.remove();
        }
        if imp.restoring.get() {
            return;
        }
        let canvases = self.canvases();
        for canvas in &canvases {
            self.remember_page(canvas);
        }
        let vault = imp.vault.borrow();
        let Some(vault) = vault.as_ref() else { return };
        let selected = self.canvas();
        let mut ws = imp.workspace.borrow_mut();
        ws.tabs.clear();
        ws.active = 0;
        for canvas in &canvases {
            let Some(rel) = canvas
                .note()
                .and_then(|n| vault.relative(n.borrow().file.path()))
            else {
                continue;
            };
            if selected.as_ref() == Some(canvas) {
                ws.active = ws.tabs.len();
            }
            ws.tabs.push(rel);
        }
        // Forget pages of notes that no longer exist.
        ws.last_page.retain(|rel, _| vault.absolute(rel).exists());
        if let Err(e) = vault.save_workspace(&ws) {
            tracing::warn!("cannot save the workspace: {e}");
        }
    }

    /// Reopens the tabs a vault had open, each at its last page.
    pub(super) fn restore_tabs(&self) {
        let imp = self.imp();
        let ws = imp.workspace.borrow().clone();
        let Some(root) = imp.vault.borrow().as_ref().map(|v| v.root.clone()) else {
            return;
        };
        imp.restoring.set(true);
        let mut selected = None;
        for (i, rel) in ws.tabs.iter().enumerate() {
            let path = root.join(rel);
            if !path.exists() {
                continue;
            }
            let page = ws.last_page.get(rel).map(|&p| p as usize);
            if self.open_note(&path, page, false) && i == ws.active {
                selected = self.tab_for(&path);
            }
        }
        if let Some(tab) = selected {
            imp.tab_view.set_selected_page(&tab);
        }
        imp.restoring.set(false);
    }

    /// The page a note was last left at, if this vault remembers it.
    pub(super) fn last_page(&self, path: &Path) -> Option<usize> {
        let imp = self.imp();
        let rel = imp.vault.borrow().as_ref()?.relative(path)?;
        imp.workspace
            .borrow()
            .last_page
            .get(&rel)
            .map(|&p| p as usize)
    }

    /// After a rename or move of `old` (a note or a folder) to `new`: open notes follow.
    pub(super) fn moved(&self, old: &Path, new: &Path) {
        let imp = self.imp();
        let view = &imp.tab_view;
        for i in 0..view.n_pages() {
            let page = view.nth_page(i);
            let Some(note) = canvas_of(&page).and_then(|c| c.note()) else {
                continue;
            };
            let path = note.borrow().file.path().to_owned();
            let Ok(rest) = path.strip_prefix(old) else {
                continue;
            };
            let moved = if rest.as_os_str().is_empty() {
                new.to_owned()
            } else {
                new.join(rest)
            };
            note.borrow_mut().file.set_path(&moved);
            page.set_title(&note.borrow().name());
            page.set_tooltip(&glib::markup_escape_text(&moved.display().to_string()));
        }
        // Keep remembered pages under the new names.
        if let Some(vault) = imp.vault.borrow().as_ref()
            && let (Some(old_rel), Some(new_rel)) = (vault.relative(old), vault.relative(new))
        {
            let mut ws = imp.workspace.borrow_mut();
            let moved: Vec<(String, u32)> = ws
                .last_page
                .iter()
                .filter(|(k, _)| Path::new(k.as_str()).starts_with(&old_rel))
                .map(|(k, &v)| (k.clone(), v))
                .collect();
            for (k, v) in moved {
                ws.last_page.remove(&k);
                let rest = Path::new(&k)
                    .strip_prefix(&old_rel)
                    .unwrap_or(Path::new(""));
                let key = Path::new(&new_rel).join(rest);
                ws.last_page.insert(key.to_string_lossy().into_owned(), v);
            }
        }
        self.update_title();
        self.schedule_workspace_save();
    }
}

/// The canvas inside a tab.
pub(super) fn canvas_of(page: &adw::TabPage) -> Option<Canvas> {
    page.child()
        .downcast::<gtk::ScrolledWindow>()
        .ok()?
        .child()
        .and_downcast()
}
