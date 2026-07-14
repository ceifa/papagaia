use std::{
    cell::{Cell, RefCell},
    f64::consts::{FRAC_PI_2, PI},
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    rc::Rc,
    thread,
};

use anyhow::Result;
use futures_util::StreamExt;
use glib::{self, ControlFlow};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{self as layer_shell, LayerShell};
use papagaia_core::{ClientRequest, OverlayMessage, PickerEntry, PickerResult};

/// Number of bars in the waveform and the drawing-area dimensions of the pill's
/// voice meter. Kept compact so the pill stays small, like Wispr Flow.
const BAR_COUNT: usize = 26;
const WAVE_W: i32 = 150;
const WAVE_H: i32 = 20;

const STATE_CLASSES: &[&str] = &[
    "state-idle",
    "state-busy",
    "state-recording",
    "state-success",
    "state-error",
    "state-notice",
];

fn main() -> Result<()> {
    let pick = std::env::args().skip(1).any(|arg| arg == "--pick");

    // NON_UNIQUE: otherwise a second instance (e.g. after a daemon restart while
    // an orphan overlay lives) would act as a remote client to the orphan and
    // never create its own window. This makes each spawn own its windows.
    let app = if pick {
        gtk::Application::builder()
            .application_id("io.ceifa.papagaia.picker")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build()
    } else {
        gtk::Application::builder()
            .application_id("io.ceifa.papagaia.overlay")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build()
    };

    if pick {
        let entries = read_picker_entries();
        app.connect_activate(move |app| build_picker_ui(app, entries.clone()));
    } else {
        app.connect_activate(build_ui);
    }

    app.run_with_args::<String>(&[]);
    Ok(())
}

// ---------------------------------------------------------------------------
// Picker mode
// ---------------------------------------------------------------------------

fn read_picker_entries() -> Vec<PickerEntry> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).unwrap_or_default();
    serde_json::from_str(&input).unwrap_or_default()
}

