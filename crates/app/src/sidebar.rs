// SPDX-License-Identifier: GPL-3.0-or-later
//! The vault sidebar (DESIGN.md §8): the folder tree, filename search and file actions.
//!
//! The tree is a monitored list per folder (`folder.rs`) → `FilterListModel` (folders, notes
//! and PDFs, no dotfiles) → `SortListModel` (folders first, natural order) → `TreeListModel`. Rows talk to the window through its `win.*` actions with file paths as
//! targets, so the sidebar holds no file-handling code of its own.

use std::cell::{Cell, OnceCell, RefCell};
use std::path::{Path, PathBuf};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use crate::vault::{self, Vault};

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/troikoss/Cera/sidebar.ui")]
    pub struct Sidebar {
        #[template_child]
        pub vault_button: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub new_button: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub search_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub search_bar: TemplateChild<gtk::SearchBar>,
        #[template_child]
        pub search_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub tree_scroller: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub tree_view: TemplateChild<gtk::ListView>,
        #[template_child]
        pub results_view: TemplateChild<gtk::ListView>,
        pub(super) vault: RefCell<Option<Vault>>,
        pub(super) results: OnceCell<gtk::StringList>,
        /// Folder of the row last activated: where "New note" goes.
        pub(super) current_folder: RefCell<Option<PathBuf>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Sidebar {
        const NAME: &'static str = "CeraSidebar";
        type Type = super::Sidebar;
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Sidebar {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_tree();
            obj.setup_search();
            obj.set_vault(None);
        }
    }

    impl WidgetImpl for Sidebar {}
    impl BinImpl for Sidebar {}
}

