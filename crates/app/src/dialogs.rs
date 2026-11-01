// SPDX-License-Identifier: GPL-3.0-or-later
//! Small dialogs: importing a PDF, picking a folder, asking for a name.

use std::cell::RefCell;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// What the import dialog collected.
pub struct ImportRequest {
    pub pdf: PathBuf,
    /// The note to write.
    pub out: PathBuf,
    /// 0-based, inclusive.
    pub pages: RangeInclusive<u32>,
    pub keep_original: bool,
}

/// A folder to offer: its path and how to show it.
pub struct Folder {
    pub path: PathBuf,
    pub label: String,
}

/// Import dialog (DESIGN.md §9): name, folder, page range and whether to keep the PDF.
pub fn import_pdf(
    parent: &impl IsA<gtk::Widget>,
    pdf: PathBuf,
    page_count: u32,
    folders: Vec<Folder>,
    default_folder: &Path,
    on_import: impl Fn(ImportRequest) + 'static,
) {
    let stem = pdf
        .file_stem()
        .map_or_else(|| "Imported".into(), |s| s.to_string_lossy().into_owned());
    let name = adw::EntryRow::builder().title("Name").text(&stem).build();
    let labels: Vec<&str> = folders.iter().map(|f| f.label.as_str()).collect();
    let folder = adw::ComboRow::builder()
        .title("Folder")
        .model(&gtk::StringList::new(&labels))
        .selected(
            folders
                .iter()
                .position(|f| f.path == default_folder)
                .unwrap_or(0) as u32,
        )
        .build();
    let count = f64::from(page_count.max(1));
    let from = adw::SpinRow::builder()
        .title("From page")
        .adjustment(&gtk::Adjustment::new(1.0, 1.0, count, 1.0, 10.0, 0.0))
        .build();
    let to = adw::SpinRow::builder()
        .title("To page")
        .adjustment(&gtk::Adjustment::new(count, 1.0, count, 1.0, 10.0, 0.0))
        .build();
    let keep = adw::SwitchRow::builder()
        .title("Keep original PDF")
        .subtitle("Otherwise it is moved to the trash")
        .active(true)
        .build();
    let group = adw::PreferencesGroup::builder()
        .description(format!("{page_count} pages"))
        .build();
    group.add(&name);
    group.add(&folder);
    let range = adw::PreferencesGroup::builder().title("Pages").build();
    range.add(&from);
    range.add(&to);
    let options = adw::PreferencesGroup::new();
    options.add(&keep);
    let page = adw::PreferencesPage::new();
    page.add(&group);
    page.add(&range);
    page.add(&options);

    let import = gtk::Button::builder()
        .label("_Import")
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    let cancel = gtk::Button::builder()
        .label("_Cancel")
        .use_underline(true)
        .build();
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&import);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&page));
    let dialog = adw::Dialog::builder()
        .title("Import PDF")
        .content_width(420)
        .child(&view)
        .default_widget(&import)
        .build();

    let folders = Rc::new(folders);
    let out_path = {
        let (name, folder, folders) = (name.clone(), folder.clone(), folders.clone());
        move || {
            let stem = name.text().trim().to_owned();
            let dir = &folders.get(folder.selected() as usize)?.path;
            (!stem.is_empty() && !stem.contains('/')).then(|| dir.join(format!("{stem}.cera")))
        }
    };
    let validate = {
        let (import, name, from, to, out_path) = (
            import.clone(),
            name.clone(),
            from.clone(),
            to.clone(),
            out_path.clone(),
        );
        move || {
            let out = out_path();
            let taken = out.as_ref().is_some_and(|p| p.exists());
            if taken || out.is_none() {
                name.add_css_class("error");
            } else {
                name.remove_css_class("error");
            }
            name.set_tooltip_text(taken.then_some("A note with this name exists"));
            import.set_sensitive(out.is_some() && !taken && from.value() <= to.value());
        }
    };
    validate();
    let validate = Rc::new(validate);
    name.connect_changed(glib::clone!(
        #[strong]
        validate,
        move |_| validate()
    ));
    folder.connect_selected_notify(glib::clone!(
        #[strong]
        validate,
        move |_| validate()
    ));
    from.connect_value_notify(glib::clone!(
        #[strong]
        validate,
        move |_| validate()
    ));
    to.connect_value_notify(glib::clone!(
        #[strong]
        validate,
        move |_| validate()
    ));

    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    let on_import = RefCell::new(Some(on_import));
    import.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            let Some(out) = out_path() else { return };
            let first = from.value() as u32 - 1;
            let last = (to.value() as u32).max(first + 1) - 1;
            if let Some(f) = on_import.borrow_mut().take() {
                f(ImportRequest {
                    pdf: pdf.clone(),
                    out,
                    pages: first..=last,
                    keep_original: keep.is_active(),
                });
            }
            dialog.close();
        }
    ));
    dialog.present(Some(parent));
}

/// A list of folders to pick from; `on_pick` gets the chosen one.
pub fn choose_folder(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    folders: Vec<Folder>,
    on_pick: impl Fn(PathBuf) + 'static,
) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .valign(gtk::Align::Start)
        .build();
    for folder in &folders {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&folder.label))
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
        list.append(&row);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(480)
        .child(&list)
        .build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&scroller));
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(360)
        .child(&view)
        .build();
    list.connect_row_activated(glib::clone!(
        #[weak]
        dialog,
        move |_, row| {
            if let Some(folder) = folders.get(row.index() as usize) {
                on_pick(folder.path.clone());
            }
            dialog.close();
        }
    ));
    dialog.present(Some(parent));
}

/// Asks for a name; `on_name` gets it, trimmed and non-empty.
pub fn ask_name(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    action: &str,
    initial: &str,
    on_name: impl Fn(String) + 'static,
) {
    let entry = gtk::Entry::builder()
        .text(initial)
        .activates_default(true)
        .build();
    let dialog = adw::AlertDialog::new(Some(heading), None);
    dialog.add_responses(&[("cancel", "_Cancel"), ("ok", action)]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.set_extra_child(Some(&entry));
    entry.connect_changed(glib::clone!(
        #[weak]
        dialog,
        move |e| {
            let name = e.text();
            let name = name.trim();
            dialog.set_response_enabled("ok", !name.is_empty() && !name.contains('/'));
        }
    ));
    dialog.connect_response(
        Some("ok"),
        glib::clone!(
            #[weak]
            entry,
            move |_, _| on_name(entry.text().trim().to_owned())
        ),
    );
    dialog.present(Some(parent));
    entry.grab_focus();
    entry.select_region(0, -1);
}