fn build_picker_ui(app: &gtk::Application, entries: Vec<PickerEntry>) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("papagaia picker")
        .resizable(false)
        .decorated(false)
        .build();

    window.init_layer_shell();
    window.set_layer(layer_shell::Layer::Overlay);
    window.set_keyboard_mode(layer_shell::KeyboardMode::OnDemand);
    window.set_anchor(layer_shell::Edge::Top, true);
    window.set_margin(layer_shell::Edge::Top, 110);

    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .build();
    card.add_css_class("picker-card");

    let input = gtk::Entry::builder()
        .placeholder_text("Search prompts or type a command…")
        .build();
    input.add_css_class("picker-input");

    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.add_css_class("picker-divider");

    let list_box = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .build();
    list_box.add_css_class("picker-list");

    for entry in &entries {
        let row_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        row_box.add_css_class("picker-row");

        let name_label = gtk::Label::new(Some(&entry.name));
        name_label.add_css_class("row-name");
        name_label.set_xalign(0.0);

        let summary_label = gtk::Label::new(Some(&entry.summary));
        summary_label.add_css_class("row-summary");
        summary_label.set_xalign(0.0);
        summary_label.set_hexpand(true);
        summary_label.set_ellipsize(gtk::pango::EllipsizeMode::End);

        row_box.append(&name_label);
        row_box.append(&summary_label);

        list_box.append(&row_box);
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .max_content_height(300)
        .propagate_natural_height(true)
        .build();
    scrolled.set_child(Some(&list_box));

    if entries.is_empty() {
        scrolled.hide();
        separator.hide();
    }

    card.append(&input);
    card.append(&separator);
    card.append(&scrolled);

    window.set_child(Some(&card));

    let entries = Rc::new(entries);

    let filter_entries = entries.clone();
    let filter_input = input.clone();
    list_box.set_filter_func(move |row| {
        let text = filter_input.text().to_string();
        let index = row.index() as usize;
        filter_entries
            .get(index)
            .is_some_and(|e| picker_matches(e, &text))
    });

    let list_for_changed = list_box.clone();
    let entries_for_changed = entries.clone();
    let separator_for_changed = separator.clone();
    let scrolled_for_changed = scrolled.clone();
    input.connect_changed(move |inp| {
        list_for_changed.invalidate_filter();
        let text = inp.text();
        let visible = picker_visible_indices(&entries_for_changed, &text);
        picker_auto_select(&list_for_changed, &entries_for_changed, &text);
        if visible.is_empty() {
            separator_for_changed.hide();
            scrolled_for_changed.hide();
            inp.add_css_class("picker-input-alone");
        } else {
            separator_for_changed.show();
            scrolled_for_changed.show();
            inp.remove_css_class("picker-input-alone");
        }
    });

    let key_ctrl = gtk::EventControllerKey::new();
    key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
    let app_for_key = app.clone();
    let list_for_key = list_box.clone();
    let input_for_key = input.clone();
    let entries_for_key = entries.clone();
    key_ctrl.connect_key_pressed(move |_, key, _, _| match key {
        gtk::gdk::Key::Escape => {
            app_for_key.quit();
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => {
            if let Some(result) = picker_resolve(&list_for_key, &entries_for_key, &input_for_key)
                && let Ok(json) = serde_json::to_string(&result)
            {
                print!("{json}");
            }
            app_for_key.quit();
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Down => {
            picker_move(&list_for_key, &entries_for_key, &input_for_key.text(), 1);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Up => {
            picker_move(&list_for_key, &entries_for_key, &input_for_key.text(), -1);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    window.add_controller(key_ctrl);

    let app_for_activate = app.clone();
    let entries_for_activate = entries.clone();
    list_box.connect_row_activated(move |_, row| {
        let index = row.index() as usize;
        if let Some(entry) = entries_for_activate.get(index) {
            let result = PickerResult::Template {
                name: entry.name.clone(),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                print!("{json}");
            }
        }
        app_for_activate.quit();
    });

    picker_auto_select(&list_box, &entries, &input.text());

    install_css();
    window.present();
    input.grab_focus();
}

fn picker_visible_indices(entries: &[PickerEntry], filter_text: &str) -> Vec<i32> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| picker_matches(e, filter_text))
        .map(|(i, _)| i as i32)
        .collect()
}

/// Whether a prompt entry matches the picker query. The query is matched as a
/// fuzzy subsequence against both the prompt name and its summary, so users can
/// type loosely ("fg" → "fix-grammar") or search by what a prompt does.
fn picker_matches(entry: &PickerEntry, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let haystack = format!("{} {}", entry.name, entry.summary).to_lowercase();
    fuzzy_subsequence(&haystack, &query)
}

/// True when every non-whitespace char of `query` appears in `haystack` in
/// order (not necessarily contiguously).
fn fuzzy_subsequence(haystack: &str, query: &str) -> bool {
    let mut hay = haystack.chars();
    'needle: for needle in query.chars() {
        if needle.is_whitespace() {
            continue;
        }
        for c in hay.by_ref() {
            if c == needle {
                continue 'needle;
            }
        }
        return false;
    }
    true
}

fn picker_auto_select(list: &gtk::ListBox, entries: &[PickerEntry], filter_text: &str) {
    let visible = picker_visible_indices(entries, filter_text);
    match visible.first() {
        Some(&index) => list.select_row(list.row_at_index(index).as_ref()),
        None => list.select_row(None::<&gtk::ListBoxRow>),
    }
}

fn picker_move(list: &gtk::ListBox, entries: &[PickerEntry], filter_text: &str, delta: i32) {
    let visible = picker_visible_indices(entries, filter_text);
    if visible.is_empty() {
        return;
    }

    let current = list.selected_row().map(|r| r.index()).unwrap_or(-1);
    let pos = visible.iter().position(|&i| i == current);

    let new_pos = match pos {
        Some(p) => (p as i32 + delta).clamp(0, visible.len() as i32 - 1) as usize,
        None => {
            if delta > 0 {
                0
            } else {
                visible.len() - 1
            }
        }
    };

    if let Some(&row_index) = visible.get(new_pos) {
        list.select_row(list.row_at_index(row_index).as_ref());
    }
}

fn picker_resolve(
    list: &gtk::ListBox,
    entries: &[PickerEntry],
    input: &gtk::Entry,
) -> Option<PickerResult> {
    if let Some(row) = list.selected_row() {
        let index = row.index() as usize;
        if let Some(entry) = entries.get(index) {
            return Some(PickerResult::Template {
                name: entry.name.clone(),
            });
        }
    }

    let text = input.text().to_string();
    if !text.is_empty() {
        return parse_picker_raw(&text);
    }

    None
}

fn parse_picker_raw(text: &str) -> Option<PickerResult> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    Some(PickerResult::Raw {
        template: text.to_string(),
    })
}

// ---------------------------------------------------------------------------
// HUD mode — Wispr-style bottom-center pill
// ---------------------------------------------------------------------------

fn build_ui(app: &gtk::Application) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("papagaia overlay")
        .resizable(false)
        .decorated(false)
        .build();

    window.init_layer_shell();
    window.set_layer(layer_shell::Layer::Overlay);
    window.set_keyboard_mode(layer_shell::KeyboardMode::None);
    // Bottom-center, like Wispr Flow. Anchoring only to the bottom edge (no
    // left/right) lets layer-shell center the pill horizontally.
    window.set_anchor(layer_shell::Edge::Bottom, true);
    window.set_margin(layer_shell::Edge::Bottom, 48);

    let key_ctrl = gtk::EventControllerKey::new();
    key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
    key_ctrl.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            thread::spawn(|| {
                send_cancel();
            });
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);

    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(9)
        .valign(gtk::Align::Center)
        .build();
    card.add_css_class("papagaia-card");

    let glyph = gtk::Label::new(None);
    glyph.add_css_class("glyph");
    glyph.set_xalign(0.5);
    glyph.set_visible(false);

    let wave = build_wave();

    let message = gtk::Label::builder()
        .label("")
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(30)
        .xalign(0.0)
        .build();
    message.add_css_class("message");
    message.set_visible(false);

    card.append(&glyph);
    card.append(&wave.area);
    card.append(&message);

    window.set_child(Some(&card));
    window.hide();

    install_css();

    let state = Rc::new(UiState {
        window: window.clone(),
        card: card.clone(),
        glyph,
        message,
        wave,
        opacity: Cell::new(0.0),
        target_opacity: Cell::new(0.0),
        pending_hide: Cell::new(false),
        shown: Cell::new(false),
        render_source: RefCell::new(None),
    });

    // A blocking stdin reader forwards parsed messages over a channel; the local
    // future below wakes only on a message (no polling). Animation ticks are armed
    // only while there's something to animate (see `ensure_render_tick`).
    let (tx, mut rx) = futures_channel::mpsc::unbounded::<OverlayMessage>();
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut locked = stdin.lock();
        let mut line = String::new();
        loop {
            line.clear();
            match locked.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Ok(message) = serde_json::from_str::<OverlayMessage>(&line)
                        && tx.unbounded_send(message).is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let apply_state = Rc::clone(&state);
    glib::spawn_future_local(async move {
        while let Some(message) = rx.next().await {
            apply_message(&apply_state, message);
            // Any message either starts a fade-in, a fade-out, or live bars, so
            // make sure the animation tick is running to carry it to rest.
            ensure_render_tick(&apply_state);
        }
    });
}

/// Arms the ~33ms animation tick if it isn't already running. The tick advances
/// the fade and the waveform, then suspends itself once everything has settled,
/// so an idle/hidden overlay holds no timer.
fn ensure_render_tick(state: &Rc<UiState>) {
    if state.render_source.borrow().is_some() {
        return;
    }
    let tick_state = Rc::clone(state);
    let id = glib::timeout_add_local(std::time::Duration::from_millis(33), move || {
        step_opacity(&tick_state);
        if tick_state.shown.get() && wave_active(&tick_state) {
            step_wave(&tick_state.wave);
        }
        if is_animating(&tick_state) {
            ControlFlow::Continue
        } else {
            *tick_state.render_source.borrow_mut() = None;
            ControlFlow::Break
        }
    });
    *state.render_source.borrow_mut() = Some(id);
}

/// The waveform animates both while recording (live mic level) and while busy
/// (a gentle processing shimmer).
fn wave_active(state: &UiState) -> bool {
    state.shown.get()
        && (state.card.has_css_class("state-recording") || state.card.has_css_class("state-busy"))
}

/// Whether the render tick still has work: a fade in progress, or a live/shimmer
/// waveform while the pill is visible.
fn is_animating(state: &UiState) -> bool {
    (state.opacity.get() - state.target_opacity.get()).abs() > 0.001 || wave_active(state)
}

struct UiState {
    window: gtk::ApplicationWindow,
    card: gtk::Box,
    glyph: gtk::Label,
    message: gtk::Label,
    wave: Wave,
    /// Current card opacity, driven toward `target_opacity` by the render tick
    /// so every show/hide eases instead of snapping.
    opacity: Cell<f64>,
    target_opacity: Cell<f64>,
    /// When set, the card is fading out and the window is hidden for real once
    /// opacity reaches zero.
    pending_hide: Cell<bool>,
    /// Whether the window is currently mapped. Lets us start a fresh fade-in
    /// only on the transition from hidden to visible.
    shown: Cell<bool>,
    /// Handle to the running animation tick, if any. `None` means no tick is
    /// scheduled (idle overlay), so `ensure_render_tick` knows to start one.
    render_source: RefCell<Option<glib::SourceId>>,
}

#[derive(Clone, Copy, PartialEq)]
enum WaveKind {
    Listening,
    Processing,
}

/// A Cairo-drawn voice meter: a row of rounded bars that ripple outward from the
/// centre, driven by the live mic level (and a low shimmer while processing).
struct Wave {
    area: gtk::DrawingArea,
    /// Displayed amplitude per bar (centre-out scroll buffer).
    bars: Rc<RefCell<Vec<f64>>>,
    /// Latest eased input level injected at the centre.
    level: Cell<f64>,
    /// Raw target the level eases toward (fast attack, slow release).
    target_level: Cell<f64>,
    /// Free-running phase for the shimmer / liveliness variation.
    phase: Cell<f64>,
    /// Whether to draw the listening or processing palette.
    kind: Rc<Cell<WaveKind>>,
}

fn build_wave() -> Wave {
    let area = gtk::DrawingArea::new();
    area.set_content_width(WAVE_W);
    area.set_content_height(WAVE_H);
    area.set_valign(gtk::Align::Center);
    area.add_css_class("wave");

    let bars = Rc::new(RefCell::new(vec![0.0_f64; BAR_COUNT]));
    let kind = Rc::new(Cell::new(WaveKind::Listening));

    let bars_for_draw = bars.clone();
    let kind_for_draw = kind.clone();
    area.set_draw_func(move |_, cr, width, height| {
        draw_wave(
            cr,
            width,
            height,
            &bars_for_draw.borrow(),
            kind_for_draw.get(),
        );
    });

    Wave {
        area,
        bars,
        level: Cell::new(0.0),
        target_level: Cell::new(0.0),
        phase: Cell::new(0.0),
        kind,
    }
}

fn draw_wave(cr: &gtk::cairo::Context, width: i32, height: i32, bars: &[f64], kind: WaveKind) {
    let n = bars.len();
    if n == 0 {
        return;
    }
    let w = width as f64;
    let h = height as f64;
    let mid = h / 2.0;
    let gap = 2.0;
    let bar_w = ((w - gap * (n as f64 - 1.0)) / n as f64).max(1.0);
    let min_h = 2.0;

    let (r, g, b, a) = match kind {
        WaveKind::Listening => (0.87, 0.91, 1.0, 0.96),
        WaveKind::Processing => (0.49, 0.64, 1.0, 0.85),
    };
    cr.set_source_rgba(r, g, b, a);

    for (i, &value) in bars.iter().enumerate() {
        let x = i as f64 * (bar_w + gap);
        let bh = (value.clamp(0.0, 1.0) * h).max(min_h);
        let y = mid - bh / 2.0;
        rounded_bar(cr, x, y, bar_w, bh, bar_w / 2.0);
    }
    let _ = cr.fill();
}

fn rounded_bar(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    let r = radius.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, PI + FRAC_PI_2);
    cr.close_path();
}