glib::wrapper! {
    pub struct Sidebar(ObjectSubclass<imp::Sidebar>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Sidebar {
    /// Shows the vault's tree, or the "No vault" page.
    pub fn set_vault(&self, vault: Option<&Vault>) {
        let imp = self.imp();
        *imp.vault.borrow_mut() = vault.cloned();
        imp.current_folder.take();
        imp.search_button.set_active(false);
        let open = vault.is_some();
        imp.new_button.set_sensitive(open);
        imp.search_button.set_sensitive(open);
        match vault {
            Some(vault) => {
                let root = gio::File::for_path(&vault.root);
                let tree = gtk::TreeListModel::new(folder_model(&root), false, false, |item| {
                    let info = item.downcast_ref::<gio::FileInfo>()?;
                    if info.file_type() != gio::FileType::Directory {
                        return None;
                    }
                    Some(folder_model(&info_file(info)?))
                });
                let selection = gtk::SingleSelection::new(Some(tree));
                selection.set_autoselect(false);
                selection.set_can_unselect(true);
                imp.tree_view.set_model(Some(&selection));
                imp.vault_button.set_label(&vault.name());
                imp.stack.set_visible_child_name("tree");
            }
            None => {
                imp.tree_view.set_model(None::<&gtk::SelectionModel>);
                imp.vault_button.set_label("Cera");
                imp.stack.set_visible_child_name("none");
            }
        }
    }

    /// The vault menu: recent vaults, then open and create.
    pub fn set_recent(&self, recent: &[PathBuf]) {
        let menu = gio::Menu::new();
        let vaults = gio::Menu::new();
        for path in recent {
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into(),
            );
            let item = gio::MenuItem::new(Some(&name), None);
            item.set_action_and_target_value(
                Some("win.open-vault-path"),
                Some(&path.to_string_lossy().to_variant()),
            );
            vaults.append_item(&item);
        }
        menu.append_section(None, &vaults);
        let actions = gio::Menu::new();
        actions.append(Some("_Open Folder…"), Some("win.open-vault"));
        actions.append(Some("_Create Vault…"), Some("win.create-vault"));
        menu.append_section(None, &actions);
        self.imp().vault_button.set_menu_model(Some(&menu));
    }

    /// Where a new note or folder goes: the folder last activated, or the vault root.
    pub fn current_folder(&self) -> Option<PathBuf> {
        let imp = self.imp();
        let root = imp.vault.borrow().as_ref()?.root.clone();
        Some(
            imp.current_folder
                .borrow()
                .clone()
                .filter(|f| f.starts_with(&root) && f.is_dir())
                .unwrap_or(root),
        )
    }

    /// Shows the search bar with the cursor in it.
    pub fn start_search(&self) {
        let imp = self.imp();
        if imp.vault.borrow().is_none() {
            return;
        }
        imp.search_button.set_active(true);
        imp.search_entry.grab_focus();
    }

    fn setup_tree(&self) {
        let imp = self.imp();
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            item.set_child(Some(&FileRow::default()));
        });
        factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let (Some(row), Some(tree_row)) = (
                item.child().and_downcast::<FileRow>(),
                item.item().and_downcast::<gtk::TreeListRow>(),
            ) else {
                return;
            };
            row.bind(&tree_row);
        });
        factory.connect_unbind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            if let Some(row) = item.child().and_downcast::<FileRow>() {
                row.unbind();
            }
        });
        imp.tree_view.set_factory(Some(&factory));
        imp.tree_view.connect_activate(glib::clone!(
            #[weak(rename_to = sidebar)]
            self,
            move |view, position| {
                let Some(row) = view
                    .model()
                    .and_then(|m| m.item(position))
                    .and_downcast::<gtk::TreeListRow>()
                else {
                    return;
                };
                let Some(info) = row.item().and_downcast::<gio::FileInfo>() else {
                    return;
                };
                let Some(path) = info_file(&info).and_then(|f| f.path()) else {
                    return;
                };
                if info.file_type() == gio::FileType::Directory {
                    row.set_expanded(!row.is_expanded());
                    *sidebar.imp().current_folder.borrow_mut() = Some(path);
                } else {
                    *sidebar.imp().current_folder.borrow_mut() = path.parent().map(Path::to_owned);
                    open_file(&sidebar, &path);
                }
            }
        ));

        // Drops on empty space go to the vault root.
        let target = drop_target();
        target.connect_drop(glib::clone!(
            #[weak(rename_to = sidebar)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                let root = sidebar
                    .imp()
                    .vault
                    .borrow()
                    .as_ref()
                    .map(|v| v.root.clone());
                root.is_some_and(|root| dropped(&sidebar, value, &root))
            }
        ));
        imp.tree_scroller.add_controller(target);
    }

    fn setup_search(&self) {
        let imp = self.imp();
        let results = gtk::StringList::new(&[]);
        let _ = imp.results.set(results.clone());
        imp.search_button
            .bind_property("active", &*imp.search_bar, "search-mode-enabled")
            .bidirectional()
            .sync_create()
            .build();
        imp.search_bar
            .connect_search_mode_enabled_notify(glib::clone!(
                #[weak(rename_to = sidebar)]
                self,
                move |bar| {
                    if !bar.is_search_mode() {
                        sidebar.imp().search_entry.set_text("");
                        sidebar.update_results();
                    }
                }
            ));
        imp.search_entry.connect_search_changed(glib::clone!(
            #[weak(rename_to = sidebar)]
            self,
            move |_| sidebar.update_results()
        ));
        imp.search_entry.connect_activate(glib::clone!(
            #[weak(rename_to = sidebar)]
            self,
            move |_| {
                if let Some(first) = sidebar.imp().results.get().and_then(|r| r.string(0)) {
                    open_file(&sidebar, Path::new(first.as_str()));
                }
            }
        ));

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let title = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let folder = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Start)
                .css_classes(["dim-label", "caption"])
                .build();
            let labels = gtk::Box::new(gtk::Orientation::Vertical, 0);
            labels.append(&title);
            labels.append(&folder);
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.append(&gtk::Image::new());
            row.append(&labels);
            item.set_child(Some(&row));
        });
        factory.connect_bind(glib::clone!(
            #[weak(rename_to = sidebar)]
            self,
            move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
                let (Some(row), Some(path)) = (
                    item.child(),
                    item.item()
                        .and_downcast::<gtk::StringObject>()
                        .map(|s| PathBuf::from(s.string().as_str())),
                ) else {
                    return;
                };
                let Some(icon) = row.first_child().and_downcast::<gtk::Image>() else {
                    return;
                };
                let labels = icon.next_sibling().expect("labels");
                let title = labels
                    .first_child()
                    .and_downcast::<gtk::Label>()
                    .expect("title");
                let folder = title
                    .next_sibling()
                    .and_downcast::<gtk::Label>()
                    .expect("folder");
                let kind = Kind::of(&path, false);
                icon.set_icon_name(Some(kind.icon()));
                title.set_label(&kind.display_name(&path));
                let rel = sidebar
                    .imp()
                    .vault
                    .borrow()
                    .as_ref()
                    .and_then(|v| v.relative(path.parent()?))
                    .unwrap_or_default();
                folder.set_label(&rel);
                folder.set_visible(!rel.is_empty());
            }
        ));
        imp.results_view.set_factory(Some(&factory));
        imp.results_view
            .set_model(Some(&gtk::NoSelection::new(Some(results))));
        imp.results_view.connect_activate(glib::clone!(
            #[weak(rename_to = sidebar)]
            self,
            move |_, position| {
                if let Some(path) = sidebar.imp().results.get().and_then(|r| r.string(position)) {
                    open_file(&sidebar, Path::new(path.as_str()));
                }
            }
        ));
    }

    fn update_results(&self) {
        let imp = self.imp();
        let query = imp.search_entry.text();
        let Some(results) = imp.results.get() else {
            return;
        };
        let has_vault = imp.vault.borrow().is_some();
        if query.trim().is_empty() || !has_vault {
            results.splice(0, results.n_items(), &[]);
            imp.stack
                .set_visible_child_name(if has_vault { "tree" } else { "none" });
            return;
        }
        let hits = imp
            .vault
            .borrow()
            .as_ref()
            .map(|v| v.search(query.trim()))
            .unwrap_or_default();
        let hits: Vec<String> = hits
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let hits: Vec<&str> = hits.iter().map(String::as_str).collect();
        results.splice(0, results.n_items(), &hits);
        imp.stack.set_visible_child_name(if hits.is_empty() {
            "no-results"
        } else {
            "results"
        });
    }
}

