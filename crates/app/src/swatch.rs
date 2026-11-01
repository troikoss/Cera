//! A colour dot for the tool bar's swatch and width buttons: a rounded clip and a colour node.

use std::cell::Cell;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

const SIZE: i32 = 22;

mod imp {
    use super::*;

    pub struct Swatch {
        pub(super) color: Cell<gdk::RGBA>,
        pub(super) size: Cell<i32>,
    }

    impl Default for Swatch {
        fn default() -> Self {
            Self {
                color: Cell::new(gdk::RGBA::BLACK),
                size: Cell::new(SIZE),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Swatch {
        const NAME: &'static str = "CeraSwatch";
        type Type = super::Swatch;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Swatch {}

    impl WidgetImpl for Swatch {
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let size = self.size.get();
            (size, size, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (w, h) = (obj.width() as f32, obj.height() as f32);
            let d = w.min(h);
            let bounds = graphene::Rect::new((w - d) / 2.0, (h - d) / 2.0, d, d);
            let circle = gsk::RoundedRect::from_rect(bounds, d / 2.0);
            snapshot.push_rounded_clip(&circle);
            // Translucent highlighter colours are shown at full strength to stay readable.
            let mut color = self.color.get();
            color.set_alpha(1.0);
            snapshot.append_color(&color, &bounds);
            snapshot.pop();
            let edge = gdk::RGBA::new(0.0, 0.0, 0.0, 0.2);
            snapshot.append_border(&circle, &[1.0; 4], &[edge; 4]);
        }
    }
}

glib::wrapper! {
    pub struct Swatch(ObjectSubclass<imp::Swatch>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Swatch {
    pub fn new(color: gdk::RGBA) -> Self {
        let swatch: Self = glib::Object::builder()
            .property("halign", gtk::Align::Center)
            .property("valign", gtk::Align::Center)
            .build();
        swatch.set_color(color);
        swatch
    }

    pub fn set_color(&self, color: gdk::RGBA) {
        self.imp().color.set(color);
        self.queue_draw();
    }

    /// Diameter in logical pixels (22 by default); width presets use smaller dots.
    pub fn set_size(&self, size: i32) {
        self.imp().size.set(size);
        self.queue_resize();
    }
}
