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
//! Push-to-talk, pick and dictate-key edges go through one chord thread first,
//! which tells a hold from a double-tap lock and decides what a combination
//! means: a dictate key overlapping a dictation routes it to that key's prompt,
//! and the pick key to the picker — on its own, the pick key opens the picker
//! for the selection.

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
use papagaia_core::{ClientRequest, DictationTarget};
use tokio::sync::mpsc::UnboundedSender;

/// How often to rescan `/dev/input` so newly plugged keyboards are picked up.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

/// A push-to-talk press shorter than this is a tap; two taps this close lock
/// dictation on.
const TAP: Duration = Duration::from_millis(300);

#[derive(Clone, PartialEq)]
pub enum Action {
    PushToTalk,
    Toggle,
    Pick,
    /// Held during a dictation: send it through the named prompt.
    Dictate(String),
}

struct Binding {
    code: u16,
    action: Action,
    /// Held-state for the toggle key, shared across a keyboard's multiple event
    /// nodes so duplicate edges collapse into single transitions. The other keys
    /// keep theirs in `Chords`.
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
        thread::spawn(move || chord_loop(edge_rx, requests, busy));
    }
    thread::spawn(move || watch_loop(bindings, requests, edges, busy));
}

/// A key edge for the chord thread (`true` = pressed), and when it was read.
#[derive(Debug, Clone, PartialEq)]
enum Key {
    Talk(bool),
    Pick(bool),
    Dictate(String, bool),
}

type Edge = (Key, Instant);

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
    if value != 0 && value != 1 {
        return true;
    }
    let down = value == 1;
    let key = match &binding.action {
        Action::Toggle => {
            return fire_on_tap(binding, value, requests, busy, ClientRequest::DictateToggle);
        }
        Action::PushToTalk => Key::Talk(down),
        Action::Pick => Key::Pick(down),
        Action::Dictate(prompt) => Key::Dictate(prompt.clone(), down),
    };
    edges.send((key, Instant::now())).is_ok()
}

