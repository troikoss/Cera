use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_core::{Document, NoteFile, Rgba8};
use cera_render::tile::DocId;
use gtk::{gdk, gio, glib};

use crate::canvas::{Canvas, HIGHLIGHTER_COLORS, PEN_COLORS, ToolKind};
use crate::note::{Note, SharedNote};
use crate::swatch::Swatch;

/// Save this long after the last edit (DESIGN.md §7), so a crash loses at most that much.
const AUTOSAVE_DELAY: Duration = Duration::from_secs(2);

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
        pub title: TemplateChild<adw::WindowTitle>,
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub note_overlay: TemplateChild<gtk::Overlay>,
        #[template_child]
        pub canvas: TemplateChild<Canvas>,
        #[template_child]
        pub toolbar: TemplateChild<gtk::Box>,
        #[template_child]
        pub tools: TemplateChild<adw::ToggleGroup>,
        pub(super) swatches: RefCell<Vec<(gtk::ToggleButton, Swatch)>>,
        pub(super) note: RefCell<Option<SharedNote>>,
        pub(super) next_doc: Cell<DocId>,
        pub(super) autosave: RefCell<Option<glib::SourceId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CeraWindow {
        const NAME: &'static str = "CeraWindow";
        type Type = super::CeraWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            Canvas::ensure_type();
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
            obj.setup_toolbar();
            self.note_overlay.add_controller(crate::palm::controller());
            for property in ["current-page", "page-count"] {
                self.canvas.connect_notify_local(
                    Some(property),
                    glib::clone!(
                        #[weak]
                        obj,
                        move |_, _| obj.update_subtitle()
                    ),
                );
            }
            self.canvas.connect_local(
                "changed",
                false,
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or_default]
                    move |_| {
                        obj.edited();
                        None
                    }
                ),
            );
            if crate::debug_enabled() {
                obj.add_debug_overlay();
            }
        }
    }

    impl WidgetImpl for CeraWindow {}

    impl WindowImpl for CeraWindow {
        fn close_request(&self) -> glib::Propagation {
            self.obj().save_now();
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

    fn setup_actions(&self) {
        let imp = self.imp();
        // Toggles the sidebar; drives the header button and F9.
        let show_sidebar =
            gio::PropertyAction::new("show-sidebar", &*imp.split_view, "show-sidebar");
        self.add_action(&show_sidebar);

        let open = gio::ActionEntry::builder("open")
            .activate(|win: &Self, _, _| win.open_dialog())
            .build();
        let undo = gio::ActionEntry::builder("undo")
            .activate(|win: &Self, _, _| win.imp().canvas.undo())
            .build();
        let redo = gio::ActionEntry::builder("redo")
            .activate(|win: &Self, _, _| win.imp().canvas.redo())
            .build();
        let zoom_in = gio::ActionEntry::builder("zoom-in")
            .activate(|win: &Self, _, _| win.imp().canvas.zoom_step(1))
            .build();
        let zoom_out = gio::ActionEntry::builder("zoom-out")
            .activate(|win: &Self, _, _| win.imp().canvas.zoom_step(-1))
            .build();
        let zoom_fit = gio::ActionEntry::builder("zoom-fit")
            .activate(|win: &Self, _, _| win.imp().canvas.zoom_fit_width())
            .build();
        let tool = gio::ActionEntry::builder("tool")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(|win: &Self, _, param| {
                if let Some(name) = param.and_then(|p| p.get::<String>()) {
                    win.imp().tools.set_active_name(Some(&name));
                }
            })
            .build();
        self.add_action_entries([open, undo, redo, zoom_in, zoom_out, zoom_fit, tool]);
        self.update_undo_actions();
    }

    /// Tool toggles follow the canvas's tool; three swatches show the pen's or the
    /// highlighter's colours.
    fn setup_toolbar(&self) {
        let imp = self.imp();
        imp.tools
            .bind_property("active-name", &*imp.canvas, "tool")
            .bidirectional()
            .sync_create()
            .build();
        let mut first: Option<gtk::ToggleButton> = None;
        for i in 0..3 {
            let swatch = Swatch::new(gdk::RGBA::BLACK);
            let button = gtk::ToggleButton::builder()
                .child(&swatch)
                .css_classes(["flat"])
                .tooltip_text("Colour")
                .build();
            button.set_group(first.as_ref());
            first.get_or_insert_with(|| button.clone());
            button.connect_toggled(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |b| {
                    if b.is_active() {
                        win.imp().canvas.set_color(win.palette()[i]);
                    }
                }
            ));
            imp.toolbar.append(&button);
            imp.swatches.borrow_mut().push((button, swatch));
        }
        imp.canvas.connect_notify_local(
            Some("tool"),
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |_, _| win.update_swatches()
            ),
        );
        self.update_swatches();
    }

    fn palette(&self) -> [Rgba8; 3] {
        match self.imp().canvas.tool() {
            ToolKind::Highlighter => HIGHLIGHTER_COLORS,
            _ => PEN_COLORS,
        }
    }

    fn update_swatches(&self) {
        let imp = self.imp();
        let palette = self.palette();
        let current = imp.canvas.color();
        let drawing = imp.canvas.tool() != ToolKind::Eraser;
        for (i, (button, swatch)) in imp.swatches.borrow().iter().enumerate() {
            let c = palette[i];
            swatch.set_color(gdk::RGBA::new(
                f32::from(c.r) / 255.0,
                f32::from(c.g) / 255.0,
                f32::from(c.b) / 255.0,
                f32::from(c.a) / 255.0,
            ));
            button.set_sensitive(drawing);
            if c == current {
                button.set_active(true);
            }
        }
    }

    fn update_undo_actions(&self) {
        let (can_undo, can_redo) = self
            .imp()
            .note
            .borrow()
            .as_ref()
            .map_or((false, false), |n| {
                let n = n.borrow();
                (n.history.can_undo(), n.history.can_redo())
            });
        for (name, enabled) in [("undo", can_undo), ("redo", can_redo)] {
            if let Some(action) = self.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                action.set_enabled(enabled);
            }
        }
    }

    /// After an edit: refresh the undo buttons and save once editing pauses.
    fn edited(&self) {
        self.update_undo_actions();
        let imp = self.imp();
        if let Some(id) = imp.autosave.take() {
            id.remove();
        }
        let weak = self.downgrade();
        let id = glib::timeout_add_local_once(AUTOSAVE_DELAY, move || {
            if let Some(win) = weak.upgrade() {
                win.imp().autosave.take();
                win.save_now();
            }
        });
        *imp.autosave.borrow_mut() = Some(id);
    }

    /// Saves the open note if it has unsaved changes.
    pub fn save_now(&self) {
        let imp = self.imp();
        if let Some(id) = imp.autosave.take() {
            id.remove();
        }
        let Some(note) = imp.note.borrow().clone() else {
            return;
        };
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
                    win.open_path(path).await;
                }
            }
        ));
    }

    /// Opens a note; a PDF is first converted to a note next to it, unless one exists.
    pub async fn open_path(&self, path: PathBuf) {
        let is_pdf = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
        let note_path = if is_pdf {
            path.with_extension("cera")
        } else {
            path.clone()
        };
        let name = display_name(&path);
        let result = if is_pdf && !note_path.exists() {
            let (pdf, out) = (path.clone(), note_path.clone());
            let converted = gio::spawn_blocking(move || {
                cera_render::import::import_pdf(&pdf, &out).map_err(anyhow::Error::from)
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("conversion crashed")));
            if converted.is_ok() {
                self.toast(&format!(
                    "Converted “{name}” to “{}”",
                    display_name(&note_path)
                ));
            }
            converted
        } else {
            NoteFile::open(&note_path).map_err(anyhow::Error::from)
        };
        match result {
            Ok((file, doc)) => self.show_note(file, doc),
            Err(e) => {
                tracing::warn!(path = %path.display(), "cannot open: {e:#}");
                self.toast(&format!("Cannot open “{name}”: {e}"));
            }
        }
    }

    fn show_note(&self, file: NoteFile, doc: Document) {
        let imp = self.imp();
        self.save_now();
        let id = imp.next_doc.get();
        imp.next_doc.set(id + 1);
        tracing::info!(path = %file.path().display(), pages = doc.page_count(), "opened note");
        let note = Note::new(id, file, doc);
        let name = note.borrow().name();
        *imp.note.borrow_mut() = Some(note.clone());
        imp.canvas.set_note(Some(note));
        imp.title.set_title(&name);
        self.set_title(Some(&name));
        imp.stack.set_visible_child_name("note");
        self.update_undo_actions();
        if std::env::var_os("CERA_DEBUG_DEMO").is_some() {
            let canvas = imp.canvas.downgrade();
            glib::timeout_add_local_once(Duration::from_millis(800), move || {
                if let Some(canvas) = canvas.upgrade() {
                    canvas.run_demo();
                }
            });
        }
    }

    fn update_subtitle(&self) {
        let canvas = &self.imp().canvas;
        let count = canvas.page_count();
        let text = if count > 0 {
            format!("Page {} of {count}", canvas.current_page() + 1)
        } else {
            String::new()
        };
        self.imp().title.set_subtitle(&text);
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
        let canvas = self.imp().canvas.downgrade();
        let label = label.downgrade();
        glib::timeout_add_local(Duration::from_millis(500), move || {
            let (Some(canvas), Some(label)) = (canvas.upgrade(), label.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            label.set_text(&canvas.debug_text());
            glib::ControlFlow::Continue
        });
    }
}

fn display_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}
