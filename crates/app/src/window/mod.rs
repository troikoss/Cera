// SPDX-License-Identifier: GPL-3.0-or-later
//! The main window: vault sidebar, one tab per open note, and the floating tool bar.
//!
//! - `mod.rs`: widgets, actions and keyboard shortcuts.
//! - `toolbar.rs`: the floating tool bar: colour and size slots and their popovers.
//! - `tabs.rs`: notes in tabs: opening, autosave, closing, restoring the workspace.
//! - `files.rs`: the vault: opening one; creating, renaming, moving, trashing and importing.
//! - `pages.rs`: the page sorter, a view of the sidebar.
//! - `export.rs`: PDF and PNG export.
//! - `prefs.rs`: preferences and the shortcuts list.

mod bookmarks;
mod export;
mod files;
mod pages;
mod prefs;
mod tabs;
mod toolbar;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_core::{Arrange, Rgba8};
use cera_render::tile::DocId;
use cera_render::worker::{Renderer, TileResult};
use gtk::{gdk, gio, glib};

use crate::canvas::{Canvas, Tools};
use crate::sidebar::Sidebar;
use crate::swatch::Swatch;
use crate::vault::{Vault, Workspace};

/// Where a render result goes: the canvas or page sorter showing that note.
type Sink = Rc<dyn Fn(TileResult)>;

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/troikoss/Cera/window.ui")]
    pub struct CeraWindow {
        #[template_child]
        pub split_view: TemplateChild<adw::OverlaySplitView>,
        #[template_child]
        pub toasts: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub sidebar_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub sidebar: TemplateChild<Sidebar>,
        #[template_child]
        pub title: TemplateChild<adw::WindowTitle>,
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub recent_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub note_overlay: TemplateChild<gtk::Overlay>,
        #[template_child]
        pub tab_view: TemplateChild<adw::TabView>,
        #[template_child]
        pub toolbar_scroll: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub toolbar: TemplateChild<gtk::Box>,
        #[template_child]
        pub tools: TemplateChild<adw::ToggleGroup>,
        #[template_child]
        pub swatch_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub swatch_separator: TemplateChild<gtk::Separator>,
        #[template_child]
        pub size_separator: TemplateChild<gtk::Separator>,
        #[template_child]
        pub size_box: TemplateChild<gtk::Box>,
        pub(super) tool_state: Rc<Tools>,
        pub(super) swatches: RefCell<Vec<(gtk::ToggleButton, Swatch)>>,
        pub(super) sizes: RefCell<Vec<(gtk::ToggleButton, Swatch)>>,
        /// Where the tool buttons were last tapped, for pointing their popover at the tool.
        pub(super) tool_tap: Cell<(f64, f64)>,
        pub(super) renderer: OnceCell<Rc<Renderer>>,
        pub(super) sinks: RefCell<HashMap<DocId, Sink>>,
        /// The sidebar's pages view, and its sorter while it is on screen.
        pub(super) pages: OnceCell<pages::PagesPanel>,
        pub(super) sorter: RefCell<Option<Rc<pages::Sorter>>>,
        pub(super) next_doc: Cell<DocId>,
        pub(super) autosave: RefCell<HashMap<DocId, glib::SourceId>>,
        pub(super) vault: RefCell<Option<Vault>>,
        pub(super) workspace: RefCell<Workspace>,
        pub(super) workspace_save: RefCell<Option<glib::SourceId>>,
        /// Set while tabs are being restored, so the half-restored state isn't saved.
        pub(super) restoring: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CeraWindow {
        const NAME: &'static str = "CeraWindow";
        type Type = super::CeraWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            Canvas::ensure_type();
            Sidebar::ensure_type();
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for CeraWindow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_actions();
            obj.setup_pages();
            obj.setup_bookmarks();
            obj.setup_renderer();
            obj.setup_toolbar();
            obj.setup_tabs();
            obj.setup_drop();
            obj.setup_shortcuts();
            // Palm rejection on the overlay sees touches before the finger gestures on the
            // tab view inside it (capture runs from parent to child).
            self.note_overlay.add_controller(crate::palm::controller());
            let weak = obj.downgrade();
            self.tab_view
                .add_controller(crate::touch::controller(move || weak.upgrade()?.canvas()));
            obj.rebuild_recent();
            obj.update_stack();
            if crate::debug_enabled() {
                obj.add_debug_overlay();
            }
        }
    }

    impl WidgetImpl for CeraWindow {}

    impl WindowImpl for CeraWindow {
        fn close_request(&self) -> glib::Propagation {
            self.obj().shutdown();
            self.parent_close_request()
        }
    }

    impl ApplicationWindowImpl for CeraWindow {}
    impl AdwApplicationWindowImpl for CeraWindow {}
}

