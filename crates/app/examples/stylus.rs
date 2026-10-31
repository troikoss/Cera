//! M1 stylus spike (throwaway, not part of the app).
//!
//! Logs what the stylus and touchscreen report and draws raw pressure-width polylines so
//! latency can be eyeballed. Each pen-up logs a stroke summary (event and sample rates, backlog
//! behaviour, pressure/tilt ranges, handler and paint times, input→presentation latency) and
//! shows it in the corner.
//!
//! ```text
//! cargo run -p cera --release --example stylus
//! RUST_LOG=stylus=debug ...       every motion event
//! RUST_LOG=stylus=trace ...       every raw GDK event, hover included
//! GSK_RENDERER=gl|vulkan|cairo ... compare renderers (GTK 4.18+ names; `ngl` is now `gl`)
//! STYLUS_ONLY=0 ...               let the mouse and touch draw too (desktop testing)
//! ```
//!
//! Colours: blue = pen tip, orange = a barrel button held, red = eraser end.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::io::IsTerminal;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib::translate::IntoGlib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{cairo, gdk, glib, graphene};
use tracing::{debug, info, trace, warn};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;

/// Ink tiles are 256 × 256 device pixels, like the app's tile cache.
const TILE: i32 = 256;

const CSS: &str = "
label.hud {
    background: rgba(0, 0, 0, 0.65);
    color: white;
    padding: 8px 10px;
    border-radius: 8px;
    font-family: monospace;
    font-size: 10pt;
}";

fn main() -> glib::ExitCode {
    init_logging();
    let app = gtk::Application::builder()
        .application_id("io.github.troikoss.Cera.StylusSpike")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(build_window);
    app.run()
}

fn init_logging() {
    let default = || {
        Targets::new()
            .with_default(LevelFilter::WARN)
            .with_target("stylus", LevelFilter::INFO)
    };
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) => spec.parse().unwrap_or_else(|_| default()),
        Err(_) => default(),
    };
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal()),
        )
        .with(filter)
        .init();
}

fn build_window(app: &gtk::Application) {
    let css = gtk::CssProvider::new();
    css.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let pad = InkPad::new();
    let hud = gtk::Label::builder()
        .label("Write with the stylus; each stroke's numbers appear here.")
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .margin_start(12)
        .margin_top(12)
        .can_target(false)
        .build();
    hud.add_css_class("hud");
    let overlay = gtk::Overlay::builder().child(&pad).build();
    overlay.add_overlay(&hud);

    let spike = Rc::new(Spike::new(pad.clone(), hud));

    let clear = gtk::Button::with_label("Clear");
    let pad_ = pad.clone();
    clear.connect_clicked(move |_| pad_.clear());
    let backlog = gtk::ToggleButton::builder()
        .label("Use backlog")
        .active(true)
        .build();
    let s = spike.clone();
    backlog.connect_toggled(move |b| s.use_backlog.set(b.is_active()));
    let header = gtk::HeaderBar::new();
    header.pack_start(&clear);
    header.pack_start(&backlog);

    let stylus_only = std::env::var("STYLUS_ONLY").as_deref() != Ok("0");
    let stylus = gtk::GestureStylus::new();
    stylus.set_stylus_only(stylus_only);
    let s = spike.clone();
    stylus.connect_down(move |g, x, y| s.down(g, x, y));
    let s = spike.clone();
    stylus.connect_motion(move |g, x, y| s.motion(g, x, y));
    let s = spike.clone();
    stylus.connect_up(move |g, x, y| s.up(g, x, y));
    stylus.connect_proximity(|g, x, y| {
        trace!(
            x,
            y,
            distance = ?g.axis(gdk::AxisUse::Distance),
            pressure = ?g.axis(gdk::AxisUse::Pressure),
            "hover"
        );
    });
    pad.add_controller(stylus);

    // Sees every event before the gesture does, without consuming it.
    let raw = gtk::EventControllerLegacy::new();
    raw.set_propagation_phase(gtk::PropagationPhase::Capture);
    let s = spike.clone();
    raw.connect_event(move |_, ev| {
        s.raw_event(ev);
        glib::Propagation::Proceed
    });
    pad.add_controller(raw);

    let s = spike.clone();
    pad.connect_realize(move |pad| s.realized(pad));

    info!(stylus_only, "starting");
    gtk::ApplicationWindow::builder()
        .application(app)
        .title("Stylus spike")
        .default_width(1000)
        .default_height(700)
        .titlebar(&header)
        .child(&overlay)
        .build()
        .present();
}

