//! Global hotkeys via evdev.
//!
//! papagaia reads the keyboard directly from `/dev/input` (needs `input`-group
//! membership, no root) and maps configured keys to actions — the only way to do
//! hold-to-talk, since compositors like niri can't bind a key release.
//!
//! Devices are *monitored*, never grabbed, so a bound key still reaches the
//! focused app — pick keys whose passthrough is harmless (F13, RightCtrl). evdev
//! reads block, so each device gets its own thread; key edges reach the runtime
//! through an `UnboundedSender` (the same entry point CLI requests use).

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use evdev::{Device, EventType, KeyCode};
use papagaia_core::ClientRequest;
use tokio::sync::mpsc::UnboundedSender;

/// How often to rescan `/dev/input` so newly plugged keyboards are picked up.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq)]
pub enum Action {
    PushToTalk,
    Toggle,
    Pick,
}

struct Binding {
    code: u16,
    action: Action,
    /// Held-state for this key, shared across a keyboard's multiple event nodes so
    /// duplicate edges collapse into single transitions.
    pressed: AtomicBool,
}

/// Start watching for the given `(keycode, action)` bindings. Spawns background
/// threads and returns immediately; does nothing if there are no bindings.
pub fn spawn(
    bindings: Vec<(u16, Action)>,
    requests: UnboundedSender<ClientRequest>,
    busy: Arc<AtomicBool>,
) {
    if bindings.is_empty() {
        return;
    }
    let bindings: Arc<Vec<Binding>> = Arc::new(
        bindings
            .into_iter()
            .map(|(code, action)| Binding {
                code,
                action,
                pressed: AtomicBool::new(false),
            })
            .collect(),
    );
    thread::spawn(move || watch_loop(bindings, requests, busy));
}

fn watch_loop(
    bindings: Arc<Vec<Binding>>,
    requests: UnboundedSender<ClientRequest>,
    busy: Arc<AtomicBool>,
) {
    let codes: HashSet<u16> = bindings.iter().map(|b| b.code).collect();
    let mut watched: HashSet<PathBuf> = HashSet::new();
    let mut first_pass = true;

    loop {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for (path, device) in evdev::enumerate() {
            seen.insert(path.clone());
            if watched.contains(&path) || !is_keyboard_like(&device, &codes) {
                continue;
            }
            watched.insert(path.clone());
            let bindings = bindings.clone();
            let requests = requests.clone();
            let busy = busy.clone();
            thread::spawn(move || read_device(device, &bindings, &requests, &busy));
        }
        watched.retain(|path| seen.contains(path));

        if first_pass {
            if watched.is_empty() {
                eprintln!(
                    "papagaia: keybinds found no readable keyboards (is your user in the 'input' group?)"
                );
            } else {
                eprintln!(
                    "papagaia: keybinds watching {} keyboard device(s)",
                    watched.len()
                );
            }
            first_pass = false;
        }

        thread::sleep(RESCAN_INTERVAL);
    }
}

/// Whether a device looks like a real keyboard — it advertises one of the bound
/// keys, or at least the letters/space a normal keyboard has (so we don't attach
/// to mice, power buttons, or other key-emitting oddities).
fn is_keyboard_like(device: &Device, codes: &HashSet<u16>) -> bool {
    device.supported_keys().is_some_and(|keys| {
        (keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_SPACE))
            || codes.iter().any(|&code| keys.contains(KeyCode::new(code)))
    })
}

/// Blocking read loop for one device. Exits (ending the thread) when the device
/// errors out, e.g. on unplug — the rescan in `watch_loop` reattaches replugs.
fn read_device(
    mut device: Device,
    bindings: &[Binding],
    requests: &UnboundedSender<ClientRequest>,
    busy: &AtomicBool,
) {
    loop {
        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(_) => return,
        };
        for event in events {
            if event.event_type() != EventType::KEY {
                continue;
            }
            for binding in bindings.iter().filter(|b| b.code == event.code()) {
                if !dispatch(binding, event.value(), requests, busy) {
                    return; // receiver gone — daemon shutting down.
                }
            }
        }
    }
}

/// Handle one key event for a binding. Returns false if the request channel is
/// closed (so the reader can stop). While `busy`, start edges are swallowed
/// (leaving `pressed` untouched, so the matching key-up is a no-op too); the
/// push-to-talk key-up is never swallowed, so a real recording can always stop.
fn dispatch(
    binding: &Binding,
    value: i32,
    requests: &UnboundedSender<ClientRequest>,
    busy: &AtomicBool,
) -> bool {
    // value: 1 = down, 0 = up, 2 = auto-repeat (ignored).
    match binding.action {
        Action::PushToTalk => match value {
            1 if busy.load(Ordering::SeqCst) => true,
            1 if !binding.pressed.swap(true, Ordering::SeqCst) => {
                requests.send(ClientRequest::DictateStart).is_ok()
            }
            0 if binding.pressed.swap(false, Ordering::SeqCst) => {
                requests.send(ClientRequest::DictateStop).is_ok()
            }
            _ => true,
        },
        Action::Toggle => fire_on_tap(binding, value, requests, busy, ClientRequest::DictateToggle),
        Action::Pick => fire_on_tap(binding, value, requests, busy, ClientRequest::Pick),
    }
}

/// Tap semantics: fire `request` once on the first key-down edge; the key-up just
/// re-arms it so the next tap fires. A down edge is dropped while `busy`.
fn fire_on_tap(
    binding: &Binding,
    value: i32,
    requests: &UnboundedSender<ClientRequest>,
    busy: &AtomicBool,
    request: ClientRequest,
) -> bool {
    match value {
        1 if busy.load(Ordering::SeqCst) => true,
        1 if !binding.pressed.swap(true, Ordering::SeqCst) => requests.send(request).is_ok(),
        0 => {
            binding.pressed.store(false, Ordering::SeqCst);
            true
        }
        _ => true,
    }
}