/// Feed key edges through `Chords`, waking up for its tap deadline.
fn chord_loop(
    edges: std_mpsc::Receiver<Edge>,
    requests: UnboundedSender<ClientRequest>,
    busy: Arc<AtomicBool>,
) {
    let mut chords = Chords::default();
    loop {
        let edge = match chords.gesture.deadline() {
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
        let busy = busy.load(Ordering::SeqCst);
        let request = match edge {
            Some((key, at)) => chords.key(key, at, busy),
            None => chords.gesture.expire(),
        };
        if let Some(request) = request
            && requests.send(request).is_err()
        {
            return;
        }
    }
}

/// Everything the chord thread tracks across keys.
#[derive(Debug, Default)]
struct Chords {
    gesture: Gesture,
    /// Where the pick or dictate key held right now would send a dictation.
    held: Option<DictationTarget>,
    pick: PickKey,
}

/// On its own, the pick key opens the picker on release, and only for a press
/// no dictation overlapped.
#[derive(Debug, Default, PartialEq)]
enum PickKey {
    #[default]
    Up,
    Armed,
    Spent,
}

impl Chords {
    fn key(&mut self, key: Key, now: Instant, busy: bool) -> Option<ClientRequest> {
        match key {
            Key::Talk(true) => {
                if self.pick != PickKey::Up {
                    self.pick = PickKey::Spent;
                }
                match self.gesture.press(now, busy) {
                    Some(ClientRequest::DictateStart { .. }) => Some(ClientRequest::DictateStart {
                        to: self.held.clone(),
                    }),
                    other => other,
                }
            }
            Key::Talk(false) => self.gesture.release(now),
            Key::Dictate(name, down) => self.target(DictationTarget::Prompt { name }, down),
            Key::Pick(true) => {
                if self.pick == PickKey::Up {
                    self.pick = if busy || self.gesture.recording() {
                        PickKey::Spent
                    } else {
                        PickKey::Armed
                    };
                }
                self.target(DictationTarget::Picker, true)
            }
            Key::Pick(false) => {
                self.target(DictationTarget::Picker, false);
                let armed = std::mem::take(&mut self.pick) == PickKey::Armed;
                (armed && !busy).then_some(ClientRequest::Pick)
            }
        }
    }

    /// A dictation goes to the target key held when it starts, or to the last
    /// one pressed during it.
    fn target(&mut self, target: DictationTarget, down: bool) -> Option<ClientRequest> {
        if !down {
            if self.held.as_ref() == Some(&target) {
                self.held = None;
            }
            return None;
        }
        if self.held.as_ref() == Some(&target) {
            return None;
        }
        self.held = Some(target.clone());
        self.gesture
            .recording()
            .then_some(ClientRequest::DictateTo { to: target })
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
                Some(ClientRequest::DictateStart { to: None })
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

    fn recording(&self) -> bool {
        !matches!(self, Gesture::Idle | Gesture::Spent)
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
    use ClientRequest::Pick;

    const START: ClientRequest = ClientRequest::DictateStart { to: None };
    const STOP: ClientRequest = ClientRequest::DictateStop;

    fn agent_target() -> DictationTarget {
        DictationTarget::Prompt {
            name: "Agent".into(),
        }
    }

    fn start_to(to: DictationTarget) -> Option<ClientRequest> {
        Some(ClientRequest::DictateStart { to: Some(to) })
    }

    fn redirect(to: DictationTarget) -> Option<ClientRequest> {
        Some(ClientRequest::DictateTo { to })
    }

    /// Feed `keys` half a second apart (no taps) to fresh `Chords`, returning what each one sent.
    fn play(keys: &[Key]) -> Vec<Option<ClientRequest>> {
        let t = Instant::now();
        let mut chords = Chords::default();
        keys.iter()
            .enumerate()
            .map(|(i, key)| chords.key(key.clone(), ms(t, 500 * i as u64), false))
            .collect()
    }

    fn agent(down: bool) -> Key {
        Key::Dictate("Agent".into(), down)
    }

    fn ms(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn a_hold_dictates_until_release() {
        let t = Instant::now();
        let mut gesture = Gesture::default();
        assert_eq!(gesture.press(t, false), Some(START));
        assert_eq!(gesture.release(ms(t, 1200)), Some(STOP));
        assert_eq!(gesture, Gesture::Idle);
    }

    #[test]
    fn a_double_tap_locks_until_the_next_press() {
        let t = Instant::now();
        let mut gesture = Gesture::default();
        assert_eq!(gesture.press(t, false), Some(START));
        assert_eq!(gesture.release(ms(t, 100)), None);
        assert_eq!(gesture.press(ms(t, 200), false), None);
        assert_eq!(gesture.release(ms(t, 300)), None);
        assert_eq!(gesture, Gesture::Locked);
        assert_eq!(gesture.deadline(), None);

        assert_eq!(gesture.press(ms(t, 9000), false), Some(STOP));
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
        assert_eq!(gesture.expire(), Some(STOP));
        assert_eq!(gesture, Gesture::Idle);
    }

    #[test]
    fn a_tap_then_a_hold_is_one_long_hold() {
        let t = Instant::now();
        let mut gesture = Gesture::default();
        gesture.press(t, false);
        gesture.release(ms(t, 100));
        assert_eq!(gesture.press(ms(t, 200), false), None);
        assert_eq!(gesture.release(ms(t, 2000)), Some(STOP));
        assert_eq!(gesture, Gesture::Idle);
    }

    #[test]
    fn repeated_edges_from_several_event_nodes_are_ignored() {
        let t = Instant::now();
        let mut gesture = Gesture::default();
        assert_eq!(gesture.press(t, false), Some(START));
        assert_eq!(gesture.press(t, false), None);
        assert_eq!(gesture.release(ms(t, 1000)), Some(STOP));
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

    #[test]
    fn a_dictate_key_pressed_during_a_hold_redirects_the_dictation_once() {
        use Key::Talk;
        let sent = play(&[
            Talk(true),
            agent(true),
            agent(true),
            agent(false),
            Talk(false),
        ]);
        assert_eq!(
            sent,
            [
                Some(START),
                redirect(agent_target()),
                None,
                None,
                Some(STOP)
            ]
        );
    }

    #[test]
    fn a_dictate_key_held_before_push_to_talk_starts_the_dictation_there() {
        use Key::Talk;
        let sent = play(&[
            agent(true),
            Talk(true),
            Talk(false),
            agent(false),
            Talk(true),
        ]);
        assert_eq!(
            sent,
            [
                None,
                start_to(agent_target()),
                Some(STOP),
                None,
                Some(START)
            ]
        );
    }

    #[test]
    fn the_pick_key_with_push_to_talk_sends_the_dictation_to_the_picker_without_opening_it() {
        use Key::{Pick as PickKey, Talk};
        let during = play(&[Talk(true), PickKey(true), PickKey(false), Talk(false)]);
        assert_eq!(
            during,
            [
                Some(START),
                redirect(DictationTarget::Picker),
                None,
                Some(STOP)
            ]
        );

        let before = play(&[PickKey(true), Talk(true), Talk(false), PickKey(false)]);
        assert_eq!(
            before,
            [None, start_to(DictationTarget::Picker), Some(STOP), None]
        );
    }

    #[test]
    fn a_dictate_key_alone_does_nothing_and_the_pick_key_alone_opens_the_picker_on_release() {
        assert_eq!(play(&[agent(true), agent(false)]), [None, None]);
        assert_eq!(
            play(&[
                Key::Pick(true),
                Key::Pick(true),
                Key::Pick(false),
                Key::Pick(false)
            ]),
            [None, None, Some(Pick), None]
        );
    }

    #[test]
    fn the_pick_key_does_nothing_while_busy() {
        let t = Instant::now();
        let mut chords = Chords::default();
        assert_eq!(chords.key(Key::Pick(true), t, true), None);
        assert_eq!(chords.key(Key::Pick(false), ms(t, 50), false), None);
        chords.key(Key::Pick(true), ms(t, 100), false);
        assert_eq!(chords.key(Key::Pick(false), ms(t, 150), true), None);
    }
}