/// One input sample in widget coordinates.
#[derive(Clone, Copy, Debug)]
struct Sample {
    t: u32,
    x: f64,
    y: f64,
    p: Option<f64>,
    xtilt: Option<f64>,
    ytilt: Option<f64>,
}

impl Sample {
    fn from_gesture(g: &gtk::GestureStylus, t: u32, x: f64, y: f64) -> Self {
        Self {
            t,
            x,
            y,
            p: g.axis(gdk::AxisUse::Pressure),
            xtilt: g.axis(gdk::AxisUse::Xtilt),
            ytilt: g.axis(gdk::AxisUse::Ytilt),
        }
    }

    /// Backlog entries; GTK has already translated X/Y to widget coordinates.
    fn from_coord(c: &gdk::TimeCoord) -> Self {
        let axes = c.axes();
        let get = |flag: gdk::AxisFlags, axis: gdk::AxisUse| {
            c.flags()
                .contains(flag)
                .then(|| axes[axis.into_glib() as usize])
        };
        Self {
            t: c.time(),
            x: axes[gdk::AxisUse::X.into_glib() as usize],
            y: axes[gdk::AxisUse::Y.into_glib() as usize],
            p: get(gdk::AxisFlags::PRESSURE, gdk::AxisUse::Pressure),
            xtilt: get(gdk::AxisFlags::XTILT, gdk::AxisUse::Xtilt),
            ytilt: get(gdk::AxisFlags::YTILT, gdk::AxisUse::Ytilt),
        }
    }
}

#[derive(Default)]
struct Range(Option<(f64, f64)>);

impl Range {
    fn add(&mut self, v: Option<f64>) {
        if let Some(v) = v {
            let (lo, hi) = self.0.unwrap_or((v, v));
            self.0 = Some((lo.min(v), hi.max(v)));
        }
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            Some((lo, hi)) => write!(f, "{lo:.3}..{hi:.3}"),
            None => f.write_str("none"),
        }
    }
}

struct Stroke {
    tool: String,
    start_ms: u32,
    end_ms: u32,
    last: Sample,
    events: u32,
    samples: u32,
    sample_gaps_ms: Vec<u32>,
    backlog_lens: Vec<u32>,
    /// Events whose backlog ends with the current point.
    backlog_has_current: u32,
    /// Events whose backlog starts before the previous event.
    backlog_overlaps: u32,
    pressure: Range,
    xtilt: Range,
    ytilt: Range,
    buttons: gdk::ModifierType,
    handler_us: Vec<u32>,
}

impl Stroke {
    fn new(tool: String, first: Sample, buttons: gdk::ModifierType) -> Self {
        let mut stroke = Self {
            tool,
            start_ms: first.t,
            end_ms: first.t,
            last: first,
            events: 0,
            samples: 0,
            sample_gaps_ms: Vec::new(),
            backlog_lens: Vec::new(),
            backlog_has_current: 0,
            backlog_overlaps: 0,
            pressure: Range::default(),
            xtilt: Range::default(),
            ytilt: Range::default(),
            buttons,
            handler_us: Vec::new(),
        };
        stroke.record(&first);
        stroke
    }

