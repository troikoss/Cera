// SPDX-License-Identifier: GPL-3.0-or-later
//! The vault (DESIGN.md §8): opening one, and the file operations behind the sidebar's
//! menus: new note and folder, rename, move, trash, drop and PDF import.

use std::path::{Path, PathBuf};

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_core::{Document, NoteFile};
use gtk::{gio, glib};

use super::{CeraWindow, display_name};
use crate::dialogs::{self, Folder, ImportRequest};
use crate::vault::{self, Vault};

impl CeraWindow {
    /// Opens the most recent vault, if there is one; for startup.
    pub fn restore_last_vault(&self) {
        if let Some(root) = vault::load_recent().vaults.first() {
            self.open_vault(&root.clone(), false);
        }
    }

    pub(super) fn open_vault_dialog(&self, create: bool) {
        let dialog = gtk::FileDialog::builder()
            .title(if create {
                "Choose a Folder for the New Vault"
            } else {
                "Open Folder as Vault"
            })
            .accept_label(if create { "_Create" } else { "_Open" })
            .build();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                // An error here means the dialog was cancelled.
                let Ok(folder) = dialog.select_folder_future(Some(&win)).await else {
                    return;
                };
                if let Some(path) = folder.path() {
                    win.open_vault(&path, create);
                }
            }
        ));
    }

    /// Opens (or with `create`, sets up) a vault, closing the previous one's tabs first.
    pub(super) fn open_vault(&self, root: &Path, create: bool) {
        let imp = self.imp();
        let opened = if create {
            Vault::create(root)
        } else {
            Vault::open(root)
        };
        let vault = match opened {
            Ok(vault) => vault,
            Err(e) => {
                self.toast(&format!("Cannot open “{}”: {e}", display_name(root)));
                return;
            }
        };
        if imp.vault.borrow().is_some() {
            self.save_workspace_now();
        }
        self.close_all_tabs();
        if let Err(e) = vault::remember_vault(&vault.root) {
            tracing::warn!("cannot update the recent vaults: {e}");
        }
        tracing::info!(root = %vault.root.display(), "opened vault");
        *imp.workspace.borrow_mut() = vault.load_workspace();
        imp.sidebar.set_vault(Some(&vault));
        *imp.vault.borrow_mut() = Some(vault);
        self.rebuild_recent();
        self.restore_tabs();
        self.update_stack();
        self.update_title();
    }

    /// The recent-vaults list on the welcome page and in the sidebar's vault menu.
    pub(super) fn rebuild_recent(&self) {
        let imp = self.imp();
        let recent = vault::load_recent().vaults;
        imp.sidebar.set_recent(&recent);
        let list = &imp.recent_list;
        while let Some(row) = list.first_child() {
            list.remove(&row);
        }
        for path in &recent {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&display_name(path)))
                .subtitle(glib::markup_escape_text(&path.display().to_string()))
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let path = path.clone();
            row.connect_activated(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |_| win.open_vault(&path, false)
            ));
            list.append(&row);
        }
        list.set_visible(!recent.is_empty());
    }

    fn vault_root(&self) -> Option<PathBuf> {
        self.imp().vault.borrow().as_ref().map(|v| v.root.clone())
    }

    fn in_vault(&self, path: &Path) -> bool {
        self.vault_root().is_some_and(|root| path.starts_with(root))
    }

    /// Every folder of the vault, the root shown by the vault's name.
    fn vault_folders(&self) -> Vec<Folder> {
        let vault = self.imp().vault.borrow();
        let Some(vault) = vault.as_ref() else {
            return Vec::new();
        };
        vault
            .folders()
            .into_iter()
            .map(|path| Folder {
                label: vault
                    .relative(&path)
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| vault.name()),
                path,
            })
            .collect()
    }

    fn target_folder(&self, folder: Option<PathBuf>) -> Option<PathBuf> {
        folder.or_else(|| self.imp().sidebar.current_folder())
    }

    pub(super) fn new_note(&self, folder: Option<PathBuf>) {
        let Some(folder) = self.target_folder(folder) else {
            return;
        };
        dialogs::ask_name(
            self,
            "New Note",
            "_Create",
            "Untitled",
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |name| win.create_note(&folder, &name)
            ),
        );
    }

    fn create_note(&self, folder: &Path, name: &str) {
        let path = vault::unique_path(folder, name, "cera");
        let spec = self
            .imp()
            .vault
            .borrow()
            .as_ref()
            .map(|v| v.config.new_note.page_spec())
            .unwrap_or_else(|| vault::NewNote::default().page_spec());
        let mut doc = Document::new([spec]);
        match NoteFile::create(&path, &mut doc, &[]) {
            Ok(file) => {
                self.add_tab(file, doc, None, true);
                self.hide_narrow_sidebar();
            }
            Err(e) => self.toast(&format!("Cannot create “{name}”: {e}")),
        }
    }

    pub(super) fn new_folder(&self, folder: Option<PathBuf>) {
        let Some(folder) = self.target_folder(folder) else {
            return;
        };
        dialogs::ask_name(
            self,
            "New Folder",
            "_Create",
            "New Folder",
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |name| {
                    let path = vault::unique_path(&folder, &name, "");
                    if let Err(e) = std::fs::create_dir(&path) {
                        win.toast(&format!("Cannot create “{name}”: {e}"));
                    }
                }
            ),
        );
    }

    /// A row was activated: open a note, or offer to convert a PDF.
    pub(super) fn open_file(&self, path: PathBuf) {
        if is_pdf(&path) {
            self.start_import(path, None);
            return;
        }
        let page = self.last_page(&path);
        if self.open_note(&path, page, true) {
            self.hide_narrow_sidebar();
        }
    }

    /// From the command line or the Open dialog. A PDF with a note of the same name next to
    /// it opens that note; otherwise it goes through the import dialog.
    pub fn open_path(&self, path: PathBuf) {
        if is_pdf(&path) {
            let note = path.with_extension("cera");
            if note.exists() {
                self.open_note(&note, None, true);
            } else {
                self.start_import(path, None);
            }
        } else {
            let page = self.last_page(&path);
            self.open_note(&path, page, true);
        }
    }

    /// When the sidebar covers the note (narrow window), get it out of the way.
    fn hide_narrow_sidebar(&self) {
        let split = &self.imp().split_view;
        if split.is_collapsed() {
            split.set_show_sidebar(false);
        }
    }

    /// Renames inside the same folder. Notes keep their `.cera` extension, PDFs their `.pdf`.
    pub(super) fn rename_file(&self, path: PathBuf, name: String) {
        let Some(parent) = path.parent() else { return };
        let file_name = if path.is_dir() {
            name.clone()
        } else {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            if ext.is_empty()
                || name
                    .to_lowercase()
                    .ends_with(&format!(".{}", ext.to_lowercase()))
            {
                name.clone()
            } else {
                format!("{name}.{ext}")
            }
        };
        if file_name.contains('/') || file_name.starts_with('.') {
            self.toast(&format!("“{name}” is not a valid name"));
            return;
        }
        self.move_path(&path, &parent.join(file_name));
    }

    pub(super) fn move_file(&self, path: PathBuf) {
        let parent = path.parent().map(Path::to_owned);
        let folders: Vec<Folder> = self
            .vault_folders()
            .into_iter()
            .filter(|f| !f.path.starts_with(&path) && Some(&f.path) != parent.as_ref())
            .collect();
        if folders.is_empty() {
            self.toast("There is no other folder to move to");
            return;
        }
        dialogs::choose_folder(
            self,
            &format!("Move “{}”", display_name(&path)),
            folders,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |folder| win.move_into(&path, &folder)
            ),
        );
    }

    fn move_into(&self, path: &Path, folder: &Path) {
        let Some(name) = path.file_name() else { return };
        if path.parent() == Some(folder) {
            return;
        }
        if folder.starts_with(path) {
            self.toast("A folder cannot be moved into itself");
            return;
        }
        self.move_path(path, &folder.join(name));
    }

    /// Renames or moves within the vault without replacing anything; open notes follow.
    fn move_path(&self, from: &Path, to: &Path) {
        if from == to {
            return;
        }
        // Case-only renames on case-insensitive file systems look like "exists".
        let same_file = to.exists()
            && from
                .canonicalize()
                .ok()
                .zip(to.canonicalize().ok())
                .is_some_and(|(a, b)| a == b);
        if to.exists() && !same_file {
            self.toast(&format!("“{}” already exists", display_name(to)));
            return;
        }
        if let Some(note) = self
            .canvases()
            .iter()
            .filter_map(|c| c.note())
            .find(|n| n.borrow().file.path() == from)
        {
            self.save_note(&note);
        }
        match std::fs::rename(from, to) {
            Ok(()) => self.moved(from, to),
            Err(e) => self.toast(&format!("Cannot move “{}”: {e}", display_name(from))),
        }
    }

    /// Moves a file or folder to the desktop trash, closing its notes first.
    pub(super) fn trash_file(&self, path: PathBuf) {
        self.close_tabs_under(&path);
        let name = display_name(&path);
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                let file = gio::File::for_path(&path);
                match file.trash_future(glib::Priority::DEFAULT).await {
                    Ok(()) => win.toast(&format!("Moved “{name}” to the trash")),
                    Err(e) => win.toast(&format!("Cannot move “{name}” to the trash: {e}")),
                }
            }
        ));
    }

    /// A file dropped on `folder`: vault files move there, PDFs are imported, outside notes
    /// are copied in.
    pub(super) fn drop_file(&self, path: PathBuf, folder: PathBuf) {
        if self.in_vault(&path) {
            self.move_into(&path, &folder);
        } else if is_pdf(&path) {
            self.start_import(path, Some(folder));
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("cera"))
        {
            let stem = path
                .file_stem()
                .map_or_else(|| "Note".into(), |s| s.to_string_lossy().into_owned());
            let dest = vault::unique_path(&folder, &stem, "cera");
            match std::fs::copy(&path, &dest) {
                Ok(_) => self.toast(&format!("Copied “{}” into the vault", display_name(&dest))),
                Err(e) => self.toast(&format!("Cannot copy “{}”: {e}", display_name(&path))),
            }
        } else {
            self.toast(&format!("“{}” is not a PDF or a note", display_name(&path)));
        }
    }

    pub(super) fn import_pdf_dialog(&self, folder: Option<PathBuf>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("PDF documents"));
        filter.add_suffix("pdf");
        filter.add_mime_type("application/pdf");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Import PDF")
            .filters(&filters)
            .default_filter(&filter)
            .build();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                let Ok(file) = dialog.open_future(Some(&win)).await else {
                    return;
                };
                if let Some(path) = file.path() {
                    win.start_import(path, folder);
                }
            }
        ));
    }

    /// Reads the PDF's page count, then shows the import dialog. Without a vault the note
    /// goes next to the PDF.
    pub(super) fn start_import(&self, pdf: PathBuf, folder: Option<PathBuf>) {
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                let probe = pdf.clone();
                let inspected = gio::spawn_blocking(move || {
                    let data = cera_core::MappedBytes::open(&probe)?;
                    cera_render::pdf::inspect(data).map_err(std::io::Error::other)
                })
                .await;
                let pages = match inspected {
                    Ok(Ok(info)) => info.pages.len() as u32,
                    Ok(Err(e)) => {
                        win.toast(&format!("Cannot read “{}”: {e}", display_name(&pdf)));
                        return;
                    }
                    Err(_) => return,
                };
                let pdf_dir = pdf.parent().map(Path::to_owned).unwrap_or_default();
                let mut folders = win.vault_folders();
                let default = folder
                    .or_else(|| win.in_vault(&pdf).then(|| pdf_dir.clone()))
                    .or_else(|| win.imp().sidebar.current_folder())
                    .unwrap_or_else(|| pdf_dir.clone());
                if folders.is_empty() {
                    folders.push(Folder {
                        label: display_name(&pdf_dir),
                        path: pdf_dir,
                    });
                }
                dialogs::import_pdf(
                    &win,
                    pdf,
                    pages,
                    folders,
                    &default,
                    glib::clone!(
                        #[weak]
                        win,
                        move |request| win.run_import(request)
                    ),
                );
            }
        ));
    }

    fn run_import(&self, request: ImportRequest) {
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                let ImportRequest {
                    pdf,
                    out,
                    pages,
                    keep_original,
                } = request;
                let (src, dest) = (pdf.clone(), out.clone());
                let imported = gio::spawn_blocking(move || {
                    cera_render::import::import_pdf(&src, &dest, Some(pages))
                })
                .await;
                match imported {
                    Ok(Ok((file, doc))) => {
                        let count = doc.page_count();
                        win.add_tab(file, doc, None, true);
                        win.hide_narrow_sidebar();
                        let pages = if count == 1 { "page" } else { "pages" };
                        win.toast(&format!("Imported {count} {pages}"));
                        if !keep_original {
                            let file = gio::File::for_path(&pdf);
                            if let Err(e) = file.trash_future(glib::Priority::DEFAULT).await {
                                win.toast(&format!("Cannot move the PDF to the trash: {e}"));
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(pdf = %pdf.display(), "import failed: {e}");
                        win.toast(&format!("Cannot import “{}”: {e}", display_name(&pdf)));
                    }
                    Err(_) => win.toast("The import crashed"),
                }
            }
        ));
    }
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}
