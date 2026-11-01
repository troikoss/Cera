// SPDX-License-Identifier: GPL-3.0-or-later
//! Palm rejection (DESIGN.md §6): while the pen is near the screen, and for 500 ms after it was
//! last seen, new touches are swallowed so a resting hand neither scrolls nor zooms.
//!
//! A capture-phase controller on a parent of the scrolled window sees touches before the
//! scrolled window's own pan gesture does. Whole touch sequences are kept or swallowed, never
//! split, so no gesture is left half-started. Touches on a popover (the context menu) are
//! never swallowed.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};

/// Touches within this long after the last pen event are rejected (ms).
const AFTER_PEN: u32 = 500;
/// Between proximity-in and -out the pen counts as near, but only this long without any pen
/// event, in case a proximity-out never arrives (ms).
const HOVER_LIMIT: u32 = 5000;

pub fn controller() -> gtk::EventControllerLegacy {
    let state = Rc::new(State::default());
    let controller = gtk::EventControllerLegacy::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.connect_event(move |c, event| {
        // A popover on the note area (the context menu) is its own surface: its touches
        // scroll and pick menu items even with the pen still hovering.
        let own = c
            .widget()
            .and_then(|w| w.native())
            .and_then(|n| n.surface());
        let touch = matches!(
            event.event_type(),
            gdk::EventType::TouchBegin
                | gdk::EventType::TouchUpdate
                | gdk::EventType::TouchEnd
                | gdk::EventType::TouchCancel
        );
        if touch && event.surface() != own {
            return glib::Propagation::Proceed;
        }
        if state.swallow(event) {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    controller
}

#[derive(Default)]
struct State {
    in_proximity: Cell<bool>,
    last_pen: Cell<Option<u32>>,
    rejected: RefCell<HashSet<usize>>,
}

impl State {
    fn swallow(&self, event: &gdk::Event) -> bool {
        use gdk::EventType as E;
        let time = event.time();
        let kind = event.event_type();
        match kind {
            E::ProximityIn | E::ProximityOut => {
                self.in_proximity.set(kind == E::ProximityIn);
                self.last_pen.set(Some(time));
                false
            }
            E::TouchBegin => {
                let reject = self.pen_near(time);
                if reject {
                    self.rejected.borrow_mut().insert(sequence(event));
                    tracing::debug!("palm rejection: ignoring a touch");
                }
                reject
            }
            E::TouchUpdate | E::TouchEnd | E::TouchCancel => {
                let mut rejected = self.rejected.borrow_mut();
                let seq = sequence(event);
                let reject = rejected.contains(&seq);
                if reject && kind != E::TouchUpdate {
                    rejected.remove(&seq);
                }
                reject
            }
            _ => {
                let pen = event.device_tool().is_some()
                    || event
                        .device()
                        .is_some_and(|d| d.source() == gdk::InputSource::Pen);
                if pen {
                    self.last_pen.set(Some(time));
                }
                false
            }
        }
    }

    fn pen_near(&self, now: u32) -> bool {
        let Some(last) = self.last_pen.get() else {
            return false;
        };
        let since = now.wrapping_sub(last);
        since < AFTER_PEN || (self.in_proximity.get() && since < HOVER_LIMIT)
    }
}

fn sequence(event: &gdk::Event) -> usize {
    event.event_sequence().as_ptr() as usize
}