fn set_state_class(state: &UiState, name: &str) {
    for cls in STATE_CLASSES {
        state.card.remove_css_class(cls);
        state.window.remove_css_class(cls);
    }
    state.card.add_css_class(name);
    state.window.add_css_class(name);
}

fn apply_message(state: &Rc<UiState>, message: OverlayMessage) {
    match message {
        OverlayMessage::Hidden => {
            begin_hide(state);
        }
        OverlayMessage::Busy {
            label,
            grab_keyboard,
        } => {
            set_state_class(state, "state-busy");
            state.glyph.set_visible(false);
            state.wave.kind.set(WaveKind::Processing);
            state.wave.area.set_visible(true);
            state.message.set_visible(true);
            state.message.set_label(&label);
            // An exclusive grab is how Esc-to-cancel works on layer-shell; the
            // daemon only asks for it when no other window needs focus.
            let mode = if grab_keyboard {
                layer_shell::KeyboardMode::Exclusive
            } else {
                layer_shell::KeyboardMode::None
            };
            state.window.set_keyboard_mode(mode);
            request_show(state, grab_keyboard);
        }
        OverlayMessage::Recording { level } => {
            set_state_class(state, "state-recording");
            state.glyph.set_visible(true);
            state.glyph.set_label("●");
            state.wave.kind.set(WaveKind::Listening);
            state.wave.area.set_visible(true);
            // Listening shows only the waveform (Wispr-style).
            state.message.set_visible(false);
            // The user is speaking, not typing, so the exclusive grab (for Esc) is safe.
            state
                .window
                .set_keyboard_mode(layer_shell::KeyboardMode::Exclusive);
            set_wave_level(&state.wave, level);
            request_show(state, true);
        }
        OverlayMessage::Result { ok, message } => {
            set_state_class(state, if ok { "state-success" } else { "state-error" });
            state.glyph.set_visible(true);
            state.glyph.set_label(if ok { "✓" } else { "✕" });
            state.wave.area.set_visible(false);
            state.message.set_visible(true);
            state.message.set_label(&message);
            state
                .window
                .set_keyboard_mode(layer_shell::KeyboardMode::None);
            request_show(state, false);
        }
        OverlayMessage::Notice { message } => {
            set_state_class(state, "state-notice");
            state.glyph.set_visible(true);
            state.glyph.set_label("·");
            state.wave.area.set_visible(false);
            state.message.set_visible(true);
            state.message.set_label(&message);
            state
                .window
                .set_keyboard_mode(layer_shell::KeyboardMode::None);
            request_show(state, false);
        }
    }
}

