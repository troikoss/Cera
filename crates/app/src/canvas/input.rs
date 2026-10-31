//! Drawing input (DESIGN.md §6). The stylus draws with the selected tool; its eraser end and
//! barrel button erase. The mouse draws at a fixed pressure and pans with the middle button.
//! Touch is left to the scrolled window (pan) and the pinch gesture.
//!
//! On pen-up the samples are simplified into a `Stroke` and added through the note's
//! `History`; an eraser drag removes whole strokes and undoes as one step.

use std::sync::Arc;

use cera_core::{Point, Rect, Rgba8, Stroke, Tool, simplify};
use cera_render::ink;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib};

use super::Canvas;
use super::tiles::ERASER_RADIUS;
use super::wet::{Pen, WetStroke};

/// Full-pressure widths in points.
const PEN_WIDTH: f32 = 1.6;
const HIGHLIGHTER_WIDTH: f32 = 12.0;
/// Pressure for devices without one (the mouse).
const MOUSE_PRESSURE: f64 = 0.6;
/// Stroke simplification on pen-up: position (points) and pressure tolerances.
const SIMPLIFY_EPS: f32 = 0.05;
const SIMPLIFY_PRESSURE_EPS: f32 = 0.03;

/// Swatch colours; highlighters are translucent.
pub const PEN_COLORS: [Rgba8; 3] = [
    Rgba8::new(30, 30, 30, 255),
    Rgba8::new(28, 113, 216, 255),
    Rgba8::new(192, 28, 40, 255),
];
pub const HIGHLIGHTER_COLORS: [Rgba8; 3] = [
    Rgba8::new(246, 211, 45, 90),
    Rgba8::new(87, 227, 137, 90),
    Rgba8::new(246, 97, 81, 90),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Pen,
    Highlighter,
    Eraser,
}

impl ToolKind {
    pub fn name(self) -> &'static str {
        match self {
            ToolKind::Pen => "pen",
            ToolKind::Highlighter => "highlighter",
            ToolKind::Eraser => "eraser",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [ToolKind::Pen, ToolKind::Highlighter, ToolKind::Eraser]
            .into_iter()
            .find(|t| t.name() == name)
    }
}

/// One input sample in widget coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Sample {
    x: f64,
    y: f64,
    pressure: f64,
    /// Event time in milliseconds.
    time: u32,
}

/// The stroke being drawn.
pub(super) struct Active {
    pub wet: WetStroke,
    /// Page origin in stack points, to turn samples into page coordinates.
    origin: (f64, f64),
    last: Sample,
}

impl Canvas {
    pub fn tool(&self) -> ToolKind {
        self.imp().tool.get()
    }

    pub fn set_tool(&self, tool: ToolKind) {
        if self.imp().tool.replace(tool) != tool {
            self.notify("tool");
        }
    }

    /// The colour of the pen or highlighter, whichever is selected (the pen's for the eraser).
    pub fn color(&self) -> Rgba8 {
        match self.tool() {
            ToolKind::Highlighter => self.imp().highlighter_color.get(),
            _ => self.imp().pen_color.get(),
        }
    }

    pub fn set_color(&self, color: Rgba8) {
        match self.tool() {
            ToolKind::Highlighter => self.imp().highlighter_color.set(color),
            ToolKind::Pen => self.imp().pen_color.set(color),
            ToolKind::Eraser => {}
        }
    }