    fn record(&mut self, s: &Sample) {
        if self.samples > 0 {
            self.sample_gaps_ms.push(s.t.saturating_sub(self.last.t));
        }
        self.samples += 1;
        self.pressure.add(s.p);
        self.xtilt.add(s.xtilt);
        self.ytilt.add(s.ytilt);
        self.last = *s;
    }
}

/// Frame timing, attributed to frames that drew new input.
#[derive(Default)]
struct Frames {
    /// Newest input drawn since the last paint.
    input_ms: Option<u32>,
    paint_start_us: i64,
    /// (frame counter, input time) waiting for presentation feedback.
    pending: Vec<(i64, u32)>,
    paint_us: Vec<u32>,
    latency_ms: Vec<u32>,
    /// Frames with only a predicted presentation time (no compositor feedback).
    predicted: u32,
    /// Frames whose timings fell out of the frame clock's short history.
    lost: u32,
    /// Latencies too large to be real (input and frame clocks disagree).
    bogus: u32,
    refresh_us: i64,
}

impl Frames {
    fn resolve(&mut self, clock: &gdk::FrameClock) {
        let mut pending = std::mem::take(&mut self.pending);
        pending.retain(|&(counter, input_ms)| {
            let Some(t) = clock.timings(counter) else {
                self.lost += 1;
                return false;
            };
            if !t.is_complete() {
                return true;
            }
            self.refresh_us = t.refresh_interval();
            let mut present_us = t.presentation_time();
            if present_us == 0 {
                present_us = t.predicted_presentation_time();
                self.predicted += 1;
            }
            // Event times are CLOCK_MONOTONIC milliseconds truncated to 32 bits; the frame
            // clock uses CLOCK_MONOTONIC microseconds.
            let latency = ((present_us / 1000) as u32).wrapping_sub(input_ms);
            if present_us == 0 || latency > 1000 {
                self.bogus += 1;
            } else {
                self.latency_ms.push(latency);
            }
            false
        });
        self.pending = pending;
    }
}

struct Touch {
    start_ms: u32,
    updates: u32,
    emulating: bool,
    concurrent: usize,
    device: String,
    source: Option<gdk::InputSource>,
}

struct Spike {
    pad: InkPad,
    hud: gtk::Label,
    use_backlog: Cell<bool>,
    stroke: RefCell<Option<Stroke>>,
    /// Ended stroke waiting for its last frames' presentation feedback.
    finished: RefCell<Option<Stroke>>,
    strokes: Cell<u32>,
    last_up_ms: Cell<Option<u32>>,
    frames: RefCell<Frames>,
    touches: RefCell<HashMap<usize, Touch>>,
    renderer: RefCell<String>,
}

impl Spike {
    fn new(pad: InkPad, hud: gtk::Label) -> Self {
        Self {
            pad,
            hud,
            use_backlog: Cell::new(true),
            stroke: RefCell::default(),
            finished: RefCell::default(),
            strokes: Cell::new(0),
            last_up_ms: Cell::new(None),
            frames: RefCell::default(),
            touches: RefCell::default(),
            renderer: RefCell::default(),
        }
    }

    fn realized(self: &Rc<Self>, pad: &InkPad) {
        let renderer = pad.native().and_then(|n| n.renderer()).map_or_else(
            || "unknown renderer".into(),
            |r| r.type_().name().to_string(),
        );
        info!(%renderer, scale = pad.surface_scale(), "realized");
        *self.renderer.borrow_mut() = renderer;

        if let Some(seat) = pad.display().default_seat() {
            for d in seat.devices(gdk::SeatCapabilities::ALL) {
                info!(
                    name = %d.name(),
                    source = ?d.source(),
                    vendor = ?d.vendor_id(),
                    product = ?d.product_id(),
                    touches = d.num_touches(),
                    "device"
                );
            }
            for tool in seat.tools() {
                info!(tool = %describe_tool(Some(&tool)), "known tool");
            }
        }

        let clock = pad
            .frame_clock()
            .expect("realized widget has a frame clock");
        let s = self.clone();
        clock.connect_before_paint(move |_| {
            s.frames.borrow_mut().paint_start_us = glib::monotonic_time();
        });
        let s = self.clone();
        clock.connect_after_paint(move |clock| {
            let mut f = s.frames.borrow_mut();
            if let Some(input_ms) = f.input_ms.take() {
                let paint_us = glib::monotonic_time() - f.paint_start_us;
                f.paint_us.push(paint_us as u32);
                f.pending.push((clock.frame_counter(), input_ms));
            }
            f.resolve(clock);
        });
    }

