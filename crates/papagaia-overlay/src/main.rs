use std::{
    cell::{Cell, RefCell},
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    rc::Rc,
    thread,
};

use anyhow::Result;
use clap::Parser;
use glib::{self, ControlFlow};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{self as layer_shell, LayerShell};
use papagaia_core::{ClientRequest, OverlayMessage};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "papagaia-overlay")]
struct Args {
    #[arg(long)]
    pick: bool,
}

#[derive(Clone, Deserialize)]
struct PickerEntry {
    name: String,
    summary: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum PickerResult {
    Template {
        name: String,
    },
    Raw {
        template: String,
        strip_markdown_fences: bool,
        trim_whitespace: bool,
        stream_output: bool,
    },
}

const BRAILLE_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const STATE_CLASSES: &[&str] = &[
    "state-idle",
    "state-busy",
    "state-recording",
    "state-success",
    "state-error",
];

fn main() -> Result<()> {
    let args = Args::parse();

    let app = if args.pick {
        gtk::Application::builder()
            .application_id("io.ceifa.papagaia.picker")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build()
    } else {
        gtk::Application::builder()
            .application_id("io.ceifa.papagaia.overlay")
            .build()
    };

    if args.pick {
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
        let text = filter_input.text().to_string().to_lowercase();
        if text.is_empty() {
            return true;
        }
        let index = row.index() as usize;
        filter_entries
            .get(index)
            .is_some_and(|e| e.name.to_lowercase().contains(&text))
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
    let filter = filter_text.to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| filter.is_empty() || e.name.to_lowercase().contains(&filter))
        .map(|(i, _)| i as i32)
        .collect()
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
        strip_markdown_fences: false,
        trim_whitespace: true,
        stream_output: true,
    })
}

// ---------------------------------------------------------------------------
// HUD mode (existing overlay)
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
    window.set_anchor(layer_shell::Edge::Top, true);
    window.set_margin(layer_shell::Edge::Top, 36);

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
        .spacing(10)
        .valign(gtk::Align::Center)
        .build();
    card.add_css_class("papagaia-card");

    let glyph = gtk::Label::new(None);
    glyph.add_css_class("glyph");
    glyph.set_use_markup(true);
    glyph.set_xalign(0.5);
    glyph.set_width_chars(2);

    let bars = build_bars();
    let message = gtk::Label::builder()
        .label("")
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(28)
        .xalign(0.0)
        .build();
    message.add_css_class("message");

    card.append(&glyph);
    card.append(&bars.container);
    card.append(&message);

    window.set_child(Some(&card));
    window.hide();

    install_css();

    let state = Rc::new(UiState {
        window: window.clone(),
        card: card.clone(),
        glyph,
        message,
        bars,
        spinner_frame: Cell::new(0),
    });

    let spinner_state = Rc::clone(&state);
    glib::timeout_add_local(std::time::Duration::from_millis(90), move || {
        if spinner_state.card.has_css_class("state-busy") {
            let next = (spinner_state.spinner_frame.get() + 1) % BRAILLE_FRAMES.len() as u32;
            spinner_state.spinner_frame.set(next);
            spinner_state
                .glyph
                .set_markup(&mono(BRAILLE_FRAMES[next as usize]));
        }
        ControlFlow::Continue
    });

    let (tx, rx) = std::sync::mpsc::channel::<OverlayMessage>();
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
                        && tx.send(message).is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let receiver = Rc::new(RefCell::new(rx));
    let apply_state = Rc::clone(&state);
    glib::timeout_add_local(std::time::Duration::from_millis(33), move || {
        while let Ok(message) = receiver.borrow_mut().try_recv() {
            apply_message(&apply_state, message);
        }
        ControlFlow::Continue
    });
}

struct UiState {
    window: gtk::ApplicationWindow,
    card: gtk::Box,
    glyph: gtk::Label,
    message: gtk::Label,
    bars: Bars,
    spinner_frame: Cell<u32>,
}

struct Bars {
    container: gtk::Box,
    widgets: Vec<gtk::LevelBar>,
}