    pub(super) fn setup_drawing_input(&self) {
        let stylus = gtk::GestureStylus::new();
        stylus.set_stylus_only(true);
        stylus.connect_down(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, x, y| {
                // GestureStylus reports every button as down/up, so a barrel press while
                // hovering arrives here too; only the tip starts a stroke.
                if !is_tip(g) {
                    return;
                }
                g.set_state(gtk::EventSequenceState::Claimed);
                let event = g.current_event();
                let eraser_end = g
                    .device_tool()
                    .is_some_and(|t| t.tool_type() == gdk::DeviceToolType::Eraser);
                let barrel = event.as_ref().is_some_and(|e| {
                    e.modifier_state().intersects(
                        gdk::ModifierType::BUTTON2_MASK | gdk::ModifierType::BUTTON3_MASK,
                    )
                });
                let sample = Sample {
                    x,
                    y,
                    pressure: g.axis(gdk::AxisUse::Pressure).unwrap_or(1.0),
                    time: event.map_or(0, |e| e.time()),
                };
                canvas.begin(sample, eraser_end || barrel);
            }
        ));
        stylus.connect_motion(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, x, y| canvas.extend(&stylus_samples(g, x, y))
        ));
        stylus.connect_up(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, _, _| {
                if is_tip(g) {
                    canvas.end();
                }
            }
        ));
        // A barrel press mid-stroke can reset the gesture; finish the stroke rather than
        // leave it hanging.
        stylus.connect_cancel(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |_, _| canvas.end()
        ));
        self.add_controller(stylus);

        let mouse = gtk::GestureDrag::new();
        mouse.set_button(gdk::BUTTON_PRIMARY);
        mouse.connect_drag_begin(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, x, y| {
                if !from_mouse(g) {
                    g.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                let time = g.current_event().map_or(0, |e| e.time());
                canvas.begin(
                    Sample {
                        x,
                        y,
                        pressure: MOUSE_PRESSURE,
                        time,
                    },
                    false,
                );
            }
        ));
        mouse.connect_drag_update(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, dx, dy| {
                let Some((x, y)) = g.start_point() else {
                    return;
                };
                let time = g.current_event().map_or(0, |e| e.time());
                canvas.extend(&[Sample {
                    x: x + dx,
                    y: y + dy,
                    pressure: MOUSE_PRESSURE,
                    time,
                }]);
            }
        ));
        mouse.connect_drag_end(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |_, _, _| canvas.end()
        ));
        self.add_controller(mouse);

        // Middle-drag pans.
        let pan = gtk::GestureDrag::new();
        pan.set_button(gdk::BUTTON_MIDDLE);
        let start = std::rc::Rc::new(std::cell::Cell::new((0.0, 0.0)));
        pan.connect_drag_begin(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            #[strong]
            start,
            move |g, _, _| {
                if !from_mouse(g) {
                    // The S Pen's barrel button can arrive as a middle click.
                    g.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                start.set(canvas.scroll());
            }
        ));
        pan.connect_drag_update(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |_, dx, dy| {
                let (sx, sy) = start.get();
                canvas.configure_adjustments(Some((sx - dx, sy - dy)));
            }
        ));
        self.add_controller(pan);
    }

    fn begin(&self, s: Sample, eraser: bool) {
        let imp = self.imp();
        let Some(note) = imp.note.borrow().clone() else {
            return;
        };
        if eraser || self.tool() == ToolKind::Eraser {
            note.borrow_mut().history.begin_group();
            imp.erasing.set(true);
            self.erase_at(s);
            return;
        }
        let Some(view) = self.view() else { return };
        let (x, y) = view.to_pt(s.x, s.y);
        let page = {
            let doc = imp.doc.borrow();
            let Some(doc) = doc.as_ref() else { return };
            doc.layout.pages_between(y, y).find_map(|i| {
                let r = doc.layout.page_rect(i);
                (x >= r.x && x <= r.x + r.w).then(|| (i, doc.pages[i], (r.x, r.y)))
            })
        };
        let Some((index, (id, spec), origin)) = page else {
            return;
        }; // between pages
        if let Err(e) = note.borrow_mut().doc.load_ink(index) {
            tracing::warn!(page = id, "cannot read ink: {e}");
            return;
        }
        let tool = self.tool();
        let pen = Pen {
            tool: if tool == ToolKind::Highlighter {
                Tool::Highlighter
            } else {
                Tool::Pen
            },
            color: self.color(),
            width: if tool == ToolKind::Highlighter {
                HIGHLIGHTER_WIDTH
            } else {
                PEN_WIDTH
            },
        };
        let mut wet = WetStroke::new(id, (spec.width, spec.height), pen, self.target_scale());
        wet.extend(&[page_point(s, (x, y), origin)]);
        wet.flush();
        *imp.active.borrow_mut() = Some(Active {
            wet,
            origin,
            last: s,
        });
        self.queue_draw();
    }

    fn extend(&self, samples: &[Sample]) {
        let imp = self.imp();
        if imp.erasing.get() {
            for &s in samples {
                self.erase_at(s);
            }
            return;
        }
        let Some(view) = self.view() else { return };
        let mut active = imp.active.borrow_mut();
        let Some(a) = active.as_mut() else { return };
        let mut points = Vec::with_capacity(samples.len());
        for &s in samples {
            // Skip samples GTK repeats (backlog overlapping the previous event).
            if s.time < a.last.time || (s.x, s.y) == (a.last.x, a.last.y) {
                continue;
            }
            points.push(page_point(s, view.to_pt(s.x, s.y), a.origin));
            a.last = s;
        }
        a.wet.extend(&points);
        a.wet.flush();
        drop(active);
        self.queue_draw();
    }

    fn end(&self) {
        let imp = self.imp();
        if imp.erasing.replace(false) {
            if let Some(note) = imp.note.borrow().as_ref() {
                note.borrow_mut().history.end_group();
            }
            imp.eraser_cursor.set(None);
            self.queue_draw();
            self.emit_by_name::<()>("changed", &[]);
            return;
        }
        let Some(active) = imp.active.take() else {
            return;
        };
        self.commit(active);
    }

    /// Simplifies the samples into a stroke, adds it to the note and asks for its tiles; the
    /// wet stroke stays on screen until they arrive.
    fn commit(&self, active: Active) {
        let imp = self.imp();
        let Some(note) = imp.note.borrow().clone() else {
            return;
        };
        let mut wet = active.wet;
        let pen = wet.pen;
        let points = simplify(&wet.points, SIMPLIFY_EPS, SIMPLIFY_PRESSURE_EPS);
        let stroke = Arc::new(Stroke::new(pen.tool, pen.color, pen.width, points));
        let area = stroke.bounds();
        let added = {
            let mut note = note.borrow_mut();
            let crate::note::Note { history, doc, .. } = &mut *note;
            history.add_strokes(doc, wet.page, vec![stroke])
        };
        if let Err(e) = added {
            tracing::warn!("cannot add stroke: {e}");
            return;
        }
        wet.generation = Some(self.invalidate_ink(wet.page, area));
        imp.wet.borrow_mut().push(wet);
        self.request_tiles();
        self.queue_draw();
        self.emit_by_name::<()>("changed", &[]);
    }

    /// Removes every stroke the eraser circle at `s` touches, on any page under it.
    fn erase_at(&self, s: Sample) {
        let imp = self.imp();
        imp.eraser_cursor.set(Some((s.x, s.y)));
        self.queue_draw();
        let (Some(note), Some(view)) = (imp.note.borrow().clone(), self.view()) else {
            return;
        };
        let radius = (ERASER_RADIUS / view.zoom) as f32;
        let (x, y) = view.to_pt(s.x, s.y);
        let pages: Vec<_> = {
            let doc = imp.doc.borrow();
            let Some(doc) = doc.as_ref() else { return };
            let r = f64::from(radius);
            doc.layout
                .pages_between(y - r, y + r)
                .map(|i| (i, doc.pages[i].0, doc.layout.page_rect(i)))
                .collect()
        };
        let mut erased = Vec::new();
        for (index, id, r) in pages {
            let at = ((x - r.x) as f32, (y - r.y) as f32);
            let mut note = note.borrow_mut();
            let crate::note::Note { history, doc, .. } = &mut *note;
            let Ok(strokes) = doc.load_ink(index) else {
                continue;
            };
            let hit: Vec<usize> = (0..strokes.len())
                .filter(|&i| ink::hits(&strokes[i], at, radius))
                .collect();
            let Some(area) = hit.iter().map(|&i| strokes[i].bounds()).reduce(Rect::union) else {
                continue;
            };
            if let Err(e) = history.remove_strokes(doc, id, &hit) {
                tracing::warn!("cannot erase: {e}");
                continue;
            }
            erased.push((id, area));
        }
        for (id, area) in &erased {
            self.invalidate_ink(*id, *area);
        }
        if !erased.is_empty() {
            self.request_tiles();
        }
    }
}

