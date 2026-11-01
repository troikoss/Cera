// SPDX-License-Identifier: GPL-3.0-or-later
//! Drawing input (DESIGN.md §6). The stylus draws with the selected tool; its eraser end and
//! barrel button erase. The mouse draws at a fixed pressure and pans with the middle button.
//! Touch is left to the scrolled window (pan) and the pinch gesture.
//!
//! On pen-up the samples are simplified into a `Stroke` and added through the note's
//! `History`; an eraser drag removes whole strokes and undoes as one step.

use std::cell::Cell;
use std::sync::Arc;

use cera_core::{Point, Rect, Rgba8, Stroke, Tool, simplify};
use cera_render::ink;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib};

use super::Canvas;
use super::wet::{Pen, WetStroke};
use crate::note::SharedNote;

/// Most colour swatches and size slots the tool bar shows (a preference, 3 by default).
pub const MAX_COLOR_SLOTS: usize = 6;
pub const MAX_SIZE_SLOTS: usize = 5;
/// Default full-pressure widths of the size slots, in points; the first three are shown.
pub const PEN_WIDTHS: [f32; MAX_SIZE_SLOTS] = [0.8, 1.6, 3.2, 5.0, 8.0];
pub const HIGHLIGHTER_WIDTHS: [f32; MAX_SIZE_SLOTS] = [8.0, 12.0, 20.0, 28.0, 36.0];
/// Default eraser radii of the size slots, in logical pixels (the eraser works on screen,
/// whatever the zoom).
pub const ERASER_WIDTHS: [f32; MAX_SIZE_SLOTS] = [5.0, 10.0, 20.0, 32.0, 48.0];
/// What a size slot can be set to: points for the pen and highlighter, logical pixels of
/// radius for the eraser.
pub const PEN_WIDTH_RANGE: (f32, f32) = (0.2, 10.0);
pub const HIGHLIGHTER_WIDTH_RANGE: (f32, f32) = (2.0, 40.0);
pub const ERASER_WIDTH_RANGE: (f32, f32) = (3.0, 60.0);
/// Pressure for devices without one (the mouse), and for the pen with pressure turned off.
const MOUSE_PRESSURE: f64 = 0.6;
/// Stroke simplification on pen-up: position (points) and pressure tolerances.
const SIMPLIFY_EPS: f32 = 0.05;
const SIMPLIFY_PRESSURE_EPS: f32 = 0.03;

/// Default swatch colours; highlighters are translucent.
pub const PEN_COLORS: [Rgba8; MAX_COLOR_SLOTS] = [
    Rgba8::new(30, 30, 30, 255),
    Rgba8::new(28, 113, 216, 255),
    Rgba8::new(192, 28, 40, 255),
    Rgba8::new(38, 162, 105, 255),
    Rgba8::new(230, 97, 0, 255),
    Rgba8::new(129, 61, 156, 255),
];
pub const HIGHLIGHTER_ALPHA: u8 = 90;
pub const HIGHLIGHTER_COLORS: [Rgba8; MAX_COLOR_SLOTS] = [
    Rgba8::new(246, 211, 45, HIGHLIGHTER_ALPHA),
    Rgba8::new(87, 227, 137, HIGHLIGHTER_ALPHA),
    Rgba8::new(246, 97, 81, HIGHLIGHTER_ALPHA),
    Rgba8::new(98, 160, 234, HIGHLIGHTER_ALPHA),
    Rgba8::new(220, 138, 221, HIGHLIGHTER_ALPHA),
    Rgba8::new(255, 163, 72, HIGHLIGHTER_ALPHA),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Pen,
    Highlighter,
    Eraser,
    Lasso,
}

impl ToolKind {
    pub fn name(self) -> &'static str {
        match self {
            ToolKind::Pen => "pen",
            ToolKind::Highlighter => "highlighter",
            ToolKind::Eraser => "eraser",
            ToolKind::Lasso => "lasso",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [
            ToolKind::Pen,
            ToolKind::Highlighter,
            ToolKind::Eraser,
            ToolKind::Lasso,
        ]
        .into_iter()
        .find(|t| t.name() == name)
    }
}