/// Maps the latest mic level onto the amplitude injected at the waveform centre.
/// `sqrt` turns linear RMS (speech is typically 0.02–0.15) into a perceptually
/// proportional scale where bar movement is visible across the whole volume span.
fn set_wave_level(wave: &Wave, level: f32) {
    let perceptual = level.sqrt();
    wave.target_level
        .set((perceptual as f64 * 1.8).clamp(0.0, 1.0));
}

/// Advances the waveform one frame: ease the level (fast attack, slow release),
/// scroll the bar buffer outward from the centre, and inject the newest
/// amplitude (or a gentle shimmer while processing) at the centre bar.
fn step_wave(wave: &Wave) {
    let target = wave.target_level.get();
    let current = wave.level.get();
    let smoothing = if target > current { 0.6 } else { 0.25 };
    let level = current + (target - current) * smoothing;
    wave.level.set(level);

    let phase = wave.phase.get() + 0.4;
    wave.phase.set(phase);

    let injected = match wave.kind.get() {
        WaveKind::Processing => 0.10 + 0.06 * (phase.sin() * 0.5 + 0.5),
        WaveKind::Listening => {
            let variation = 0.78 + 0.22 * (phase * 1.7).sin().abs();
            (level * variation).clamp(0.0, 1.0)
        }
    };

    {
        let mut bars = wave.bars.borrow_mut();
        let n = bars.len();
        if n == 0 {
            return;
        }
        let center = n / 2;
        for i in 0..center {
            bars[i] = bars[i + 1];
        }
        for i in (center + 1..n).rev() {
            bars[i] = bars[i - 1];
        }
        bars[center] = injected;
    }
    wave.area.queue_draw();
}