glib::wrapper! {
    pub struct CeraWindow(ObjectSubclass<imp::CeraWindow>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
            gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl CeraWindow {
    pub fn new(app: &impl IsA<gtk::Application>) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    /// Saves every open note and the workspace; for closing the window or quitting.
    pub fn shutdown(&self) {
        self.save_workspace_now();
        self.save_all();
    }

    /// One pool of render threads per window; results go to whoever registered the note.
    fn setup_renderer(&self) {
        let threads =
            std::thread::available_parallelism().map_or(1, |n| if n.get() >= 4 { 2 } else { 1 });
        let (tx, rx) = async_channel::unbounded();
        let _ = self.imp().renderer.set(Rc::new(Renderer::new(threads, tx)));
        let weak = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(result) = rx.recv().await {
                let Some(win) = weak.upgrade() else { break };
                let sink = win.imp().sinks.borrow().get(&result.key.doc).cloned();
                if let Some(sink) = sink {
                    sink(result);
                }
            }
        });
    }

    fn renderer(&self) -> Rc<Renderer> {
        self.imp()
            .renderer
            .get()
            .expect("set up in constructed")
            .clone()
    }

    fn setup_actions(&self) {
        let imp = self.imp();
        // Toggles the sidebar; drives the header button and F9.
        let show_sidebar =
            gio::PropertyAction::new("show-sidebar", &*imp.split_view, "show-sidebar");
        self.add_action(&show_sidebar);

        let canvas_action = |name: &str, f: fn(&Canvas)| {
            gio::ActionEntry::builder(name)
                .activate(move |win: &Self, _, _| {
                    if let Some(canvas) = win.canvas() {
                        f(&canvas);
                    }
                })
                .build()
        };
        let path_action = |name: &str, f: fn(&Self, PathBuf)| {
            gio::ActionEntry::builder(name)
                .parameter_type(Some(glib::VariantTy::STRING))
                .activate(move |win: &Self, _, param| {
                    if let Some(path) = param.and_then(|p| p.get::<String>()) {
                        f(win, PathBuf::from(path));
                    }
                })
                .build()
        };
        // "" means the sidebar's current folder.
        let folder_action = |name: &str, f: fn(&Self, Option<PathBuf>)| {
            gio::ActionEntry::builder(name)
                .parameter_type(Some(glib::VariantTy::STRING))
                .activate(move |win: &Self, _, param| {
                    let path = param.and_then(|p| p.get::<String>()).unwrap_or_default();
                    f(win, (!path.is_empty()).then(|| PathBuf::from(path)));
                })
                .build()
        };
        let pair_action = |name: &str, f: fn(&Self, PathBuf, String)| {
            gio::ActionEntry::builder(name)
                .parameter_type(Some(glib::VariantTy::new("(ss)").expect("valid type")))
                .activate(move |win: &Self, _, param| {
                    if let Some((a, b)) = param.and_then(|p| p.get::<(String, String)>()) {
                        f(win, PathBuf::from(a), b);
                    }
                })
                .build()
        };
        let simple = |name: &str, f: fn(&Self)| {
            gio::ActionEntry::builder(name)
                .activate(move |win: &Self, _, _| f(win))
                .build()
        };
        let tool = gio::ActionEntry::builder("tool")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(|win: &Self, _, param| {
                if let Some(name) = param.and_then(|p| p.get::<String>()) {
                    win.imp().tools.set_active_name(Some(&name));
                }
            })
            .build();
        let change_style = gio::ActionEntry::builder("change-style")
            .parameter_type(Some(glib::VariantTy::new("(ddu)").expect("valid type")))
            .activate(|win: &Self, _, param| {
                if let Some((x, y, source)) = param.and_then(|p| p.get::<(f64, f64, u32)>()) {
                    win.show_style_popover((x, y), crate::canvas::InputSource::from_u32(source));
                }
            })
            .build();
        let arrange = gio::ActionEntry::builder("arrange")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(|win: &Self, _, param| {
                let how = match param.and_then(|p| p.get::<String>()).as_deref() {
                    Some("front") => Arrange::Front,
                    Some("forward") => Arrange::Forward,
                    Some("backward") => Arrange::Backward,
                    Some("back") => Arrange::Back,
                    _ => return,
                };
                if let Some(canvas) = win.canvas() {
                    canvas.arrange_selection(how);
                }
            })
            .build();
        self.add_action_entries([
            simple("open", Self::open_dialog),
            simple("open-vault", |win| win.open_vault_dialog(false)),
            simple("create-vault", |win| win.open_vault_dialog(true)),
            path_action("open-vault-path", |win, path| win.open_vault(&path, false)),
            path_action("open-file", |win, path| win.open_file(path)),
            path_action("move-file", Self::move_file),
            path_action("trash-file", Self::trash_file),
            folder_action("new-note", Self::new_note),
            folder_action("new-folder", Self::new_folder),
            folder_action("import-pdf", Self::import_pdf_dialog),
            pair_action("rename-file", Self::rename_file),
            pair_action("drop-file", |win, src, dest| {
                win.drop_file(src, PathBuf::from(dest))
            }),
            simple("close-tab", Self::close_current_tab),
            simple("search", |win| {
                win.imp().split_view.set_show_sidebar(true);
                win.imp().sidebar.start_search();
            }),
            canvas_action("undo", Canvas::undo),
            canvas_action("redo", Canvas::redo),
            simple("cut", |win| win.clipboard(Canvas::cut_selection)),
            simple("copy", |win| {
                win.clipboard(|c| {
                    c.copy_selection();
                })
            }),
            canvas_action("delete-selection", Canvas::delete_selection),
            canvas_action("select-none", Canvas::clear_selection),
            simple("paste", |win| win.paste(None)),
            gio::ActionEntry::builder("paste-at")
                .parameter_type(Some(glib::VariantTy::new("(dd)").expect("valid type")))
                .activate(|win: &Self, _, param| {
                    win.paste(param.and_then(|p| p.get::<(f64, f64)>()));
                })
                .build(),
            simple("add-page", Self::add_page),
            simple("preferences", Self::show_preferences),
            simple("shortcuts", Self::show_shortcuts),
            simple("export-pdf", |win| win.export(export::Format::Pdf)),
            simple("export-png", |win| win.export(export::Format::Png)),
            canvas_action("zoom-in", |c| c.zoom_step(1)),
            canvas_action("zoom-out", |c| c.zoom_step(-1)),
            canvas_action("zoom-fit", Canvas::zoom_fit_width),
            tool,
            arrange,
            change_style,
        ]);
        self.update_note_actions();
    }

    /// Keys that text fields also use (letters, Delete, Ctrl+C…) only act here when the
    /// focused widget doesn't take them, so they are handled in the bubble phase rather than
    /// as application accelerators.
    fn setup_shortcuts(&self) {
        let controller = gtk::ShortcutController::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Bubble);
        let shortcuts: [(&str, &str, Option<&str>); 18] = [
            ("p", "win.tool", Some("pen")),
            ("h", "win.tool", Some("highlighter")),
            ("e", "win.tool", Some("eraser")),
            ("l", "win.tool", Some("lasso")),
            ("<Control>z", "win.undo", None),
            ("<Control><Shift>z|<Control>y", "win.redo", None),
            ("<Control>x", "win.cut", None),
            ("<Control>c", "win.copy", None),
            ("<Control>v", "win.paste", None),
            ("Delete|BackSpace", "win.delete-selection", None),
            ("Escape", "win.select-none", None),
            ("<Control>bracketright", "win.arrange", Some("forward")),
            ("<Control>bracketleft", "win.arrange", Some("backward")),
            (
                "<Control><Shift>bracketright|<Control>braceright",
                "win.arrange",
                Some("front"),
            ),
            (
                "<Control><Shift>bracketleft|<Control>braceleft",
                "win.arrange",
                Some("back"),
            ),
            ("<Control>Return", "win.add-page", None),
            ("<Control>p", "win.pages", None),
            ("<Control>d", "win.bookmark", None),
        ];
        for (keys, action, target) in shortcuts {
            let shortcut = gtk::Shortcut::builder()
                .trigger(&gtk::ShortcutTrigger::parse_string(keys).expect("valid trigger"))
                .action(&gtk::NamedAction::new(action))
                .build();
            if let Some(target) = target {
                shortcut.set_arguments(Some(&target.to_variant()));
            }
            controller.add_shortcut(shortcut);
        }
        self.add_controller(controller);
    }

    /// Cut or copy; afterwards Paste is available in every tab.
    fn clipboard(&self, f: impl FnOnce(&Canvas)) {
        if let Some(canvas) = self.canvas() {
            f(&canvas);
            self.update_note_actions();
        }
    }

    /// Pastes into the selected tab (centred on view point `at`, else in the middle of the
    /// view) and switches to the lasso to move what was pasted.
    fn paste(&self, at: Option<(f64, f64)>) {
        if let Some(canvas) = self.canvas()
            && canvas.paste(at)
        {
            self.imp().tools.set_active_name(Some("lasso"));
        }
    }

    fn add_page(&self) {
        let paper = self.new_page_paper();
        if let Some(canvas) = self.canvas() {
            canvas.add_page(|spec| pages::blank_like(spec, paper));
        }
    }

    /// Accepts PDFs and notes dropped anywhere on the note area.
    fn setup_drop(&self) {
        let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        target.connect_drop(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                let Ok(list) = value.get::<gdk::FileList>() else {
                    return false;
                };
                let Some(folder) = win.imp().sidebar.current_folder() else {
                    for path in list.files().iter().filter_map(|f| f.path()) {
                        win.open_path(path);
                    }
                    return true;
                };
                for path in list.files().iter().filter_map(|f| f.path()) {
                    win.drop_file(path, folder.clone());
                }
                true
            }
        ));
        self.imp().stack.add_controller(target);
    }

    fn open_dialog(&self) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("PDFs and notes"));
        filter.add_suffix("pdf");
        filter.add_suffix("cera");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Open PDF or Note")
            .filters(&filters)
            .default_filter(&filter)
            .build();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                // An error here means the dialog was cancelled.
                let Ok(file) = dialog.open_future(Some(&win)).await else {
                    return;
                };
                if let Some(path) = file.path() {
                    win.open_path(path);
                }
            }
        ));
    }

    /// The content area: the welcome page without a vault, a hint without tabs, else tabs.
    fn update_stack(&self) {
        let imp = self.imp();
        let name = if imp.tab_view.n_pages() > 0 {
            "tabs"
        } else if imp.vault.borrow().is_some() {
            "vault"
        } else {
            "welcome"
        };
        imp.stack.set_visible_child_name(name);
    }

    fn toast(&self, text: &str) {
        self.imp().toasts.add_toast(adw::Toast::new(text));
    }

    /// `CERA_DEBUG=1`: tiles, memory and frame times in the corner, refreshed twice a second.
    fn add_debug_overlay(&self) {
        let label = gtk::Label::builder()
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .margin_top(8)
            .margin_end(8)
            .can_target(false)
            .build();
        label.add_css_class("debug-overlay");
        self.imp().note_overlay.add_overlay(&label);
        let win = self.downgrade();
        let label = label.downgrade();
        glib::timeout_add_local(Duration::from_millis(500), move || {
            let (Some(win), Some(label)) = (win.upgrade(), label.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            if let Some(canvas) = win.canvas() {
                let text = canvas.debug_text();
                tracing::debug!(target: "cera::stats", "{}", text.replace('\n', " · "));
                label.set_text(&text);
            }
            glib::ControlFlow::Continue
        });
    }
}

fn rgba(c: Rgba8) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(c.r) / 255.0,
        f32::from(c.g) / 255.0,
        f32::from(c.b) / 255.0,
        f32::from(c.a) / 255.0,
    )
}

fn display_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}