/// One folder's listing: filtered, sorted and kept up to date.
fn folder_model(folder: &gio::File) -> gio::ListModel {
    let list = crate::folder::model(folder);
    let filter = gtk::CustomFilter::new(|item| {
        item.downcast_ref::<gio::FileInfo>().is_some_and(|info| {
            vault::is_listed(
                &info.name().to_string_lossy(),
                info.file_type() == gio::FileType::Directory,
            )
        })
    });
    let filtered = gtk::FilterListModel::new(Some(list), Some(filter));
    let sorter = gtk::CustomSorter::new(|a, b| {
        let (Some(a), Some(b)) = (
            a.downcast_ref::<gio::FileInfo>(),
            b.downcast_ref::<gio::FileInfo>(),
        ) else {
            return gtk::Ordering::Equal;
        };
        let dir = |i: &gio::FileInfo| i.file_type() != gio::FileType::Directory;
        dir(a)
            .cmp(&dir(b))
            .then_with(|| vault::natural_cmp(&shown_name(a), &shown_name(b)))
            .into()
    });
    gtk::SortListModel::new(Some(filtered), Some(sorter)).upcast()
}

/// The name as the tree shows it, to sort by: notes without `.cera`, so "Untitled" comes
/// before "Untitled 2".
fn shown_name(info: &gio::FileInfo) -> String {
    let name = info.display_name();
    match name.len().checked_sub(5) {
        Some(stem)
            if info.file_type() != gio::FileType::Directory
                && name.is_char_boundary(stem)
                && name[stem..].eq_ignore_ascii_case(".cera") =>
        {
            name[..stem].to_owned()
        }
        _ => name.to_string(),
    }
}

/// `folder::model` puts each entry's `GFile` in the info.
fn info_file(info: &gio::FileInfo) -> Option<gio::File> {
    info.attribute_object("standard::file").and_downcast()
}

fn open_file(widget: &impl IsA<gtk::Widget>, path: &Path) {
    let _ = widget.activate_action("win.open-file", Some(&path.to_string_lossy().to_variant()));
}

fn drop_target() -> gtk::DropTarget {
    let target = gtk::DropTarget::new(
        glib::Type::INVALID,
        gdk::DragAction::MOVE | gdk::DragAction::COPY,
    );
    target.set_types(&[gdk::FileList::static_type(), gio::File::static_type()]);
    target
}