    fn down(self: &Rc<Self>, g: &gtk::GestureStylus, x: f64, y: f64) {
        self.finish_pending();
        let Some(ev) = g.current_event() else { return };
        let tool = g.device_tool();
        let buttons = ev.modifier_state();
        let mut first = Sample::from_gesture(g, ev.time(), x, y);
        if tool.is_none() {
            // Mice report a constant 0.0 pressure on some backends; draw them at mid width.
            first.p = None;
        }
        let tool_desc = describe_tool(tool.as_ref());
        info!(
            tool = %tool_desc,
            t = first.t,
            x,
            y,
            pressure = ?first.p,
            xtilt = ?first.xtilt,
            ytilt = ?first.ytilt,
            ?buttons,
            "down"
        );
        self.pad
            .draw_segment(&first, &first, ink_color(tool.as_ref(), buttons));
        self.pad.flush();
        self.frames.borrow_mut().input_ms = Some(first.t);
        *self.stroke.borrow_mut() = Some(Stroke::new(tool_desc, first, buttons));
    }

    fn motion(self: &Rc<Self>, g: &gtk::GestureStylus, x: f64, y: f64) {
        let started = Instant::now();
        let Some(ev) = g.current_event() else { return };
        let mut guard = self.stroke.borrow_mut();
        let Some(stroke) = guard.as_mut() else { return };

        let current = Sample::from_gesture(g, ev.time(), x, y);
        let buttons = ev.modifier_state();
        let color = ink_color(g.device_tool().as_ref(), buttons);
        let backlog: Vec<Sample> = g
            .backlog()
            .unwrap_or_default()
            .iter()
            .map(Sample::from_coord)
            .collect();

        // Facts the real input code needs: does the backlog repeat the current point, or
        // points the previous event already delivered?
        let has_current = backlog.last().is_some_and(|b| {
            b.t == current.t && (b.x - current.x).abs() < 0.01 && (b.y - current.y).abs() < 0.01
        });
        let overlaps = backlog.first().is_some_and(|b| b.t < stroke.last.t);
        let mut new = backlog[..backlog.len() - usize::from(has_current)].to_vec();
        new.push(current);
        if g.device_tool().is_none() {
            new.iter_mut().for_each(|s| s.p = None);
        }

        let dt = current.t.wrapping_sub(stroke.last.t);
        let first_drawn = if self.use_backlog.get() {
            0
        } else {
            new.len() - 1
        };
        let mut from = stroke.last;
        for s in &new[first_drawn..] {
            self.pad.draw_segment(&from, s, color);
            from = *s;
        }
        for s in &new {
            stroke.record(s);
        }
        stroke.events += 1;
        stroke.backlog_lens.push(backlog.len() as u32);
        stroke.backlog_has_current += u32::from(has_current);
        stroke.backlog_overlaps += u32::from(overlaps);
        stroke.buttons |= buttons;

        self.pad.flush();
        self.frames.borrow_mut().input_ms = Some(current.t);
        let handler_us = started.elapsed().as_micros() as u32;
        stroke.handler_us.push(handler_us);
        debug!(
            t = current.t,
            dt,
            backlog = backlog.len(),
            has_current,
            overlaps,
            x,
            y,
            pressure = ?current.p,
            xtilt = ?current.xtilt,
            ytilt = ?current.ytilt,
            ?buttons,
            handler_us,
            "motion"
        );
    }