fn build_bars() -> Bars {
    let container = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(3)
        .valign(gtk::Align::Center)
        .build();
    container.add_css_class("bars");
    let mut widgets = Vec::new();
    for _ in 0..7 {
        let bar = gtk::LevelBar::builder()
            .orientation(gtk::Orientation::Vertical)
            .min_value(0.0)
            .max_value(1.0)
            .value(0.05)
            .inverted(true)
            .build();
        bar.set_size_request(3, 24);
        bar.add_css_class("wave");
        widgets.push(bar.clone());
        container.append(&bar);
    }
    container.hide();
    Bars { container, widgets }
}

fn set_state_class(state: &UiState, name: &str) {
    for cls in STATE_CLASSES {
        state.card.remove_css_class(cls);
        state.window.remove_css_class(cls);
    }
    state.card.add_css_class(name);
    state.window.add_css_class(name);
}

fn mono(text: &str) -> String {
    format!(
        "<span font_family=\"IBM Plex Mono,JetBrains Mono,Iosevka,Fira Code,monospace\">{}</span>",
        glib::markup_escape_text(text)
    )
}

fn apply_message(state: &UiState, message: OverlayMessage) {
    match message {
        OverlayMessage::Hidden => {
            state.bars.container.hide();
            state
                .window
                .set_keyboard_mode(layer_shell::KeyboardMode::None);
            state.window.hide();
            set_state_class(state, "state-idle");
        }
        OverlayMessage::Busy {
            label,
            grab_keyboard,
        } => {
            set_state_class(state, "state-busy");
            state
                .glyph
                .set_markup(&mono(BRAILLE_FRAMES[state.spinner_frame.get() as usize]));
            state.bars.container.hide();
            state.message.set_label(&label);
            // Exclusive keyboard focus is how Esc-to-cancel actually works on
            // layer-shell compositors — On-demand only delivers keys after an
            // explicit click, and None blocks them entirely. The daemon only
            // asks for a grab during phases where no other window needs focus
            // (i.e. engine/whisper running), so it's safe to steal input here.
            let mode = if grab_keyboard {
                layer_shell::KeyboardMode::Exclusive
            } else {
                layer_shell::KeyboardMode::None
            };
            state.window.set_keyboard_mode(mode);
            if grab_keyboard {
                state.window.present();
            } else {
                state.window.show();
            }
        }
        OverlayMessage::Recording { level, transcript } => {
            set_state_class(state, "state-recording");
            state.glyph.set_markup(&mono("●"));
            state.bars.container.show();
            state
                .message
                .set_label(&transcript.unwrap_or_else(|| "Listening…".into()));
            // During recording the user is speaking, not typing, so grabbing
            // the keyboard exclusively is safe and makes Esc-to-cancel work.
            state
                .window
                .set_keyboard_mode(layer_shell::KeyboardMode::Exclusive);
            state.window.present();
            set_bars(&state.bars.widgets, level);
        }
        OverlayMessage::Result { ok, message } => {
            if ok {
                set_state_class(state, "state-success");
                state.glyph.set_markup(&mono("✓"));
            } else {
                set_state_class(state, "state-error");
                state.glyph.set_markup(&mono("✕"));
            }
            state.bars.container.hide();
            state.message.set_label(&message);
            state
                .window
                .set_keyboard_mode(layer_shell::KeyboardMode::None);
            state.window.show();
        }
    }
}

