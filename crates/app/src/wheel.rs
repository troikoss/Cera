// SPDX-License-Identifier: GPL-3.0-or-later
//! A colour wheel for the palette popover: a hue ring around a saturation and value square.
//! Everything is drawn from gradient nodes, so nothing is rasterised or cached. Dragging on
//! the ring picks the hue, on the square the saturation (across) and value (up); `changed`
//! fires while dragging and `picked` once the finger lifts.

use std::cell::Cell;

use cera_core::Rgba8;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// Diameter in logical pixels, and the ring's thickness.
const SIZE: i32 = 208;
const RING: f32 = 22.0;
/// Space between the ring and the square.
const GAP: f32 = 8.0;
const MARKER: f32 = 7.0;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Part {
    Ring,
    Square,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ColorWheel {
        /// Hue in degrees (0–360), saturation and value (0–1).
        pub(super) hsv: Cell<(f32, f32, f32)>,
        pub(super) dragging: Cell<Option<Part>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ColorWheel {
        const NAME: &'static str = "CeraColorWheel";
        type Type = super::ColorWheel;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ColorWheel {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: std::sync::OnceLock<Vec<glib::subclass::Signal>> =
                std::sync::OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // The colour moved under the finger.
                    glib::subclass::Signal::builder("changed").build(),
                    // The finger lifted: the colour is chosen.
                    glib::subclass::Signal::builder("picked").build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup_input();
        }
    }

    impl WidgetImpl for ColorWheel {
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (SIZE, SIZE, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let geo = Geometry::of(&*obj);
            let (h, s, v) = self.hsv.get();
            let (cx, cy, r) = (geo.cx, geo.cy, geo.radius);
            let centre = graphene::Point::new(cx, cy);
            let outer = graphene::Rect::new(cx - r, cy - r, 2.0 * r, 2.0 * r);

            // Hue ring: a conic gradient (0° at the top, clockwise) filled through two circles.
            let ring = gsk::PathBuilder::new();
            ring.add_circle(&centre, r);
            ring.add_circle(&centre, r - RING);
            snapshot.push_fill(&ring.to_path(), gsk::FillRule::EvenOdd);
            let stops: Vec<gsk::ColorStop> = (0..=6)
                .map(|i| {
                    let hue = i as f32 * 60.0;
                    gsk::ColorStop::new(i as f32 / 6.0, hsv_rgba(hue, 1.0, 1.0))
                })
                .collect();
            snapshot.append_conic_gradient(&outer, &centre, 0.0, &stops);
            snapshot.pop();

            // Square: white to the pure hue across, then clear to black downwards.
            let sq = geo.square();
            let left = graphene::Point::new(sq.x(), sq.y());
            let right = graphene::Point::new(sq.x() + sq.width(), sq.y());
            let bottom = graphene::Point::new(sq.x(), sq.y() + sq.height());
            snapshot.append_linear_gradient(
                &sq,
                &left,
                &right,
                &[
                    gsk::ColorStop::new(0.0, gdk::RGBA::WHITE),
                    gsk::ColorStop::new(1.0, hsv_rgba(h, 1.0, 1.0)),
                ],
            );
            snapshot.append_linear_gradient(
                &sq,
                &left,
                &bottom,
                &[
                    gsk::ColorStop::new(0.0, gdk::RGBA::new(0.0, 0.0, 0.0, 0.0)),
                    gsk::ColorStop::new(1.0, gdk::RGBA::BLACK),
                ],
            );

            // Markers: a white ring with a dark edge, readable on any colour.
            let angle = h.to_radians();
            let mid = r - RING / 2.0;
            marker(snapshot, cx + mid * angle.sin(), cy - mid * angle.cos());
            marker(
                snapshot,
                sq.x() + s * sq.width(),
                sq.y() + (1.0 - v) * sq.height(),
            );
        }
    }
}

glib::wrapper! {
    pub struct ColorWheel(ObjectSubclass<imp::ColorWheel>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ColorWheel {
    pub fn new(color: Rgba8) -> Self {
        let wheel: Self = glib::Object::builder()
            .property("halign", gtk::Align::Center)
            .build();
        wheel.imp().hsv.set(rgb_to_hsv(color));
        wheel
    }

    /// The chosen colour, opaque.
    pub fn color(&self) -> Rgba8 {
        let (h, s, v) = self.imp().hsv.get();
        hsv_to_rgb(h, s, v)
    }

    fn setup_input(&self) {
        let drag = gtk::GestureDrag::new();
        drag.connect_drag_begin(glib::clone!(
            #[weak(rename_to = wheel)]
            self,
            move |g, x, y| {
                let part = Geometry::of(&wheel).part_at(x as f32, y as f32);
                if part.is_none() {
                    g.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                g.set_state(gtk::EventSequenceState::Claimed);
                wheel.imp().dragging.set(part);
                wheel.pick(x, y);
            }
        ));
        drag.connect_drag_update(glib::clone!(
            #[weak(rename_to = wheel)]
            self,
            move |g, dx, dy| {
                if let Some((x, y)) = g.start_point() {
                    wheel.pick(x + dx, y + dy);
                }
            }
        ));
        drag.connect_drag_end(glib::clone!(
            #[weak(rename_to = wheel)]
            self,
            move |_, _, _| {
                if wheel.imp().dragging.take().is_some() {
                    wheel.emit_by_name::<()>("picked", &[]);
                }
            }
        ));
        self.add_controller(drag);
    }

    /// Moves the part being dragged to (`x`, `y`).
    fn pick(&self, x: f64, y: f64) {
        let imp = self.imp();
        let geo = Geometry::of(self);
        let (x, y) = (x as f32, y as f32);
        let (mut h, mut s, mut v) = imp.hsv.get();
        match imp.dragging.get() {
            Some(Part::Ring) => {
                h = (x - geo.cx)
                    .atan2(geo.cy - y)
                    .to_degrees()
                    .rem_euclid(360.0);
            }
            Some(Part::Square) => {
                let sq = geo.square();
                s = ((x - sq.x()) / sq.width()).clamp(0.0, 1.0);
                v = 1.0 - ((y - sq.y()) / sq.height()).clamp(0.0, 1.0);
            }
            None => return,
        }
        imp.hsv.set((h, s, v));
        self.queue_draw();
        self.emit_by_name::<()>("changed", &[]);
    }

    pub fn connect_changed(&self, f: impl Fn(&Self) + 'static) {
        self.connect_local("changed", false, move |args| {
            f(&args[0].get::<Self>().expect("emitted by a ColorWheel"));
            None
        });
    }

    pub fn connect_picked(&self, f: impl Fn(&Self) + 'static) {
        self.connect_local("picked", false, move |args| {
            f(&args[0].get::<Self>().expect("emitted by a ColorWheel"));
            None
        });
    }
}

/// Where the ring and the square are in the widget.
struct Geometry {
    cx: f32,
    cy: f32,
    radius: f32,
}

impl Geometry {
    fn of(widget: &impl IsA<gtk::Widget>) -> Self {
        let (w, h) = (widget.width() as f32, widget.height() as f32);
        Self {
            cx: w / 2.0,
            cy: h / 2.0,
            radius: w.min(h) / 2.0 - 1.0,
        }
    }

    /// The square inside the ring.
    fn square(&self) -> graphene::Rect {
        let half = (self.radius - RING - GAP) / std::f32::consts::SQRT_2;
        graphene::Rect::new(self.cx - half, self.cy - half, 2.0 * half, 2.0 * half)
    }

    fn part_at(&self, x: f32, y: f32) -> Option<Part> {
        let d = (x - self.cx).hypot(y - self.cy);
        if d <= self.radius && d >= self.radius - RING - GAP / 2.0 {
            return Some(Part::Ring);
        }
        let sq = self.square().inset_r(-MARKER, -MARKER);
        sq.contains_point(&graphene::Point::new(x, y))
            .then_some(Part::Square)
    }
}

fn marker(snapshot: &gtk::Snapshot, x: f32, y: f32) {
    let path = gsk::PathBuilder::new();
    path.add_circle(&graphene::Point::new(x, y), MARKER);
    let path = path.to_path();
    snapshot.append_stroke(
        &path,
        &gsk::Stroke::new(4.0),
        &gdk::RGBA::new(0.0, 0.0, 0.0, 0.35),
    );
    snapshot.append_stroke(&path, &gsk::Stroke::new(2.0), &gdk::RGBA::WHITE);
}

fn hsv_rgba(h: f32, s: f32, v: f32) -> gdk::RGBA {
    let c = hsv_to_rgb(h, s, v);
    gdk::RGBA::new(
        f32::from(c.r) / 255.0,
        f32::from(c.g) / 255.0,
        f32::from(c.b) / 255.0,
        1.0,
    )
}

/// Hue in degrees, saturation and value 0–1, to an opaque colour.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> Rgba8 {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let byte = |f: f32| ((f + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgba8::new(byte(r), byte(g), byte(b), 255)
}

fn rgb_to_hsv(c: Rgba8) -> (f32, f32, f32) {
    let (r, g, b) = (
        f32::from(c.r) / 255.0,
        f32::from(c.g) / 255.0,
        f32::from(c.b) / 255.0,
    );
    let max = r.max(g).max(b);
    let d = max - r.min(g).min(b);
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let s = if max == 0.0 { 0.0 } else { d / max };
    (h, s, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_round_trip() {
        for c in [
            Rgba8::new(30, 30, 30, 255),
            Rgba8::new(28, 113, 216, 255),
            Rgba8::new(192, 28, 40, 255),
            Rgba8::new(255, 255, 255, 255),
            Rgba8::new(0, 0, 0, 255),
            Rgba8::new(255, 0, 255, 255),
            Rgba8::new(246, 211, 45, 255),
        ] {
            let (h, s, v) = rgb_to_hsv(c);
            assert_eq!(hsv_to_rgb(h, s, v), c, "{c:?} → {h} {s} {v}");
        }
    }

    #[test]
    fn primaries() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), Rgba8::new(255, 0, 0, 255));
        assert_eq!(hsv_to_rgb(120.0, 1.0, 1.0), Rgba8::new(0, 255, 0, 255));
        assert_eq!(hsv_to_rgb(240.0, 1.0, 1.0), Rgba8::new(0, 0, 255, 255));
        assert_eq!(hsv_to_rgb(360.0, 1.0, 1.0), Rgba8::new(255, 0, 0, 255));
    }
}
