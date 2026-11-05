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
//! Push-to-talk edges go through one gesture thread first, which tells a hold
//! from a double-tap lock.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self as std_mpsc, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use evdev::{Device, EventType, KeyCode};
use papagaia_core::ClientRequest;
use tokio::sync::mpsc::UnboundedSender;

/// How often to rescan `/dev/input` so newly plugged keyboards are picked up.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

/// A push-to-talk press shorter than this is a tap; two taps this close lock
/// dictation on.
const TAP: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, PartialEq)]
pub enum Action {
    PushToTalk,
    Toggle,
    Pick,
}

struct Binding {
    code: u16,
    action: Action,
    /// Held-state for a tap key, shared across a keyboard's multiple event nodes so
    /// duplicate edges collapse into single transitions. Push-to-talk keeps its
    /// own in `Gesture`.
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
    let (edges, edge_rx) = std_mpsc::channel();
    {
        let requests = requests.clone();
        let busy = busy.clone();
        thread::spawn(move || gesture_loop(edge_rx, requests, busy));
    }
    thread::spawn(move || watch_loop(bindings, requests, edges, busy));
}

/// A push-to-talk key edge: pressed or released, and when it was read.
type Edge = (bool, Instant);

fn watch_loop(
    bindings: Arc<Vec<Binding>>,
    requests: UnboundedSender<ClientRequest>,
    edges: std_mpsc::Sender<Edge>,
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
            let edges = edges.clone();
            let busy = busy.clone();
            thread::spawn(move || read_device(device, &bindings, &requests, &edges, &busy));
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
    edges: &std_mpsc::Sender<Edge>,
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
                if !dispatch(binding, event.value(), requests, edges, busy) {
                    return; // receiver gone — daemon shutting down.
                }
            }
        }
    }
}

/// Handle one key event for a binding. Returns false if the daemon is gone (so
/// the reader can stop).
fn dispatch(
    binding: &Binding,
    value: i32,
    requests: &UnboundedSender<ClientRequest>,
    edges: &std_mpsc::Sender<Edge>,
    busy: &AtomicBool,
) -> bool {
    // value: 1 = down, 0 = up, 2 = auto-repeat (ignored).
    match binding.action {
        Action::PushToTalk => match value {
            0 | 1 => edges.send((value == 1, Instant::now())).is_ok(),
            _ => true,
        },
        Action::Toggle => fire_on_tap(binding, value, requests, busy, ClientRequest::DictateToggle),
        Action::Pick => fire_on_tap(binding, value, requests, busy, ClientRequest::Pick),
    }
}