/// The selected tool and its settings, shared by every canvas in a window.
#[derive(Debug)]
pub struct Tools {
    pub tool: Cell<ToolKind>,
    pub pen_color: Cell<Rgba8>,
    pub highlighter_color: Cell<Rgba8>,
    pub pen_width: Cell<f32>,
    pub highlighter_width: Cell<f32>,
    /// Eraser radius in logical pixels; also used by the stylus's eraser end and barrel.
    pub eraser_width: Cell<f32>,
    /// The pen's width follows the stylus pressure.
    pub pressure: Cell<bool>,
    /// The lasso selects with a rectangle instead of a free loop.
    pub rect_select: Cell<bool>,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            tool: Cell::new(ToolKind::Pen),
            pen_color: Cell::new(PEN_COLORS[0]),
            highlighter_color: Cell::new(HIGHLIGHTER_COLORS[0]),
            pen_width: Cell::new(PEN_WIDTHS[1]),
            highlighter_width: Cell::new(HIGHLIGHTER_WIDTHS[1]),
            eraser_width: Cell::new(ERASER_WIDTHS[1]),
            pressure: Cell::new(true),
            rect_select: Cell::new(false),
        }
    }
}

impl Tools {
    /// The colour of the pen or highlighter, whichever is selected (the pen's otherwise).
    pub fn color(&self) -> Rgba8 {
        match self.tool.get() {
            ToolKind::Highlighter => self.highlighter_color.get(),
            _ => self.pen_color.get(),
        }
    }

    pub fn set_color(&self, color: Rgba8) {
        match self.tool.get() {
            ToolKind::Highlighter => self.highlighter_color.set(color),
            ToolKind::Pen => self.pen_color.set(color),
            ToolKind::Eraser | ToolKind::Lasso => {}
        }
    }

    /// Width of the selected tool (the pen's for the lasso).
    pub fn width(&self) -> f32 {
        match self.tool.get() {
            ToolKind::Highlighter => self.highlighter_width.get(),
            ToolKind::Eraser => self.eraser_width.get(),
            _ => self.pen_width.get(),
        }
    }

