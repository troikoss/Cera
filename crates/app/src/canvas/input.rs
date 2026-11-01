// SPDX-License-Identifier: GPL-3.0-or-later
//! Drawing input (DESIGN.md §6). The stylus draws with the selected tool; its eraser end and
//! barrel button use the secondary preset (the eraser by default) or the lasso. The mouse
//! draws at a fixed pressure and pans with the middle button. Touch is left to the scrolled
//! window (pan) and the pinch gesture.
//!
//! On pen-up the samples are simplified into a `Stroke` and added through the note's
//! `History`; an eraser drag removes whole strokes and undoes as one step.

use std::cell::Cell;
use std::sync::Arc;

use cera_core::{Point, Rect, Stroke, simplify};
use cera_render::ink;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib};

use super::Canvas;
use super::wet::{Pen, WetStroke};
use crate::note::SharedNote;
use crate::preset::{Preset, PresetId, Secondary};
use crate::settings::{self, SelectShape};

/// Pressure for devices without one (the mouse and fingers).
const MOUSE_PRESSURE: f64 = 0.6;
/// Stroke simplification on pen-up: position (points) and pressure tolerances.
const SIMPLIFY_EPS: f32 = 0.05;
const SIMPLIFY_PRESSURE_EPS: f32 = 0.03;

/// A tool bar tool: a preset, or the lasso.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Preset(PresetId),
    Lasso,
}

impl ToolKind {
    /// The tool bar toggle's name.
    pub fn name(self) -> String {
        match self {
            ToolKind::Preset(id) => format!("preset-{id}"),
            ToolKind::Lasso => "lasso".to_owned(),
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        if name == "lasso" {
            return Some(ToolKind::Lasso);
        }
        name.strip_prefix("preset-")?
            .parse()
            .ok()
            .map(ToolKind::Preset)
    }

    pub fn preset(self) -> Option<PresetId> {
        match self {
            ToolKind::Preset(id) => Some(id),
            ToolKind::Lasso => None,
        }
    }
}

impl From<Secondary> for ToolKind {
    fn from(s: Secondary) -> Self {
        match s {
            Secondary::Preset(id) => ToolKind::Preset(id),
            Secondary::Lasso => ToolKind::Lasso,
        }
    }
}

/// The selected tool, shared by every canvas in a window. The presets themselves are in the
/// settings.
#[derive(Debug)]
pub struct Tools {
    pub tool: Cell<ToolKind>,
    /// What the lasso draws to select.
    pub select_shape: Cell<SelectShape>,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            tool: Cell::new(ToolKind::Preset(0)),
            select_shape: Cell::new(SelectShape::Lasso),
        }
    }
}

impl Tools {
    /// The selected preset, if a preset (not the lasso) is selected and still exists.
    pub fn preset(&self) -> Option<Preset> {
        let id = self.tool.get().preset()?;
        settings::with(|s| s.preset(id).cloned())
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
                        canvas.secondary_from(Sample {
                            x,
                            y,
                            pressure: 1.0,
                            time,
                        });
                    }
                    return;
                }
                g.set_state(gtk::EventSequenceState::Claimed);
                canvas.imp().input_source.set(super::InputSource::Stylus);
                let eraser_end = g
                    .device_tool()
                    .is_some_and(|t| t.tool_type() == gdk::DeviceToolType::Eraser);
                let barrel = buttons
                    .intersects(gdk::ModifierType::BUTTON2_MASK | gdk::ModifierType::BUTTON3_MASK);
                tracing::debug!(eraser_end, barrel, "stylus tip down");
                let sample = Sample {
                    x,
                    y,
                    pressure: g.axis(gdk::AxisUse::Pressure).unwrap_or(1.0),
                    time,
                };
                canvas.begin(sample, eraser_end || barrel);
            }
        ));
        stylus.connect_motion(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, x, y| {
                canvas.extend(&stylus_samples(g, x, y));
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
                canvas.imp().input_source.set(super::InputSource::Mouse);
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

        // A right click opens the context menu at the pointer, with any tool.
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
                    // The stylus's barrel button is the secondary action instead.
                    g.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                g.set_state(gtk::EventSequenceState::Claimed);
                canvas.context_menu_at(x, y, super::InputSource::Mouse);
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

    /// A press starts: with the selected tool, or the secondary action (the stylus's eraser
    /// end or barrel button).
    fn begin(&self, s: Sample, secondary: bool) {
        let imp = self.imp();
        imp.drawn.take();
        imp.on_secondary.set(secondary);
        let Some(note) = imp.note.borrow().clone() else {
            return;
        };
        // With any tool, the stylus or a finger held still opens the context menu.
        imp.press.set((s.x, s.y));
        if imp.input_source.get() != super::InputSource::Mouse {
            self.start_hold(s.x, s.y);
        }
        let tool = if secondary {
            settings::with(|st| st.secondary).into()
        } else {
            self.tool()
        };
        let preset = match tool {
            ToolKind::Lasso => {
                self.lasso_begin(s.x, s.y);
                return;
            }
            ToolKind::Preset(id) => settings::with(|st| st.preset(id).cloned()),
        };
        let Some(preset) = preset else { return };
        self.clear_selection();
        if preset.role.is_eraser() {
            self.begin_erase(&note, s, preset.width);
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
        let pen = Pen {
            style: preset.stroke_style(),
            color: preset.ink_color(),
            width: preset.width,
            constant: !preset.pressure,
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
    /// stroke uses the secondary action. An eraser leaves the ink so far alone.
    fn secondary_from(&self, s: Sample) {
        let imp = self.imp();
        if imp.on_secondary.get() {
            return;
        }
        self.end();
        let eraser = match settings::with(|st| st.secondary) {
            Secondary::Preset(id) => settings::with(|st| {
                st.preset(id)
                    .filter(|p| p.role.is_eraser())
                    .map(|p| p.width)
            }),
            Secondary::Lasso => None,
        };
        let Some(radius) = eraser else {
            self.begin(s, true);
            return;
        };
        imp.on_secondary.set(true);
        self.clear_selection();
        let Some(note) = imp.note.borrow().clone() else {
            return;
        };
        self.begin_erase(&note, s, radius);
    }

    /// An eraser drag starts with a radius in logical pixels; its removals undo as one step.
    fn begin_erase(&self, note: &SharedNote, s: Sample, radius: f32) {
        note.borrow_mut().history.begin_group();
        let imp = self.imp();
        imp.erasing.set(true);
        imp.eraser_radius.set(radius);
        self.erase_at(s);
    }

    fn extend(&self, samples: &[Sample]) {
        let imp = self.imp();
        if let Some(s) = samples.last() {
            self.hold_moved(s.x, s.y);
        }
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
        self.cancel_hold();
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

    /// Drops what a press started (a stroke, an erase, a loop) when it turns into a pinch
    /// or a hold.
    pub(super) fn cancel(&self) {
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
        self.imp().input_source.set(super::InputSource::Touch);
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
        let stroke = Arc::new(Stroke::new(pen.style, pen.color, pen.width, points));
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
        let radius = (f64::from(imp.eraser_radius.get()) / view.zoom) as f32;
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
        let highlighter = settings::with(|s| {
            s.presets
                .iter()
                .find(|p| matches!(p.role, crate::preset::Role::Highlighter { .. }))
                .map(|p| p.id)
        });
        if let Some(id) = highlighter {
            let tool = tools.tool.replace(ToolKind::Preset(id));
            stroke(self, line(1.5, 0.0), false);
            tools.tool.set(tool);
        }
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
                canvas.secondary_from(barrel[30]);
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