/// Files dropped on `folder`: the window moves vault files and imports outside ones.
fn dropped(widget: &impl IsA<gtk::Widget>, value: &glib::Value, folder: &Path) -> bool {
    let files = if let Ok(list) = value.get::<gdk::FileList>() {
        list.files()
    } else if let Ok(file) = value.get::<gio::File>() {
        vec![file]
    } else {
        return false;
    };
    let dest = folder.to_string_lossy();
    for path in files.iter().filter_map(|f| f.path()) {
        let target = (path.to_string_lossy().into_owned(), dest.to_string()).to_variant();
        let _ = widget.activate_action("win.drop-file", Some(&target));
    }
    true
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Kind {
    #[default]
    Folder,
    Note,
    Pdf,
}

impl Kind {
    fn of(path: &Path, is_dir: bool) -> Self {
        if is_dir {
            Kind::Folder
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
        {
            Kind::Pdf
        } else {
            Kind::Note
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Kind::Folder => "folder-symbolic",
            Kind::Note => "cera-note-symbolic",
            Kind::Pdf => "cera-pdf-symbolic",
        }
    }

    /// Notes show without their extension; PDFs keep it to tell them apart.
    fn display_name(self, path: &Path) -> String {
        let name = match self {
            Kind::Note => path.file_stem(),
            _ => path.file_name(),
        };
        name.map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    }
}

mod row_imp {
    use super::*;

    pub struct Widgets {
        pub expander: gtk::TreeExpander,
        pub icon: gtk::Image,
        pub stack: gtk::Stack,
        pub label: gtk::Label,
        pub entry: gtk::Entry,
    }

    #[derive(Default)]
    pub struct FileRow {
        pub(super) widgets: OnceCell<Widgets>,
        pub(super) path: RefCell<Option<PathBuf>>,
        pub(super) kind: Cell<Kind>,
        pub(super) drop: OnceCell<gtk::DropTarget>,
        pub(super) renaming: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FileRow {
        const NAME: &'static str = "CeraFileRow";
        type Type = super::FileRow;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for FileRow {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().build();
        }
    }

    impl WidgetImpl for FileRow {}
    impl BinImpl for FileRow {}
}

glib::wrapper! {
    /// A row of the vault tree: expander, icon and name, which turns into an entry to rename.
    pub struct FileRow(ObjectSubclass<row_imp::FileRow>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for FileRow {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl FileRow {
    fn widgets(&self) -> &row_imp::Widgets {
        self.imp().widgets.get().expect("built in constructed")
    }

    fn build(&self) {
        let imp = self.imp();
        let icon = gtk::Image::new();
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let entry = gtk::Entry::builder().hexpand(true).build();
        let stack = gtk::Stack::new();
        stack.add_named(&label, Some("label"));
        stack.add_named(&entry, Some("entry"));
        stack.set_hhomogeneous(false);
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.append(&icon);
        content.append(&stack);
        let expander = gtk::TreeExpander::new();
        expander.set_child(Some(&content));
        self.set_child(Some(&expander));
        let _ = imp.widgets.set(row_imp::Widgets {
            expander,
            icon,
            stack,
            label,
            entry: entry.clone(),
        });

        let actions = gio::SimpleActionGroup::new();
        let rename = gio::ActionEntry::builder("rename")
            .activate(glib::clone!(
                #[weak(rename_to = row)]
                self,
                // After the menu has closed and handed focus back, or it takes it from the entry.
                move |_: &gio::SimpleActionGroup, _, _| {
                    glib::idle_add_local_once(glib::clone!(
                        #[weak]
                        row,
                        move || row.start_rename()
                    ));
                }
            ))
            .build();
        actions.add_action_entries([rename]);
        self.insert_action_group("row", Some(&actions));

        let right_click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        right_click.connect_pressed(glib::clone!(
            #[weak(rename_to = row)]
            self,
            move |g, _, x, y| {
                g.set_state(gtk::EventSequenceState::Claimed);
                row.show_menu(x, y);
            }
        ));
        self.add_controller(right_click);
        let long_press = gtk::GestureLongPress::builder().touch_only(true).build();
        long_press.connect_pressed(glib::clone!(
            #[weak(rename_to = row)]
            self,
            move |g, x, y| {
                g.set_state(gtk::EventSequenceState::Claimed);
                row.show_menu(x, y);
            }
        ));
        self.add_controller(long_press);

        // Dragging moves files; touch drags are left to scrolling.
        let drag = gtk::DragSource::new();
        drag.set_actions(gdk::DragAction::MOVE);
        drag.connect_prepare(glib::clone!(
            #[weak(rename_to = row)]
            self,
            #[upgrade_or]
            None,
            move |source, _, _| {
                let touch = source
                    .current_event_device()
                    .is_some_and(|d| d.source() == gdk::InputSource::Touchscreen);
                if touch || row.imp().renaming.get() {
                    return None;
                }
                let path = row.imp().path.borrow().clone()?;
                let file = gio::File::for_path(path);
                Some(gdk::ContentProvider::for_value(&file.to_value()))
            }
        ));
        drag.connect_drag_begin(glib::clone!(
            #[weak(rename_to = row)]
            self,
            move |source, _| {
                let icon = gtk::WidgetPaintable::new(Some(&row.widgets().icon));
                source.set_icon(Some(&icon), 0, 0);
            }
        ));
        self.add_controller(drag);

        // Only folder rows take drops; on other rows the drop falls through to the list.
        let target = drop_target();
        target.connect_drop(glib::clone!(
            #[weak(rename_to = row)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                let path = row.imp().path.borrow().clone();
                path.is_some_and(|folder| dropped(&row, value, &folder))
            }
        ));
        self.add_controller(target.clone());
        let _ = imp.drop.set(target);

        entry.connect_activate(glib::clone!(
            #[weak(rename_to = row)]
            self,
            move |_| row.finish_rename(true)
        ));
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = row)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    row.finish_rename(false);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        entry.add_controller(keys);
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(glib::clone!(
            #[weak(rename_to = row)]
            self,
            move |_| row.finish_rename(true)
        ));
        entry.add_controller(focus);
    }

    fn bind(&self, tree_row: &gtk::TreeListRow) {
        let imp = self.imp();
        let w = self.widgets();
        w.expander.set_list_row(Some(tree_row));
        let Some(info) = tree_row.item().and_downcast::<gio::FileInfo>() else {
            return;
        };
        let path = info_file(&info).and_then(|f| f.path());
        let kind = path.as_deref().map_or(Kind::Note, |p| {
            Kind::of(p, info.file_type() == gio::FileType::Directory)
        });
        imp.kind.set(kind);
        w.icon.set_icon_name(Some(kind.icon()));
        w.label.set_label(
            &path
                .as_deref()
                .map_or_else(String::new, |p| kind.display_name(p)),
        );
        if let Some(target) = imp.drop.get() {
            target.set_propagation_phase(if kind == Kind::Folder {
                gtk::PropagationPhase::Bubble
            } else {
                gtk::PropagationPhase::None
            });
        }
        *imp.path.borrow_mut() = path;
    }

    fn unbind(&self) {
        self.finish_rename(false);
        self.widgets().expander.set_list_row(None);
        self.imp().path.take();
    }

    fn show_menu(&self, x: f64, y: f64) {
        let Some(path) = self.imp().path.borrow().clone() else {
            return;
        };
        let target = path.to_string_lossy().to_variant();
        let item = |label: &str, action: &str, with_path: bool| {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some(action), with_path.then_some(&target));
            item
        };
        let first = gio::Menu::new();
        match self.imp().kind.get() {
            Kind::Folder => {
                first.append_item(&item("New _Note", "win.new-note", true));
                first.append_item(&item("New _Folder", "win.new-folder", true));
                first.append_item(&item("_Import PDF…", "win.import-pdf", true));
            }
            Kind::Note => first.append_item(&item("_Open", "win.open-file", true)),
            Kind::Pdf => first.append_item(&item("_Convert to Note…", "win.open-file", true)),
        }
        let edit = gio::Menu::new();
        edit.append_item(&item("_Rename", "row.rename", false));
        edit.append_item(&item("_Move To…", "win.move-file", true));
        edit.append_item(&item("Move to _Trash", "win.trash-file", true));
        let menu = gio::Menu::new();
        menu.append_section(None, &first);
        menu.append_section(None, &edit);

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(self);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        // Unparent later: the menu activates its action after closing.
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn start_rename(&self) {
        let imp = self.imp();
        let w = self.widgets();
        imp.renaming.set(true);
        w.entry.set_text(&w.label.label());
        w.stack.set_visible_child_name("entry");
        w.entry.grab_focus();
        w.entry.select_region(0, -1);
    }

    /// Leaves the entry; with `commit`, asks the window to rename to its text.
    fn finish_rename(&self, commit: bool) {
        let imp = self.imp();
        if !imp.renaming.replace(false) {
            return;
        }
        let w = self.widgets();
        w.stack.set_visible_child_name("label");
        let name = w.entry.text().trim().to_owned();
        let Some(path) = imp.path.borrow().clone() else {
            return;
        };
        if commit && !name.is_empty() && name != w.label.label().as_str() {
            let target = (path.to_string_lossy().into_owned(), name).to_variant();
            let _ = self.activate_action("win.rename-file", Some(&target));
        }
    }
}
