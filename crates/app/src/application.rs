use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::APP_ID;
use crate::window::CeraWindow;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct CeraApplication;

    #[glib::object_subclass]
    impl ObjectSubclass for CeraApplication {
        const NAME: &'static str = "CeraApplication";
        type Type = super::CeraApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for CeraApplication {}

    impl ApplicationImpl for CeraApplication {
        fn startup(&self) {
            self.parent_startup();
            self.obj().setup_actions();
        }

        fn activate(&self) {
            self.parent_activate();
            self.obj().window().present();
        }

        /// `cera file.pdf` or `cera note.cera`: show it (only one note at a time until M5).
        fn open(&self, files: &[gio::File], _hint: &str) {
            let window = self.obj().window();
            window.present();
            if let Some(path) = files.first().and_then(|f| f.path()) {
                glib::spawn_future_local(async move { window.open_path(path).await });
            }
        }
    }

    impl GtkApplicationImpl for CeraApplication {}
    impl AdwApplicationImpl for CeraApplication {}
}

glib::wrapper! {
    pub struct CeraApplication(ObjectSubclass<imp::CeraApplication>)
        @extends adw::Application, gtk::Application, gio::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl CeraApplication {
    pub fn new() -> Self {
        glib::Object::builder()
            .property("application-id", APP_ID)
            .property("flags", gio::ApplicationFlags::HANDLES_OPEN)
            .build()
    }

    fn window(&self) -> CeraWindow {
        self.active_window()
            .and_downcast()
            .unwrap_or_else(|| CeraWindow::new(self))
    }

    fn setup_actions(&self) {
        let quit = gio::ActionEntry::builder("quit")
            .activate(|app: &Self, _, _| {
                for window in app.windows() {
                    if let Some(window) = window.downcast_ref::<CeraWindow>() {
                        window.save_now();
                    }
                }
                app.quit();
            })
            .build();
        let about = gio::ActionEntry::builder("about")
            .activate(|app: &Self, _, _| app.show_about())
            .build();
        self.add_action_entries([quit, about]);

        self.set_accels_for_action("app.quit", &["<Control>q"]);
        self.set_accels_for_action("win.show-sidebar", &["F9"]);
        self.set_accels_for_action("win.open", &["<Control>o"]);
        self.set_accels_for_action("win.zoom-in", &["<Control>plus", "<Control>equal"]);
        self.set_accels_for_action("win.zoom-out", &["<Control>minus"]);
        self.set_accels_for_action("win.zoom-fit", &["<Control>0"]);
        self.set_accels_for_action("win.undo", &["<Control>z"]);
        self.set_accels_for_action("win.redo", &["<Control><Shift>z", "<Control>y"]);
        self.set_accels_for_action("win.tool::pen", &["p"]);
        self.set_accels_for_action("win.tool::highlighter", &["h"]);
        self.set_accels_for_action("win.tool::eraser", &["e"]);
    }

    fn show_about(&self) {
        let dialog = adw::AboutDialog::builder()
            .application_name("Cera")
            .application_icon(APP_ID)
            .developer_name("troikoss")
            .version(env!("CARGO_PKG_VERSION"))
            .license_type(gtk::License::Gpl30)
            .build();
        dialog.present(self.active_window().as_ref());
    }
}
