use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

mod imp {
    use super::*;

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/troikoss/Cera/window.ui")]
    pub struct CeraWindow {
        #[template_child]
        pub split_view: TemplateChild<adw::OverlaySplitView>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CeraWindow {
        const NAME: &'static str = "CeraWindow";
        type Type = super::CeraWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for CeraWindow {
        fn constructed(&self) {
            self.parent_constructed();
            // Toggles the sidebar; drives the header button and F9.
            let show_sidebar =
                gio::PropertyAction::new("show-sidebar", &*self.split_view, "show-sidebar");
            self.obj().add_action(&show_sidebar);
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
}
