//! Finger gestures on the note area (DESIGN.md §6), after palm rejection has had its say:
//!
//! - A quick tap with two fingers undoes, with three redoes (setting "Undo with two fingers").
//! - "Draw with finger": one finger draws with the current tool, and two fingers scroll and
//!   zoom. Touches are then handled here and never reach the scrolled window. A second
//!   finger landing soon after the first turns a just-started stroke into a pinch.
//!
//! Without "draw with finger" touches pass through untouched: the scrolled window pans and
//! the canvas's own pinch gesture zooms.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib, graphene};

use crate::canvas::Canvas;
use crate::settings;

/// A tap: every finger lifted within this long (ms), none moving more than `TAP_SLOP` px.
const TAP_TIME: u32 = 300;
const TAP_SLOP: f64 = 12.0;
/// A second finger this soon (ms) after the first turns its stroke into a pinch.
const PINCH_GRACE: u32 = 250;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Idle,
    Drawing(usize),
    Pinching {
        start_distance: f64,
    },
    /// Further touches are swallowed until every finger is up.
    Ignoring,
}

struct Touch {
    start: (f64, f64),
    pos: (f64, f64),
}

struct State {
    touches: HashMap<usize, Touch>,
    most: usize,
    moved: bool,
    began: u32,
    mode: Mode,
}

/// The controller for the note overlay; `canvas` gives the canvas of the selected tab.
pub fn controller(canvas: impl Fn() -> Option<Canvas> + 'static) -> gtk::EventControllerLegacy {
    let state = Rc::new(RefCell::new(State {
        touches: HashMap::new(),
        most: 0,
        moved: false,
        began: 0,
        mode: Mode::Idle,
    }));
    let controller = gtk::EventControllerLegacy::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.connect_event(move |c, event| {
        use gdk::EventType as E;
        let kind = event.event_type();
        if !matches!(
            kind,
            E::TouchBegin | E::TouchUpdate | E::TouchEnd | E::TouchCancel
        ) {
            return glib::Propagation::Proceed;
        }
        let Some(canvas) = canvas() else {
            return glib::Propagation::Proceed;
        };
        let draw = settings::with(|s| s.draw_with_finger);
        let seq = event.event_sequence().as_ptr() as usize;
        let time = event.time();
        let pos = event
            .position()
            .and_then(|p| to_canvas(&canvas, p))
            .unwrap_or((0.0, 0.0));
        let mut st = state.borrow_mut();
        match kind {
            E::TouchBegin => {
                if st.touches.is_empty() {
                    st.most = 0;
                    st.moved = false;
                    st.began = time;
                    st.mode = Mode::Idle;
                }
                st.touches.insert(seq, Touch { start: pos, pos });
                st.most = st.most.max(st.touches.len());
                if draw {
                    let n = st.touches.len();
                    st.mode = match (st.mode, n) {
                        (Mode::Idle, 1) => {
                            canvas.touch_begin(pos.0, pos.1, time);
                            Mode::Drawing(seq)
                        }
                        (Mode::Drawing(_), 2) if time.wrapping_sub(st.began) < PINCH_GRACE => {
                            canvas.touch_cancel();
                            let (centre, distance) = pair(&st.touches);
                            canvas.pinch_begin(centre);
                            Mode::Pinching {
                                start_distance: distance.max(1.0),
                            }
                        }
                        (Mode::Drawing(s), _) => Mode::Drawing(s),
                        _ => Mode::Ignoring,
                    };
                }
            }
            E::TouchUpdate => {
                if let Some(t) = st.touches.get_mut(&seq) {
                    t.pos = pos;
                    if (pos.0 - t.start.0).hypot(pos.1 - t.start.1) > TAP_SLOP {
                        st.moved = true;
                    }
                }
                match st.mode {
                    Mode::Drawing(s) if s == seq => canvas.touch_extend(pos.0, pos.1, time),
                    Mode::Pinching { start_distance } if st.touches.len() >= 2 => {
                        let (centre, distance) = pair(&st.touches);
                        canvas.pinch_update(distance / start_distance, centre);
                    }
                    _ => {}
                }
            }
            _ => {
                st.touches.remove(&seq);
                match st.mode {
                    Mode::Drawing(s) if s == seq => {
                        if kind == E::TouchCancel {
                            canvas.touch_cancel();
                        } else {
                            canvas.touch_end();
                        }
                        st.mode = Mode::Ignoring;
                    }
                    Mode::Pinching { .. } => {
                        canvas.pinch_end();
                        st.mode = Mode::Ignoring;
                    }
                    _ => {}
                }
                if st.touches.is_empty() {
                    let quick = time.wrapping_sub(st.began) < TAP_TIME;
                    let tap = quick && !st.moved && kind == E::TouchEnd;
                    let action = match st.most {
                        2 if tap => Some("win.undo"),
                        3 if tap => Some("win.redo"),
                        _ => None,
                    };
                    st.mode = Mode::Idle;
                    drop(st);
                    if let Some(action) = action
                        && settings::with(|s| s.finger_taps)
                        && let Some(widget) = c.widget()
                    {
                        let _ = widget.activate_action(action, None);
                    }
                    return if draw {
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    };
                }
            }
        }
        if draw {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    controller
}

/// Centre and distance of the first two fingers.
fn pair(touches: &HashMap<usize, Touch>) -> ((f64, f64), f64) {
    let mut it = touches.values();
    let (Some(a), Some(b)) = (it.next(), it.next()) else {
        let p = touches.values().next().map_or((0.0, 0.0), |t| t.pos);
        return (p, 1.0);
    };
    let centre = ((a.pos.0 + b.pos.0) / 2.0, (a.pos.1 + b.pos.1) / 2.0);
    (centre, (a.pos.0 - b.pos.0).hypot(a.pos.1 - b.pos.1))
}

/// Surface coordinates (as touch events carry them) → the canvas's own.
fn to_canvas(canvas: &Canvas, (x, y): (f64, f64)) -> Option<(f64, f64)> {
    let native = canvas.native()?;
    let (tx, ty) = native.surface_transform();
    let point = graphene::Point::new((x - tx) as f32, (y - ty) as f32);
    let p = native.compute_point(canvas, &point)?;
    Some((f64::from(p.x()), f64::from(p.y())))
}