    pub fn set_width(&self, width: f32) {
        match self.tool.get() {
            ToolKind::Highlighter => self.highlighter_width.set(width),
            ToolKind::Pen => self.pen_width.set(width),
            ToolKind::Eraser => self.eraser_width.set(width),
            ToolKind::Lasso => {}
        }
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
    fn tool(&self) -> ToolKind {
        self.tools().tool.get()
    }

    pub(super) fn setup_drawing_input(&self) {
        let stylus = gtk::GestureStylus::new();
        stylus.set_stylus_only(true);
        stylus.connect_down(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, x, y| {
                let event = g.current_event();
                let buttons = event
                    .as_ref()
                    .map_or(gdk::ModifierType::empty(), |e| e.modifier_state());
                let time = event.map_or(0, |e| e.time());
                let button = button(g);
                // GestureStylus reports every button as down/up, so a barrel press arrives
                // here too; only the tip starts a stroke.
                if button != gdk::BUTTON_PRIMARY {
                    let tip_down = buttons.contains(gdk::ModifierType::BUTTON1_MASK);
                    tracing::debug!(button, tip_down, "stylus button down");
                    // With the tip down the press resets the gesture, and `cancel` below has
                    // committed the ink so far, which stays; the rest of the stroke erases.
                    if tip_down && is_barrel(button) {
                        canvas.erase_from(Sample {
                            x,
                            y,
                            pressure: 1.0,
                            time,
                        });
                    }
                    return;
                }
                g.set_state(gtk::EventSequenceState::Claimed);
                let eraser_end = g
                    .device_tool()
                    .is_some_and(|t| t.tool_type() == gdk::DeviceToolType::Eraser);
                let barrel = buttons
                    .intersects(gdk::ModifierType::BUTTON2_MASK | gdk::ModifierType::BUTTON3_MASK);
                tracing::debug!(eraser_end, barrel, "stylus tip down");
                let sample = Sample {
                    x,
                    y,
                    pressure: canvas.pressure(g.axis(gdk::AxisUse::Pressure)),
                    time,
                };
                canvas.begin(sample, eraser_end || barrel);
            }
        ));
        stylus.connect_motion(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, x, y| {
                let mut samples = stylus_samples(g, x, y);
                if !canvas.tools().pressure.get() {
                    samples.iter_mut().for_each(|s| s.pressure = MOUSE_PRESSURE);
                }
                canvas.extend(&samples);
            }
        ));
        stylus.connect_up(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, _, _| {
                if button(g) == gdk::BUTTON_PRIMARY {
                    canvas.end();
                }
            }
        ));
        // A barrel press mid-stroke resets the gesture (GestureSingle tracks one button);
        // finish the stroke rather than leave it hanging. Motion and up still arrive.
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

        // A right click opens the context menu: at the selection if it is on it.
        let context = gtk::GestureClick::new();
        context.set_button(gdk::BUTTON_SECONDARY);
        context.connect_pressed(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, _, x, y| {
                let mouse = g.current_event().is_some_and(|e| {
                    e.device_tool().is_none()
                        && matches!(
                            e.device().map(|d| d.source()),
                            Some(gdk::InputSource::Mouse | gdk::InputSource::Touchpad)
                        )
                });
                if !mouse {
                    // The stylus's barrel button erases instead.
                    g.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                g.set_state(gtk::EventSequenceState::Claimed);
                let on_selection = canvas
                    .selection_view_rect()
                    .filter(|&(rx, ry, w, h)| x >= rx && x <= rx + w && y >= ry && y <= ry + h);
                canvas.emit_context_menu(on_selection.unwrap_or((x, y, 1.0, 1.0)));
            }
        ));
        self.add_controller(context);

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
        imp.drawn.take();
        let Some(note) = imp.note.borrow().clone() else {
            return;
        };
        if !eraser && self.tool() == ToolKind::Lasso {
            self.lasso_begin(s.x, s.y);
            return;
        }
        self.clear_selection();
        if eraser || self.tool() == ToolKind::Eraser {
            self.begin_erase(&note, s);
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
        let tools = self.tools();
        let pen = Pen {
            tool: if tools.tool.get() == ToolKind::Highlighter {
                Tool::Highlighter
            } else {
                Tool::Pen
            },
            color: tools.color(),
            width: tools.width(),
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

    /// The barrel button pressed with the tip down: the ink so far stays and the rest of the
    /// stroke erases other strokes.
    fn erase_from(&self, s: Sample) {
        let imp = self.imp();
        if imp.erasing.get() {
            return;
        }
        self.end();
        self.clear_selection();
        let Some(note) = imp.note.borrow().clone() else {
            return;
        };
        self.begin_erase(&note, s);
    }

    /// An eraser drag starts; its removals undo as one step.
    fn begin_erase(&self, note: &SharedNote, s: Sample) {
        note.borrow_mut().history.begin_group();
        self.imp().erasing.set(true);
        self.erase_at(s);
    }

    fn extend(&self, samples: &[Sample]) {
        let imp = self.imp();
        if imp.erasing.get() {
            for &s in samples {
                self.erase_at(s);
            }
            return;
        }
        if self.lasso_active() {
            for s in samples {
                self.lasso_extend(s.x, s.y);
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
        if self.lasso_active() {
            self.lasso_end();
            return;
        }
        let Some(active) = imp.active.take() else {
            return;
        };
        self.commit(active);
    }

    /// Drops what a touch started (a stroke, an erase, a loop) when it turns into a pinch.
    fn cancel(&self) {
        let imp = self.imp();
        if imp.erasing.replace(false) {
            if let Some(note) = imp.note.borrow().as_ref() {
                note.borrow_mut().history.end_group();
            }
            imp.eraser_cursor.set(None);
            self.emit_by_name::<()>("changed", &[]);
        }
        imp.active.take();
        self.lasso_cancel();
        self.queue_draw();
    }

    /// One-finger drawing ("draw with finger"): touches arrive here instead of panning.
    pub fn touch_begin(&self, x: f64, y: f64, time: u32) {
        let pressure = MOUSE_PRESSURE;
        self.begin(
            Sample {
                x,
                y,
                pressure,
                time,
            },
            false,
        );
    }

    pub fn touch_extend(&self, x: f64, y: f64, time: u32) {
        let pressure = MOUSE_PRESSURE;
        self.extend(&[Sample {
            x,
            y,
            pressure,
            time,
        }]);
    }

    pub fn touch_end(&self) {
        self.end();
    }

    pub fn touch_cancel(&self) {
        self.cancel();
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
            history.add_strokes(doc, wet.page, vec![stroke.clone()])
        };
        if let Err(e) = added {
            tracing::warn!("cannot add stroke: {e}");
            return;
        }
        imp.drawn.replace(Some(stroke));
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
        let radius = (f64::from(self.tools().eraser_width.get()) / view.zoom) as f32;
        let (x, y) = view.to_pt(s.x, s.y);
        let drawn = imp.drawn.borrow();
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
                .filter(|&i| !drawn.as_ref().is_some_and(|d| Arc::ptr_eq(d, &strokes[i])))
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
        let line = move |row: f64, wave: f64| -> Vec<Sample> {
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
        let tools = self.tools();
        let tool = tools.tool.replace(ToolKind::Highlighter);
        stroke(self, line(1.5, 0.0), false);
        tools.tool.set(tool);
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
                // The barrel pressed mid-stroke (the gesture's cancel, then its down): the line
                // so far stays, though the eraser starts on it.
                let barrel = line(10.0, 8.0);
                canvas.begin(barrel[0], false);
                canvas.extend(&barrel[1..30]);
                canvas.end();
                canvas.erase_from(barrel[30]);
                canvas.extend(&barrel[31..]);
                canvas.end();
                tracing::info!("demo: drew a line and pressed the barrel");
                // Long enough to take a screenshot of it (tools/headless).
                glib::timeout_add_local_once(std::time::Duration::from_millis(3000), move || {
                    canvas.demo_scroll();
                });
            });
        });
    }

    /// Scrolls down at a steady 4000 px/s for 5 s, one step per frame, to measure frame times
    /// while tiles stream in (`CERA_DEBUG=1` logs them).
    fn demo_scroll(&self) {
        let Some(adj) = self.adjustment(gtk::Orientation::Vertical) else {
            return;
        };
        tracing::info!("demo: scrolling");
        let start = std::cell::Cell::new(None::<i64>);
        let from = adj.value();
        self.add_tick_callback(move |canvas, clock| {
            let now = clock.frame_time();
            let t0 = start.get().unwrap_or(now);
            start.set(Some(t0));
            let seconds = (now - t0) as f64 / 1e6;
            adj.set_value(from + seconds * 4000.0);
            if seconds >= 5.0 || adj.value() >= adj.upper() - adj.page_size() {
                tracing::info!(page = canvas.current_page() + 1, "demo: done");
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }
}

impl Canvas {
    /// The stylus pressure to draw with, or a fixed one when pressure is turned off.
    fn pressure(&self, axis: Option<f64>) -> f64 {
        if self.tools().pressure.get() {
            axis.unwrap_or(1.0)
        } else {
            MOUSE_PRESSURE
        }
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

/// The button of the stylus down or up event: 1 is the tip.
fn button(g: &gtk::GestureStylus) -> u32 {
    g.current_event()
        .and_then(|e| e.downcast::<gdk::ButtonEvent>().ok())
        .map_or(gdk::BUTTON_PRIMARY, |b| b.button())
}

/// GDK reports the barrel buttons (`BTN_STYLUS`, `BTN_STYLUS2`) as middle and right; the
/// S Pen's arrives as right under GNOME 50.
fn is_barrel(button: u32) -> bool {
    button == gdk::BUTTON_MIDDLE || button == gdk::BUTTON_SECONDARY
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