/// Feed push-to-talk edges through `Gesture`, waking up for its tap deadline.
fn gesture_loop(
    edges: std_mpsc::Receiver<Edge>,
    requests: UnboundedSender<ClientRequest>,
    busy: Arc<AtomicBool>,
) {
    let mut gesture = Gesture::default();
    loop {
        let edge = match gesture.deadline() {
            Some(deadline) => {
                match edges.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(edge) => Some(edge),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match edges.recv() {
                Ok(edge) => Some(edge),
                Err(_) => return,
            },
        };
        let request = match edge {
            Some((true, at)) => gesture.press(at, busy.load(Ordering::SeqCst)),
            Some((false, at)) => gesture.release(at),
            None => gesture.expire(),
        };
        if let Some(request) = request
            && requests.send(request).is_err()
        {
            return;
        }
    }
}

/// The push-to-talk key: hold to dictate, or double-tap to keep dictating
/// hands-free until the next press. Recording starts on the first press either
/// way, so a hold never waits on the gesture.
#[derive(Debug, Default, PartialEq)]
enum Gesture {
    #[default]
    Idle,
    Held {
        since: Instant,
    },
    /// A tap ended at `released`; a second tap within `TAP` locks.
    Tapped {
        released: Instant,
    },
    /// Pressed again after a tap: the release decides between lock and a hold.
    SecondPress {
        first_release: Instant,
    },
    Locked,
    /// The press that finished a lock, or one dropped while busy: its release
    /// does nothing.
    Spent,
}

impl Gesture {
    /// Repeated edges (a keyboard's several event nodes report each one) fall
    /// through as no-ops.
    fn press(&mut self, now: Instant, busy: bool) -> Option<ClientRequest> {
        match *self {
            Gesture::Idle if busy => {
                *self = Gesture::Spent;
                None
            }
            Gesture::Idle => {
                *self = Gesture::Held { since: now };
                Some(ClientRequest::DictateStart)
            }
            Gesture::Tapped { released } => {
                *self = Gesture::SecondPress {
                    first_release: released,
                };
                None
            }
            Gesture::Locked => {
                *self = Gesture::Spent;
                Some(ClientRequest::DictateStop)
            }
            Gesture::Held { .. } | Gesture::SecondPress { .. } | Gesture::Spent => None,
        }
    }

    fn release(&mut self, now: Instant) -> Option<ClientRequest> {
        match *self {
            Gesture::Held { since } if now - since < TAP => {
                *self = Gesture::Tapped { released: now };
                None
            }
            Gesture::SecondPress { first_release } if now - first_release < TAP => {
                *self = Gesture::Locked;
                None
            }
            Gesture::Held { .. } | Gesture::SecondPress { .. } => {
                *self = Gesture::Idle;
                Some(ClientRequest::DictateStop)
            }
            Gesture::Spent => {
                *self = Gesture::Idle;
                None
            }
            Gesture::Idle | Gesture::Tapped { .. } | Gesture::Locked => None,
        }
    }

    fn deadline(&self) -> Option<Instant> {
        match *self {
            Gesture::Tapped { released } => Some(released + TAP),
            _ => None,
        }
    }

    /// The tap window closed without a second tap: a lone tap is a short dictation.
    fn expire(&mut self) -> Option<ClientRequest> {
        match *self {
            Gesture::Tapped { .. } => {
                *self = Gesture::Idle;
                Some(ClientRequest::DictateStop)
            }
            _ => None,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ClientRequest::{DictateStart, DictateStop};

    fn ms(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn a_hold_dictates_until_release() {
        let t = Instant::now();
        let mut gesture = Gesture::default();
        assert_eq!(gesture.press(t, false), Some(DictateStart));
        assert_eq!(gesture.release(ms(t, 1200)), Some(DictateStop));
        assert_eq!(gesture, Gesture::Idle);
    }

    #[test]
    fn a_double_tap_locks_until_the_next_press() {
        let t = Instant::now();
        let mut gesture = Gesture::default();
        assert_eq!(gesture.press(t, false), Some(DictateStart));
        assert_eq!(gesture.release(ms(t, 100)), None);
        assert_eq!(gesture.press(ms(t, 200), false), None);
        assert_eq!(gesture.release(ms(t, 300)), None);
        assert_eq!(gesture, Gesture::Locked);
        assert_eq!(gesture.deadline(), None);

        assert_eq!(gesture.press(ms(t, 9000), false), Some(DictateStop));
        assert_eq!(gesture.release(ms(t, 9100)), None);
        assert_eq!(gesture, Gesture::Idle);
    }

    #[test]
    fn a_lone_tap_finishes_when_the_tap_window_closes() {
        let t = Instant::now();
        let mut gesture = Gesture::default();
        gesture.press(t, false);
        assert_eq!(gesture.release(ms(t, 150)), None);
        assert_eq!(gesture.deadline(), Some(ms(t, 450)));
        assert_eq!(gesture.expire(), Some(DictateStop));
        assert_eq!(gesture, Gesture::Idle);
    }

    #[test]
    fn a_tap_then_a_hold_is_one_long_hold() {
        let t = Instant::now();
        let mut gesture = Gesture::default();
        gesture.press(t, false);
        gesture.release(ms(t, 100));
        assert_eq!(gesture.press(ms(t, 200), false), None);
        assert_eq!(gesture.release(ms(t, 2000)), Some(DictateStop));
        assert_eq!(gesture, Gesture::Idle);
    }

    #[test]
    fn repeated_edges_from_several_event_nodes_are_ignored() {
        let t = Instant::now();
        let mut gesture = Gesture::default();
        assert_eq!(gesture.press(t, false), Some(DictateStart));
        assert_eq!(gesture.press(t, false), None);
        assert_eq!(gesture.release(ms(t, 1000)), Some(DictateStop));
        assert_eq!(gesture.release(ms(t, 1000)), None);
    }

    #[test]
    fn a_press_while_busy_starts_nothing_and_neither_does_its_release() {
        let t = Instant::now();
        let mut gesture = Gesture::default();
        assert_eq!(gesture.press(t, true), None);
        assert_eq!(gesture.release(ms(t, 100)), None);
        assert_eq!(gesture, Gesture::Idle);
    }
}