impl Canvas {
    /// `CERA_DEBUG_DEMO=1`: draws, erases and undoes through the real input path with
    /// synthetic samples, so the pipeline can be checked without a stylus or input injection.
    pub fn run_demo(&self) {
        let (w, h) = (f64::from(self.width()), f64::from(self.height()));
        let line = |row: f64, wave: f64| -> Vec<Sample> {
            (0..60)
                .map(|i| {
                    let t = f64::from(i);
                    Sample {
                        x: w * 0.3 + t * 5.0,
                        y: h * 0.3 + row * 40.0 + (t / 6.0).sin() * wave,
                        pressure: 0.3 + 0.6 * (t / 20.0).sin().abs(),
                        time: 1000 + i * 8,
                    }
                })
                .collect()
        };
        let stroke = |canvas: &Canvas, samples: Vec<Sample>, eraser: bool| {
            canvas.begin(samples[0], eraser);
            for chunk in samples[1..].chunks(4) {
                canvas.extend(chunk);
            }
            canvas.end();
        };
        for row in 0..4 {
            stroke(self, line(f64::from(row), 8.0), false);
        }
        self.set_tool(ToolKind::Highlighter);
        stroke(self, line(1.5, 0.0), false);
        self.set_tool(ToolKind::Pen);
        tracing::info!("demo: drew 5 strokes");
        let weak = self.downgrade();
        glib::timeout_add_local_once(std::time::Duration::from_millis(1000), move || {
            let Some(canvas) = weak.upgrade() else { return };
            let across: Vec<Sample> = (0..20)
                .map(|i| Sample {
                    x: w * 0.3 + 100.0,
                    y: h * 0.3 + 20.0 + f64::from(i) * 2.0,
                    pressure: 1.0,
                    time: 5000 + i * 8,
                })
                .collect();
            stroke(&canvas, across, true);
            tracing::info!("demo: erased across row 1");
            glib::timeout_add_local_once(std::time::Duration::from_millis(1000), move || {
                canvas.undo();
                tracing::info!("demo: undid the erase");
            });
        });
    }
}

