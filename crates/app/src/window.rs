use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::Duration;

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_core::{Document, NoteFile};
use cera_render::tile::DocId;
use gtk::{gio, glib};

use crate::canvas::{Canvas, CanvasDoc};

/// The note shown in the window, kept open for editing (M4) and autosave.
struct OpenNote {
    id: DocId,
    #[expect(dead_code, reason = "saved from M4")]
    file: NoteFile,
    #[expect(dead_code, reason = "edited from M4")]
    doc: Document,
}

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
        pub(super) note: RefCell<Option<OpenNote>>,
        pub(super) next_doc: Cell<DocId>,
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
            if crate::debug_enabled() {
                obj.add_debug_overlay();
            }
        }
    }

    impl WidgetImpl for CeraWindow {}
    impl WindowImpl for CeraWindow {}
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
        let zoom_in = gio::ActionEntry::builder("zoom-in")
            .activate(|win: &Self, _, _| win.imp().canvas.zoom_step(1))
            .build();
        let zoom_out = gio::ActionEntry::builder("zoom-out")
            .activate(|win: &Self, _, _| win.imp().canvas.zoom_step(-1))
            .build();
        let zoom_fit = gio::ActionEntry::builder("zoom-fit")
            .activate(|win: &Self, _, _| win.imp().canvas.zoom_fit_width())
            .build();
        self.add_action_entries([open, zoom_in, zoom_out, zoom_fit]);
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
        let renderer = imp.canvas.renderer();
        if let Some(old) = imp.note.take() {
            renderer.close_doc(old.id);
        }
        let id = imp.next_doc.get();
        imp.next_doc.set(id + 1);
        renderer.open_doc(
            id,
            file.assets()
                .filter_map(|a| Some((a.id, file.asset(a.id)?))),
        );
        imp.canvas.set_document(Some(CanvasDoc::new(id, &doc)));

        let name = file
            .path()
            .file_stem()
            .map_or_else(|| "Note".into(), |s| s.to_string_lossy().into_owned());
        imp.title.set_title(&name);
        self.set_title(Some(&name));
        imp.stack.set_visible_child_name("note");
        tracing::info!(path = %file.path().display(), pages = doc.page_count(), "opened note");
        *imp.note.borrow_mut() = Some(OpenNote { id, file, doc });
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