    fn up(self: &Rc<Self>, g: &gtk::GestureStylus, x: f64, y: f64) {
        let Some(mut stroke) = self.stroke.borrow_mut().take() else {
            return;
        };
        stroke.end_ms = g.current_event().map_or(stroke.last.t, |e| e.time());
        info!(
            t = stroke.end_ms,
            x,
            y,
            pressure = ?g.axis(gdk::AxisUse::Pressure),
            "up"
        );
        self.last_up_ms.set(Some(stroke.end_ms));
        *self.finished.borrow_mut() = Some(stroke);
        let s = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(200), move || s.finish_pending());
    }

    /// Logs the summary of the stroke that ended last, if not done yet.
    fn finish_pending(&self) {
        let Some(stroke) = self.finished.borrow_mut().take() else {
            return;
        };
        let n = self.strokes.get() + 1;
        self.strokes.set(n);
        let mut frames = std::mem::take(&mut *self.frames.borrow_mut());
        if let Some(clock) = self.pad.frame_clock() {
            frames.resolve(&clock);
        }
        let text = summary(
            n,
            stroke,
            frames,
            &self.renderer.borrow(),
            self.pad.tile_count(),
        );
        info!("\n{text}");
        self.hud.set_text(&text);
    }

    fn raw_event(&self, ev: &gdk::Event) {
        use gdk::EventType as E;
        let kind = ev.event_type();
        let source = ev.device().map(|d| d.source());
        match kind {
            E::ProximityIn => info!(
                tool = %describe_tool(ev.device_tool().as_ref()),
                ?source,
                t = ev.time(),
                "proximity in"
            ),
            E::ProximityOut => {
                let since_up_ms = self.last_up_ms.get().map(|up| ev.time().wrapping_sub(up));
                info!(t = ev.time(), ?since_up_ms, "proximity out");
            }
            E::ButtonPress | E::ButtonRelease => {
                let button = ev
                    .downcast_ref::<gdk::ButtonEvent>()
                    .map_or(0, |b| b.button());
                let tool = describe_tool(ev.device_tool().as_ref());
                if button == 1 {
                    debug!(?kind, button, ?source, %tool, t = ev.time(), "button");
                } else {
                    info!(?kind, button, ?source, %tool, t = ev.time(), "button");
                }
            }
            E::TouchBegin | E::TouchUpdate | E::TouchEnd | E::TouchCancel => {
                self.touch_event(ev, kind, source);
            }
            _ => trace!(?kind, ?source, t = ev.time(), "event"),
        }
    }

    fn touch_event(&self, ev: &gdk::Event, kind: gdk::EventType, source: Option<gdk::InputSource>) {
        use gdk::EventType as E;
        if source == Some(gdk::InputSource::Pen) || ev.device_tool().is_some() {
            warn!(?kind, ?source, "stylus arrived as a touch event");
        }
        let seq = ev.event_sequence().as_ptr() as usize;
        let mut touches = self.touches.borrow_mut();
        match kind {
            E::TouchBegin => {
                let touch = Touch {
                    start_ms: ev.time(),
                    updates: 0,
                    emulating: ev
                        .downcast_ref::<gdk::TouchEvent>()
                        .is_some_and(|t| t.is_emulating_pointer()),
                    concurrent: touches.len() + 1,
                    device: ev
                        .device()
                        .map(|d| d.name().to_string())
                        .unwrap_or_default(),
                    source,
                };
                debug!(
                    seq,
                    t = ev.time(),
                    concurrent = touch.concurrent,
                    "touch begin"
                );
                touches.insert(seq, touch);
            }
            E::TouchUpdate => {
                if let Some(touch) = touches.get_mut(&seq) {
                    touch.updates += 1;
                }
            }
            _ => {
                if let Some(touch) = touches.remove(&seq) {
                    info!(
                        seq,
                        t = ev.time(),
                        ms = ev.time().wrapping_sub(touch.start_ms),
                        updates = touch.updates,
                        cancelled = kind == E::TouchCancel,
                        emulating_pointer = touch.emulating,
                        concurrent = touch.concurrent,
                        device = %touch.device,
                        source = ?touch.source,
                        "touch sequence"
                    );
                }
            }
        }
    }
}