fn page_point(s: Sample, (x, y): (f64, f64), origin: (f64, f64)) -> Point {
    Point {
        x: (x - origin.0) as f32,
        y: (y - origin.1) as f32,
        pressure: s.pressure.clamp(0.0, 1.0) as f32,
    }
}

/// The motion event's coalesced samples (GTK's backlog, in widget coordinates) plus its own.
fn stylus_samples(g: &gtk::GestureStylus, x: f64, y: f64) -> Vec<Sample> {
    use gtk::glib::translate::IntoGlib;
    let axis = |a: gdk::AxisUse| a.into_glib() as usize;
    let mut out: Vec<Sample> = g
        .backlog()
        .unwrap_or_default()
        .iter()
        .map(|c| {
            let axes = c.axes();
            let pressure = if c.flags().contains(gdk::AxisFlags::PRESSURE) {
                axes[axis(gdk::AxisUse::Pressure)]
            } else {
                1.0
            };
            Sample {
                x: axes[axis(gdk::AxisUse::X)],
                y: axes[axis(gdk::AxisUse::Y)],
                pressure,
                time: c.time(),
            }
        })
        .collect();
    out.push(Sample {
        x,
        y,
        pressure: g.axis(gdk::AxisUse::Pressure).unwrap_or(1.0),
        time: g.current_event().map_or(0, |e| e.time()),
    });
    out
}

/// Whether the event that started or ended the gesture is the tip (button 1), not a barrel
/// button.
fn is_tip(g: &gtk::GestureStylus) -> bool {
    g.current_event()
        .and_then(|e| e.downcast::<gdk::ButtonEvent>().ok())
        .is_none_or(|b| b.button() == gdk::BUTTON_PRIMARY)
}

/// Mouse, touchpad or trackpoint; the stylus and touch are handled elsewhere.
fn from_mouse(g: &gtk::GestureDrag) -> bool {
    let Some(event) = g.current_event() else {
        return false;
    };
    if event.device_tool().is_some() {
        return false;
    }
    matches!(
        event.device().map(|d| d.source()),
        Some(gdk::InputSource::Mouse | gdk::InputSource::Touchpad | gdk::InputSource::Trackpoint)
    )
}