/// Marks the overlay to fade in (or stay visible) and presents/shows the
/// window. A fresh fade-in only starts on the hidden→visible transition.
fn request_show(state: &UiState, present: bool) {
    if !state.shown.get() {
        state.opacity.set(0.0);
        state.card.set_opacity(0.0);
        state.shown.set(true);
    }
    state.pending_hide.set(false);
    state.target_opacity.set(1.0);
    if present {
        state.window.present();
    } else {
        state.window.show();
    }
}

/// Begins a fade-out. The window is unmapped for real by `step_opacity` once
/// the card reaches full transparency.
fn begin_hide(state: &UiState) {
    state
        .window
        .set_keyboard_mode(layer_shell::KeyboardMode::None);
    if !state.shown.get() {
        finish_hide(state);
        return;
    }
    state.pending_hide.set(true);
    state.target_opacity.set(0.0);
}

fn finish_hide(state: &UiState) {
    state.window.hide();
    state.shown.set(false);
    state.pending_hide.set(false);
    set_state_class(state, "state-idle");
}

/// Advances the card's fade animation by one render frame (~33ms). A step of
/// 0.2 makes a full fade take ~5 frames (~165ms).
fn step_opacity(state: &UiState) {
    let current = state.opacity.get();
    let target = state.target_opacity.get();
    if (current - target).abs() <= 0.001 {
        return;
    }
    const STEP: f64 = 0.2;
    let next = if current < target {
        (current + STEP).min(target)
    } else {
        (current - STEP).max(target)
    };
    state.opacity.set(next);
    state.card.set_opacity(next);
    if state.pending_hide.get() && next <= 0.001 {
        finish_hide(state);
    }
}

