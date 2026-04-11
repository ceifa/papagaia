use std::{
    cell::RefCell,
    io::{self, BufRead},
    rc::Rc,
    thread,
};

use anyhow::Result;
use glib::{self, ControlFlow};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{self as layer_shell, LayerShell};
use papagaia_core::OverlayMessage;

fn main() -> Result<()> {
    let app = gtk::Application::builder()
        .application_id("io.ceifa.papagaia.overlay")
        .build();
    app.connect_activate(build_ui);
    app.run();
    Ok(())
}

fn build_ui(app: &gtk::Application) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("papagaia overlay")
        .default_width(260)
        .default_height(84)
        .resizable(false)
        .decorated(false)
        .build();

    window.init_layer_shell();
    window.set_layer(layer_shell::Layer::Overlay);
    window.set_keyboard_mode(layer_shell::KeyboardMode::None);
    window.set_anchor(layer_shell::Edge::Top, true);
    window.set_anchor(layer_shell::Edge::Left, true);
    window.set_anchor(layer_shell::Edge::Right, true);
    window.set_margin(layer_shell::Edge::Top, 28);

    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(18)
        .margin_end(18)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    outer.add_css_class("papagaia-card");

    let title = gtk::Label::builder()
        .label("papagaia")
        .halign(gtk::Align::Start)
        .build();
    title.add_css_class("title-4");

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();

    let spinner = gtk::Spinner::builder().spinning(false).build();
    let bars = build_bars();
    let status = gtk::Label::builder()
        .label("")
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .build();

    row.append(&spinner);
    row.append(&bars.container);
    row.append(&status);
    outer.append(&title);
    outer.append(&row);
    window.set_child(Some(&outer));
    window.hide();

    install_css();

    let state = Rc::new(UiState {
        window: window.clone(),
        spinner,
        status,
        bars,
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
                    if let Ok(message) = serde_json::from_str::<OverlayMessage>(&line) {
                        if tx.send(message).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    let receiver = Rc::new(RefCell::new(rx));
    glib::timeout_add_local(std::time::Duration::from_millis(33), move || {
        while let Ok(message) = receiver.borrow_mut().try_recv() {
            apply_message(&state, message);
        }
        ControlFlow::Continue
    });
}

struct UiState {
    window: gtk::ApplicationWindow,
    spinner: gtk::Spinner,
    status: gtk::Label,
    bars: Bars,
}

struct Bars {
    container: gtk::Box,
    widgets: Vec<gtk::LevelBar>,
}

fn build_bars() -> Bars {
    let container = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();
    let mut widgets = Vec::new();
    for _ in 0..5 {
        let bar = gtk::LevelBar::builder()
            .orientation(gtk::Orientation::Vertical)
            .min_value(0.0)
            .max_value(1.0)
            .value(0.05)
            .inverted(true)
            .build();
        bar.set_size_request(8, 28);
        widgets.push(bar.clone());
        container.append(&bar);
    }
    container.hide();
    Bars { container, widgets }
}

fn apply_message(state: &UiState, message: OverlayMessage) {
    match message {
        OverlayMessage::Hidden => {
            state.spinner.stop();
            state.spinner.hide();
            state.bars.container.hide();
            state.window.hide();
        }
        OverlayMessage::Busy { label } => {
            state.status.set_label(&label);
            state.spinner.show();
            state.spinner.start();
            state.bars.container.hide();
            state.window.present();
        }
        OverlayMessage::Recording { level, transcript } => {
            state.spinner.stop();
            state.spinner.hide();
            state.bars.container.show();
            state.window.present();
            let label = transcript.unwrap_or_else(|| "Listening".into());
            state.status.set_label(&label);
            set_bars(&state.bars.widgets, level);
        }
        OverlayMessage::Result { ok, message } => {
            state.spinner.stop();
            state.spinner.hide();
            state.bars.container.hide();
            state.status.set_label(&message);
            state.window.present();
            if ok {
                state.window.add_css_class("success");
                state.window.remove_css_class("error");
            } else {
                state.window.add_css_class("error");
                state.window.remove_css_class("success");
            }
        }
    }
}

fn set_bars(bars: &[gtk::LevelBar], level: f32) {
    let multipliers = [0.45, 0.7, 1.0, 0.72, 0.5];
    for (bar, factor) in bars.iter().zip(multipliers) {
        bar.set_value(((level * 3.2) * factor as f32).clamp(0.05, 1.0) as f64);
    }
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        "
        window {
            background: transparent;
        }

        .papagaia-card {
            background: rgba(18, 23, 31, 0.92);
            color: #eef3f7;
            border-radius: 18px;
            border: 1px solid rgba(148, 196, 255, 0.16);
            min-width: 220px;
        }

        levelbar block.filled {
            background: #74d39f;
            border-radius: 999px;
        }

        levelbar trough {
            background: rgba(255, 255, 255, 0.08);
            border-radius: 999px;
        }

        window.success .papagaia-card {
            border-color: rgba(116, 211, 159, 0.35);
        }

        window.error .papagaia-card {
            border-color: rgba(255, 120, 120, 0.38);
        }
        ",
    );

    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("display not available"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