fn describe_tool(tool: Option<&gdk::DeviceTool>) -> String {
    match tool {
        Some(t) => format!(
            "{:?} serial={:#x} hw={:#x} axes={:?}",
            t.tool_type(),
            t.serial(),
            t.hardware_id(),
            t.axes()
        ),
        None => "no tool".into(),
    }
}

fn ink_color(tool: Option<&gdk::DeviceTool>, buttons: gdk::ModifierType) -> [f64; 3] {
    if tool.is_some_and(|t| t.tool_type() == gdk::DeviceToolType::Eraser) {
        [0.85, 0.15, 0.15]
    } else if buttons.intersects(gdk::ModifierType::BUTTON2_MASK | gdk::ModifierType::BUTTON3_MASK)
    {
        [0.95, 0.55, 0.05]
    } else {
        [0.1, 0.2, 0.55]
    }
}

/// "median 1, p95 2, max 3", or "n/a".
fn spread(mut v: Vec<u32>) -> String {
    if v.is_empty() {
        return "n/a".into();
    }
    v.sort_unstable();
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    format!("median {}, p95 {}, max {}", at(0.5), at(0.95), at(1.0))
}

fn summary(n: u32, s: Stroke, f: Frames, renderer: &str, tiles: usize) -> String {
    let ms = s.end_ms.wrapping_sub(s.start_ms).max(1);
    let per_s = |count: u32| f64::from(count) * 1000.0 / f64::from(ms);
    let backlog_max = s.backlog_lens.iter().max().copied().unwrap_or(0);
    let backlog_avg =
        s.backlog_lens.iter().sum::<u32>() as f64 / s.backlog_lens.len().max(1) as f64;
    let refresh = if f.refresh_us > 0 {
        format!("{:.1} Hz", 1e6 / f.refresh_us as f64)
    } else {
        "unknown".into()
    };
    [
        format!("stroke {n}: {}", s.tool),
        format!(
            "  {ms} ms, {} events ({:.0}/s), {} samples ({:.0}/s), sample gap ms: {}",
            s.events,
            per_s(s.events),
            s.samples,
            per_s(s.samples),
            spread(s.sample_gaps_ms)
        ),
        format!(
            "  backlog/event max {backlog_max} avg {backlog_avg:.1}; repeats current {}/{}; overlaps previous {}/{}",
            s.backlog_has_current, s.events, s.backlog_overlaps, s.events
        ),
        format!(
            "  pressure {}  xtilt {}  ytilt {}  buttons {:?}",
            s.pressure, s.xtilt, s.ytilt, s.buttons
        ),
        format!("  motion handler µs: {}", spread(s.handler_us)),
        format!("  paint µs: {}", spread(f.paint_us)),
        format!(
            "  input→present ms: {} (predicted {}, lost {}, bogus {}, unresolved {})",
            spread(f.latency_ms),
            f.predicted,
            f.lost,
            f.bogus,
            f.pending.len()
        ),
        format!("  {renderer}, refresh {refresh}, {tiles} ink tiles"),
    ]
    .join("\n")
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct InkPad {
        pub(super) tiles: RefCell<HashMap<(i32, i32), super::Tile>>,
        /// Device pixels per widget pixel the tiles were drawn at.
        pub(super) scale: Cell<f64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for InkPad {
        const NAME: &'static str = "SpikeInkPad";
        type Type = super::InkPad;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for InkPad {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_hexpand(true);
            obj.set_vexpand(true);
            obj.set_overflow(gtk::Overflow::Hidden);
        }
    }

    impl WidgetImpl for InkPad {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let bounds = graphene::Rect::new(0.0, 0.0, obj.width() as f32, obj.height() as f32);
            snapshot.append_color(&gdk::RGBA::WHITE, &bounds);
            let size = TILE as f32 / self.scale.get() as f32;
            for (&(tx, ty), tile) in self.tiles.borrow().iter() {
                if let Some(texture) = &tile.texture {
                    let rect = graphene::Rect::new(tx as f32 * size, ty as f32 * size, size, size);
                    snapshot.append_texture(texture, &rect);
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct InkPad(ObjectSubclass<imp::InkPad>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

struct Tile {
    surface: cairo::ImageSurface,
    texture: Option<gdk::Texture>,
    dirty: bool,
}

impl InkPad {
    fn new() -> Self {
        glib::Object::new()
    }

    fn surface_scale(&self) -> f64 {
        self.native()
            .and_then(|n| n.surface())
            .map_or(1.0, |s| s.scale())
    }

    /// Draws one raw segment into every tile it touches. Width follows the end pressure.
    fn draw_segment(&self, a: &Sample, b: &Sample, color: [f64; 3]) {
        let imp = self.imp();
        let scale = self.surface_scale();
        if scale != imp.scale.get() {
            imp.tiles.borrow_mut().clear();
            imp.scale.set(scale);
        }
        let width = 1.0 + 5.0 * b.p.unwrap_or(0.5);
        let margin = width / 2.0 + 1.0;
        let tile_of = |v: f64| ((v * scale).floor() as i32).div_euclid(TILE);
        let (tx0, tx1) = (
            tile_of(a.x.min(b.x) - margin),
            tile_of(a.x.max(b.x) + margin),
        );
        let (ty0, ty1) = (
            tile_of(a.y.min(b.y) - margin),
            tile_of(a.y.max(b.y) + margin),
        );

        let mut tiles = imp.tiles.borrow_mut();
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let tile = tiles.entry((tx, ty)).or_insert_with(|| Tile {
                    surface: cairo::ImageSurface::create(cairo::Format::ARgb32, TILE, TILE)
                        .expect("tile surface"),
                    texture: None,
                    dirty: false,
                });
                let cr = cairo::Context::new(&tile.surface).expect("cairo context");
                cr.translate(f64::from(-tx * TILE), f64::from(-ty * TILE));
                cr.scale(scale, scale);
                cr.set_source_rgb(color[0], color[1], color[2]);
                cr.set_line_width(width);
                cr.set_line_cap(cairo::LineCap::Round);
                cr.move_to(a.x, a.y);
                cr.line_to(b.x, b.y);
                let _ = cr.stroke();
                tile.dirty = true;
            }
        }
    }

    /// Turns dirty tiles into fresh textures. Called once per input event, which GTK already
    /// limits to about one per frame, so snapshot() only appends textures.
    fn flush(&self) {
        let mut changed = false;
        for tile in self.imp().tiles.borrow_mut().values_mut() {
            if !tile.dirty {
                continue;
            }
            let stride = tile.surface.stride() as usize;
            let mut bytes = None;
            let _ = tile
                .surface
                .with_data(|data| bytes = Some(glib::Bytes::from(data)));
            if let Some(bytes) = bytes {
                // cairo ARGB32 is premultiplied BGRA in memory on little-endian machines.
                let texture = gdk::MemoryTexture::new(
                    TILE,
                    TILE,
                    gdk::MemoryFormat::B8g8r8a8Premultiplied,
                    &bytes,
                    stride,
                );
                tile.texture = Some(texture.upcast());
            }
            tile.dirty = false;
            changed = true;
        }
        if changed {
            self.queue_draw();
        }
    }

    fn clear(&self) {
        self.imp().tiles.borrow_mut().clear();
        self.queue_draw();
    }

    fn tile_count(&self) -> usize {
        self.imp().tiles.borrow().len()
    }
}
