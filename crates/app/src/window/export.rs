// SPDX-License-Identifier: GPL-3.0-or-later
//! Export of the selected note (DESIGN.md §7): the whole note as a vector PDF, or the
//! current page as a PNG. Rendering runs on a worker thread; ink is shared, not copied.

use std::path::PathBuf;

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_render::export::{self, ExportPage};
use gtk::{gio, glib};

use super::{CeraWindow, display_name};

/// PNG resolution: 3 pixels per point is 216 dpi, sharp on paper and screens.
const PNG_SCALE: f64 = 3.0;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Format {
    Pdf,
    Png,
}

impl CeraWindow {
    pub(super) fn export(&self, format: Format) {
        let Some(canvas) = self.canvas() else { return };
        let Some(note) = canvas.note() else { return };
        let (stem, folder) = {
            let n = note.borrow();
            (n.name(), n.file.path().parent().map(gio::File::for_path))
        };
        let (name, filter_name, suffix) = match format {
            Format::Pdf => (format!("{stem}.pdf"), "PDF documents", "pdf"),
            Format::Png => (
                format!("{stem} – page {}.png", canvas.current_page() + 1),
                "PNG images",
                "png",
            ),
        };
        let filter = gtk::FileFilter::new();
        filter.set_name(Some(filter_name));
        filter.add_suffix(suffix);
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title(match format {
                Format::Pdf => "Export as PDF",
                Format::Png => "Export Page as PNG",
            })
            .initial_name(name)
            .filters(&filters)
            .default_filter(&filter)
            .build();
        if let Some(folder) = folder {
            dialog.set_initial_folder(Some(&folder));
        }
        let page = canvas.current_page() as usize;
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                // An error here means the dialog was cancelled.
                let Ok(file) = dialog.save_future(Some(&win)).await else {
                    return;
                };
                let Some(out) = file.path() else { return };
                let collected = {
                    let mut n = note.borrow_mut();
                    let assets = export::note_assets(&n.file);
                    export::note_pages(&mut n.doc).map(|pages| (pages, assets))
                };
                let (pages, assets) = match collected {
                    Ok(c) => c,
                    Err(e) => {
                        win.toast(&format!("Cannot read the note: {e}"));
                        return;
                    }
                };
                win.run_export(format, out, pages, assets, page).await;
            }
        ));
    }

    async fn run_export(
        &self,
        format: Format,
        out: PathBuf,
        pages: Vec<ExportPage>,
        assets: std::collections::HashMap<cera_core::AssetId, cera_core::MappedBytes>,
        page: usize,
    ) {
        // A long note takes seconds (324 pages: 11 s on the tablet); say it's running.
        let busy = adw::Toast::builder()
            .title(format!("Exporting “{}”…", display_name(&out)))
            .timeout(0)
            .build();
        self.imp().toasts.add_toast(busy.clone());
        let dest = out.clone();
        let written = gio::spawn_blocking(move || match format {
            Format::Pdf => export::export_pdf(&dest, &pages, &assets),
            Format::Png => {
                let page = &pages[page.min(pages.len().saturating_sub(1))];
                export::export_png(&dest, page, &assets, PNG_SCALE)
            }
        })
        .await;
        busy.dismiss();
        match written {
            Ok(Ok(())) => {
                let toast = adw::Toast::builder()
                    .title(format!("Exported “{}”", display_name(&out)))
                    .button_label("_Open")
                    .build();
                toast.connect_button_clicked(glib::clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |_| {
                        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&out)));
                        launcher.launch(Some(&win), gio::Cancellable::NONE, |_| {});
                    }
                ));
                self.imp().toasts.add_toast(toast);
            }
            Ok(Err(e)) => {
                tracing::warn!(out = %out.display(), "export failed: {e}");
                self.toast(&format!("Cannot export: {e}"));
            }
            Err(_) => self.toast("The export crashed"),
        }
    }
}