fn send_cancel() {
    let socket = match papagaia_core::socket_path() {
        Ok(path) => path,
        Err(_) => return,
    };
    let mut stream = match UnixStream::connect(&socket) {
        Ok(s) => s,
        Err(_) => return,
    };
    let request = match serde_json::to_string(&ClientRequest::Cancel) {
        Ok(json) => format!("{json}\n"),
        Err(_) => return,
    };
    let _ = stream.write_all(request.as_bytes());
    let _ = stream.flush();
    // Read response to avoid broken pipe on daemon side
    let mut response = String::new();
    let _ = BufReader::new(stream).read_line(&mut response);
}

// ---------------------------------------------------------------------------
// Shared CSS
// ---------------------------------------------------------------------------

fn install_css() {
    let display = gtk::gdk::Display::default().expect("display not available");

    // Fallback palette at PRIORITY_FALLBACK: real themes (PRIORITY_THEME) win;
    // this only fills in semantic colors a minimal theme leaves undefined.
    let fallback = gtk::CssProvider::new();
    fallback.load_from_data(
        r#"
        @define-color card_bg_color #15181f;
        @define-color card_fg_color #eef1f6;
        @define-color window_bg_color #15181f;
        @define-color window_fg_color #eef1f6;
        @define-color accent_bg_color #7aa2ff;
        @define-color accent_color #7aa2ff;
        @define-color success_color #74d39f;
        @define-color warning_color #ffb547;
        @define-color error_color #ff7a85;
        @define-color borders rgba(128, 128, 128, 0.25);
        "#,
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &fallback,
        gtk::STYLE_PROVIDER_PRIORITY_FALLBACK,
    );

    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        r#"
        window {
            background: transparent;
        }

        /* --- HUD pill --- */

        .papagaia-card {
            /* margin leaves room for the drop shadow to render unclipped */
            margin: 14px;
            padding: 6px 15px;
            background: alpha(@card_bg_color, 0.88);
            color: @card_fg_color;
            border-radius: 9999px;
            border: 1px solid alpha(@card_fg_color, 0.10);
            box-shadow: 0 6px 18px rgba(0, 0, 0, 0.5);
        }

        .glyph {
            font-size: 10px;
            font-weight: 700;
            min-width: 10px;
            color: alpha(@card_fg_color, 0.6);
        }

        .message {
            font-family: "Inter", "IBM Plex Sans", "Cantarell", sans-serif;
            font-size: 12px;
            font-weight: 500;
            color: @card_fg_color;
        }

        .state-recording .glyph {
            color: @error_color;
            animation: papagaia-rec-pulse 1.3s ease-in-out infinite;
        }

        @keyframes papagaia-rec-pulse {
            0% { opacity: 1; }
            50% { opacity: 0.2; }
            100% { opacity: 1; }
        }

        .state-success .glyph { color: @success_color; }
        .state-error .glyph { color: @error_color; }
        .state-error .message { color: @error_color; }
        .state-notice .glyph { color: alpha(@card_fg_color, 0.45); }
        .state-notice .message { color: alpha(@card_fg_color, 0.7); }

        /* --- Picker --- */

        /* Same visual language as the HUD pill: flat translucent surface, a
           hairline border, a soft shadow, and the Inter UI font.
           NOTE: the window is sized to the card + its margin, so the shadow's
           reach (offset + blur) must fit inside `margin` or layer-shell clips it
           into a hard rectangle. margin 26 contains this 0 6px 18px shadow. */
        .picker-card {
            margin: 26px;
            background: alpha(@card_bg_color, 0.92);
            color: @card_fg_color;
            border-radius: 16px;
            border: 1px solid alpha(@card_fg_color, 0.10);
            box-shadow: 0 6px 18px rgba(0, 0, 0, 0.5);
            min-width: 420px;
        }

        .picker-input {
            background: alpha(@card_fg_color, 0.05);
            color: @card_fg_color;
            border: none;
            border-radius: 16px 16px 0 0;
            padding: 13px 18px;
            font-family: "Inter", "IBM Plex Sans", "Cantarell", sans-serif;
            font-size: 14px;
            box-shadow: none;
        }

        .picker-input.picker-input-alone {
            border-radius: 16px;
        }

        .picker-input:focus {
            box-shadow: inset 0 -2px 0 @accent_bg_color;
        }

        .picker-divider {
            background: alpha(@card_fg_color, 0.10);
            min-height: 1px;
        }

        .picker-list {
            background: transparent;
        }

        .picker-list row {
            padding: 10px 18px;
            border-radius: 0;
        }

        .picker-list row:selected {
            background: alpha(@accent_bg_color, 0.16);
        }

        .row-name {
            font-family: "Inter", "IBM Plex Sans", "Cantarell", sans-serif;
            font-size: 13px;
            font-weight: 600;
            color: @card_fg_color;
        }

        .row-summary {
            font-family: "Inter", "IBM Plex Sans", "Cantarell", sans-serif;
            font-size: 12px;
            color: alpha(@card_fg_color, 0.55);
        }
        "#,
    );

    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