fn set_bars(bars: &[gtk::LevelBar], level: f32) {
    // Apply sqrt to convert linear RMS into a perceptually-proportional scale.
    // Raw RMS for speech is typically 0.02–0.15; sqrt expands that into a
    // range where bar movement is clearly visible across the full volume span.
    let perceptual = level.sqrt();
    let multipliers = [0.35, 0.58, 0.82, 1.0, 0.82, 0.58, 0.35];
    for (bar, factor) in bars.iter().zip(multipliers) {
        bar.set_value((perceptual * 2.5 * factor).clamp(0.05, 1.0) as f64);
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

    // Fallback palette: only applies when the active GTK theme doesn't define
    // these semantic colors. Theme providers sit at PRIORITY_THEME (200),
    // which overrides our PRIORITY_FALLBACK (1), so Adwaita & modern themes
    // win and we only fill in when a minimal theme leaves them undefined.
    let fallback = gtk::CssProvider::new();
    fallback.load_from_data(
        r#"
        @define-color card_bg_color #1e242f;
        @define-color card_fg_color #e9edf3;
        @define-color window_bg_color #1e242f;
        @define-color window_fg_color #e9edf3;
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

        /* --- HUD overlay --- */

        .papagaia-card {
            padding: 8px 14px;
            background: linear-gradient(160deg, shade(@card_bg_color, 1.08) 0%, shade(@card_bg_color, 0.88) 100%);
            color: @card_fg_color;
            border-radius: 12px;
            border-left: 3px solid @borders;
            min-width: 180px;
        }

        .glyph {
            font-size: 12px;
            font-weight: 700;
            color: alpha(@card_fg_color, 0.55);
            min-width: 14px;
        }

        .message {
            font-family: "IBM Plex Sans", "Inter Tight", "Cantarell", sans-serif;
            font-size: 12px;
            font-weight: 400;
            color: @card_fg_color;
        }

        levelbar.wave trough {
            background: alpha(@card_fg_color, 0.08);
            border-radius: 2px;
            min-width: 3px;
        }

        levelbar.wave block.filled {
            background: @warning_color;
            border-radius: 2px;
            min-width: 3px;
            box-shadow: 0 0 8px alpha(@warning_color, 0.55);
        }

        levelbar.wave block.empty {
            background: transparent;
            min-width: 3px;
        }

        .state-busy.papagaia-card { border-left-color: @accent_bg_color; }
        .state-busy .glyph { color: @accent_bg_color; }

        .state-recording.papagaia-card { border-left-color: @warning_color; }
        .state-recording .glyph { color: @warning_color; }

        .state-success.papagaia-card {
            border-left-color: @success_color;
            border-top-color: alpha(@success_color, 0.22);
            border-right-color: alpha(@success_color, 0.22);
            border-bottom-color: alpha(@success_color, 0.22);
        }
        .state-success .glyph { color: @success_color; }

        .state-error.papagaia-card {
            border-left-color: @error_color;
            border-top-color: alpha(@error_color, 0.26);
            border-right-color: alpha(@error_color, 0.26);
            border-bottom-color: alpha(@error_color, 0.26);
        }
        .state-error .glyph { color: @error_color; }

        /* --- Picker --- */

        .picker-card {
            background: linear-gradient(160deg, shade(@card_bg_color, 1.08) 0%, shade(@card_bg_color, 0.88) 100%);
            color: @card_fg_color;
            border-radius: 14px;
            border: 1px solid @borders;
            box-shadow: none;
            min-width: 420px;
        }

        .picker-input {
            background: alpha(@card_fg_color, 0.04);
            color: @card_fg_color;
            border: none;
            border-radius: 14px 14px 0 0;
            padding: 14px 18px;
            font-family: "IBM Plex Mono", "JetBrains Mono", "Iosevka", "Fira Code", monospace;
            font-size: 14px;
            box-shadow: none;
        }

        .picker-input.picker-input-alone {
            border-radius: 14px;
        }

        .picker-input:focus {
            box-shadow: inset 0 -2px 0 @accent_bg_color;
        }

        .picker-divider {
            background: @borders;
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
            background: alpha(@accent_bg_color, 0.14);
        }

        .row-name {
            font-family: "IBM Plex Mono", "JetBrains Mono", "Iosevka", "Fira Code", monospace;
            font-size: 13px;
            font-weight: 600;
            color: @card_fg_color;
        }

        .row-summary {
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
    use super::{PickerResult, parse_picker_raw};

    #[test]
    fn plain_picker_text_uses_streaming_raw_defaults() {
        let result = parse_picker_raw("Fix this: {{text}}").expect("picker should resolve");
        match result {
            PickerResult::Raw {
                template,
                strip_markdown_fences,
                trim_whitespace,
                stream_output,
            } => {
                assert_eq!(template, "Fix this: {{text}}");
                assert!(!strip_markdown_fences);
                assert!(trim_whitespace);
                assert!(stream_output);
            }
            PickerResult::Template { .. } => panic!("expected raw picker result"),
        }
    }
}
