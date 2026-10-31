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
            let app = self.obj();
            let window = app
                .active_window()
                .unwrap_or_else(|| CeraWindow::new(&*app).upcast());
            window.present();
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
            .build()
    }

    fn setup_actions(&self) {
        let quit = gio::ActionEntry::builder("quit")
            .activate(|app: &Self, _, _| app.quit())
            .build();
        let about = gio::ActionEntry::builder("about")
            .activate(|app: &Self, _, _| app.show_about())
            .build();
        self.add_action_entries([quit, about]);

        self.set_accels_for_action("app.quit", &["<Control>q"]);
        self.set_accels_for_action("win.show-sidebar", &["F9"]);
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