#[cfg(test)]
mod tests {
    use super::{PickerEntry, PickerResult, parse_picker_raw, picker_matches};

    fn entry(name: &str, summary: &str) -> PickerEntry {
        PickerEntry {
            name: name.into(),
            summary: summary.into(),
        }
    }

    #[test]
    fn picker_matches_empty_query_matches_everything() {
        assert!(picker_matches(&entry("shorten", "Make it shorter"), "  "));
    }

    #[test]
    fn picker_matches_fuzzy_subsequence_on_name() {
        let e = entry("fix-grammar", "Correct grammar and spelling");
        assert!(picker_matches(&e, "fg"));
        assert!(picker_matches(&e, "fixgram"));
        assert!(!picker_matches(&e, "gf"));
    }

    #[test]
    fn picker_matches_searches_summary_too() {
        let e = entry("shorten", "Make it shorter but keep the meaning");
        assert!(picker_matches(&e, "meaning"));
    }

    #[test]
    fn plain_picker_text_resolves_to_raw() {
        let result = parse_picker_raw("Fix this: {{text}}").expect("picker should resolve");
        match result {
            PickerResult::Raw { template } => {
                assert_eq!(template, "Fix this: {{text}}");
            }
            PickerResult::Template { .. } => panic!("expected raw picker result"),
        }
    }
}
