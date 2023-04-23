use crate::action::Action;
use crate::client::WMClient;
use crate::config::application::Application;
use crate::config::key_press::{KeyPress, Modifier};
use crate::config::keymap::{build_override_table, OverrideEntry};
use crate::config::keymap_action::KeymapAction;
use crate::config::modmap_action::{ModmapAction, MultiPurposeKey, PressReleaseKey};
use crate::config::remap::Remap;
use crate::device::InputDevice;
use crate::event::{Event, KeyEvent, RelativeEvent};
use crate::Config;
use evdev::Key;
use lazy_static::lazy_static;
use log::debug;
use nix::sys::time::TimeSpec;
use nix::sys::timerfd::{Expiration, TimerFd, TimerSetTimeFlags};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::time::{Duration, Instant};

// This const is a value used to offset RELATIVE events' scancodes
// so that they correspond to the custom aliases created in config::key::parse_key.
// This offset also prevents resulting scancodes from corresponding to non-Xremap scancodes,
// to prevent conflating disguised relative events with other events.
pub const DISGUISED_EVENT_OFFSETTER: u16 = 59974;

pub struct EventHandler {
    // Currently pressed modifier keys
    modifiers: HashSet<Key>,
    // Modifiers that are currently pressed but not in the source KeyPress
    extra_modifiers: HashSet<Key>,
    // Make sure the original event is released even if remapping changes while holding the key
    pressed_keys: HashMap<Key, Key>,
    // Check the currently active application
    application_client: WMClient,
    application_cache: Option<String>,
    // State machine for multi-purpose keys
    multi_purpose_keys: HashMap<Key, MultiPurposeKeyState>,
    // Current nested remaps
    override_remaps: Vec<HashMap<Key, Vec<OverrideEntry>>>,
    // Key triggered on a timeout of nested remaps
    override_timeout_key: Option<Key>,
    // Trigger a timeout of nested remaps through select(2)
    override_timer: TimerFd,
    // { set_mode: String }
    mode: String,
    // { set_mark: true }
    mark_set: bool,
    // { escape_next_key: true }
    escape_next_key: bool,
    // keypress_delay_ms
    keypress_delay: Duration,
    // Buffered actions to be dispatched. TODO: Just return actions from each function instead of using this.
    actions: Vec<Action>,
}

struct TaggedAction {
    action: KeymapAction,
    exact_match: bool,
}

impl EventHandler {
    pub fn new(timer: TimerFd, mode: &str, keypress_delay: Duration, application_client: WMClient) -> EventHandler {
        EventHandler {
            modifiers: HashSet::new(),
            extra_modifiers: HashSet::new(),
            pressed_keys: HashMap::new(),
            application_client,
            application_cache: None,
            multi_purpose_keys: HashMap::new(),
            override_remaps: vec![],
            override_timeout_key: None,
            override_timer: timer,
            mode: mode.to_string(),
            mark_set: false,
            escape_next_key: false,
            keypress_delay,
            actions: vec![],
        }
    }

    // Handle an Event and return Actions. This should be the only public method of EventHandler.
    pub fn on_events(&mut self, events: &Vec<Event>, config: &Config, input_device: &InputDevice) -> Result<Vec<Action>, Box<dyn Error>> {
        // a vector to collect mouse movement events to be able to send them all at once as one MouseMovementEventCollection.
        let mut mouse_movement_collection: Vec<RelativeEvent> = Vec::new();
        for event in events {
            match event {
                Event::KeyEvent(key_event) => {
                    self.on_key_event(key_event, config, input_device)?;
                    ()
                }
                Event::RelativeEvent(relative_event) => {
                    self.on_relative_event(relative_event, &mut mouse_movement_collection, config, input_device)?
                }

                Event::OtherEvents(event) => self.send_action(Action::InputEvent(*event)),
                Event::OverrideTimeout => self.timeout_override()?,
            };
        }
        // if there is at least one mouse movement event, sending all of them as one MouseMovementEventCollection
        if mouse_movement_collection.len() > 0 {
            self.send_action(Action::MouseMovementEventCollection(mouse_movement_collection));
        }
        Ok(self.actions.drain(..).collect())
    }

    // Handle EventType::KEY
    fn on_key_event(&mut self, event: &KeyEvent, config: &Config, input_device: &InputDevice) -> Result<bool, Box<dyn Error>> {
        self.application_cache = None; // expire cache
        let key = Key::new(event.code());
        debug!("=> {}: {:?}", event.value(), &key);

        // Apply modmap
        let mut key_values = if let Some(key_action) = self.find_modmap(config, &key, input_device) {
            self.dispatch_keys(key_action, key, event.value())?
        } else {
            vec![(key, event.value())]
        };
        self.maintain_pressed_keys(key, event.value(), &mut key_values);
        if !self.multi_purpose_keys.is_empty() {
            key_values = self.flush_timeout_keys(key_values);
        }

        let mut send_original_relative_event = false;
        // Apply keymap
        for (key, value) in key_values.into_iter() {
            if config.virtual_modifiers.contains(&key) {
                self.update_modifier(key, value);
                continue;
            } else if MODIFIER_KEYS.contains(&key) {
                self.update_modifier(key, value);
            } else if is_pressed(value) {
                if self.escape_next_key {
                    self.escape_next_key = false
                } else if let Some(actions) = self.find_keymap(config, &key, input_device)? {
                    self.dispatch_actions(&actions, &key)?;
                    continue;
                }
            }
            // checking if there's a "disguised" key version of a relative event,
            // (scancodes equal to and over DISGUISED_EVENT_OFFSETTER are only "disguised" custom events)
            // and also if it's the same "key" and value as the one that came in.
            if key.code() >= DISGUISED_EVENT_OFFSETTER && (key.code(), value) == (event.code(), event.value()) {
                // if it is, setting send_original_relative_event to true to later tell on_relative_event to send the original event.
                send_original_relative_event = true;
                continue;
            }
            self.send_key(&key, value);
        }

        // Using the Ok() to send a boolean to on_relative_event, which will be used to decide whether to send the original relative event.
        // (True = send the original relative event, false = don't send it.)
        Ok(send_original_relative_event)
    }

    // Handle EventType::RELATIVE
    fn on_relative_event(
        &mut self,
        event: &RelativeEvent,
        mouse_movement_collection: &mut Vec<RelativeEvent>,
        config: &Config,
        input_device: &InputDevice
    ) -> Result<(), Box<dyn Error>> {
        // Because a "full" RELATIVE event is only one event,
        // it doesn't translate very well into a KEY event (because those have a "press" event and an "unpress" event).
        // The solution used here is to send two events for each relative event :
        // one for the press "event" and one for the "unpress" event.

        // These consts are used because 'RELEASE'/'PRESS' are better than '0'/'1' at indicating a button release/press.
        const RELEASE: i32 = 0;
        const PRESS: i32 = 1;

        // All relative events (except maybe those i haven't found information about (REL_DIAL, REL_MISC and REL_RESERVED))
        // can have either a positive value or a negative value.
        // A negative value is associated with a different action than the positive value.
        // Specifically, negative values are associated with the opposite of the action that would emit a positive value.
        // For example, a positive value for a scroll event (REL_WHEEL) comes from an upscroll, while a negative value comes from a downscroll.
        let key = match event.value {
            // Positive and negative values can be really high because the events are relative,
            // so their values are variable, meaning we have to match with all positive/negative values.
            // Not sure if there is any relative event with a fixed value.
            1..=i32::MAX => (event.code * 2) + DISGUISED_EVENT_OFFSETTER,
            // While some events may appear to have a fixed value,
            // events like scrolling will have higher values with more "agressive" scrolling.

            // *2 to create a "gap" between events (since multiplying by two means that all resulting values will be even, the odd numbers between will be missing),
            // +1 if the event has a negative value to "fill" the gap (since adding one shifts the parity from even to odd),
            // and adding DISGUISED_EVENT_OFFSETTER,
            // so that the total as a keycode corresponds to one of the custom aliases that
            // are created in config::key::parse_key specifically for these "disguised" relative events.
            i32::MIN..=-1 => (event.code * 2) + 1 + DISGUISED_EVENT_OFFSETTER,

            0 => {
                println!("This event has a value of zero : {:?}", event);
                // A value of zero would be unexpected for a relative event,
                // since changing something by zero is kinda useless.
                // Just in case it can actually happen (and also because match arms need the same output type),
                // we'll just act like the value of the event was a positive.
                (event.code * 2) + DISGUISED_EVENT_OFFSETTER
            }
        };

        // Sending a RELATIVE event "disguised" as a "fake" KEY event press to on_key_event.
        match self.on_key_event(&KeyEvent::new_with(key, PRESS), config, input_device)? {
            // the boolean value is from a variable at the end of on_key_event from event_handler,
            // used to indicate whether the event got through unchanged.
            true => {
                // Sending the original RELATIVE event if the "press" version of the "fake" KEY event got through on_key_event unchanged.
                let action = RelativeEvent::new_with(event.code, event.value);
                if event.code <= 2 {
                    // If it's a mouse movement event (event.code <= 2),
                    // it is added to mouse_movement_collection to later be sent alongside all other mouse movement event,
                    // as a single MouseMovementEventCollection instead of potentially multiple RelativeEvent .

                    // Mouse movement events need to be sent all at once because they would otherwise be separated by a synchronization event¹,
                    // which the OS handles differently from two unseparated mouse movement events.
                    // For example, a REL_X event², followed by a SYNCHRONIZATION event, followed by a REL_Y event³, followed by a SYNCHRONIZATION event,
                    // will move the mouse cursor by a different amount than a REL_X followed by a REL_Y followed by a SYNCHRONIZATION.

                    // ¹Because Xremap usually sends events one by one through evdev's "emit" function, which adds a synchronization event during each call.
                    // ²Mouse movement along the X (horizontal) axis.
                    // ³Mouse movement along the Y (vertical) axis.
                    mouse_movement_collection.push(action);
                } else {
                    // Otherwise, the event is directly sent as a relative event, to be dispatched like other events.
                    self.send_action(Action::RelativeEvent(action));
                }
            }
            false => {}
        }

        // Sending the "unpressed" version of the "fake" KEY event.
        self.on_key_event(&KeyEvent::new_with(key, RELEASE), config, input_device)?;

        Ok(())
    }

    fn timeout_override(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(key) = self.override_timeout_key {
            self.send_key(&key, PRESS);
            self.send_key(&key, RELEASE);
        }
        self.remove_override()
    }

    fn remove_override(&mut self) -> Result<(), Box<dyn Error>> {
        self.override_timer.unset()?;
        self.override_remaps.clear();
        self.override_timeout_key = None;
        Ok(())
    }

    fn send_keys(&mut self, keys: &Vec<Key>, value: i32) {
        for key in keys {
            self.send_key(key, value);
        }
    }

    fn send_key(&mut self, key: &Key, value: i32) {
        // let event = InputEvent::new(EventType::KEY, key.code(), value);
        let event = KeyEvent::new_with(key.code(), value);
        self.send_action(Action::KeyEvent(event));
    }

    fn send_action(&mut self, action: Action) {
        self.actions.push(action);
    }

    // Repeat/Release what's originally pressed even if remapping changes while holding it
    fn maintain_pressed_keys(&mut self, key: Key, value: i32, events: &mut Vec<(Key, i32)>) {
        // Not handling multi-purpose keysfor now; too complicated
        if events.len() != 1 || value != events[0].1 {
            return;
        }

        let event = events[0];
        if value == PRESS {
            self.pressed_keys.insert(key, event.0);
        } else {
            if let Some(original_key) = self.pressed_keys.get(&key) {
                events[0].0 = *original_key;
            }
            if value == RELEASE {
                self.pressed_keys.remove(&key);
            }
        }
    }

    fn dispatch_keys(
        &mut self,
        key_action: ModmapAction,
        key: Key,
        value: i32,
    ) -> Result<Vec<(Key, i32)>, Box<dyn Error>> {
        let keys = match key_action {
            ModmapAction::Key(modmap_key) => vec![(modmap_key, value)],
            ModmapAction::MultiPurposeKey(MultiPurposeKey {
                held,
                alone,
                alone_timeout,
            }) => {
                if value == PRESS {
                    self.multi_purpose_keys.insert(
                        key,
                        MultiPurposeKeyState {
                            held,
                            alone,
                            alone_timeout_at: Some(Instant::now() + alone_timeout),
                        },
                    );
                    return Ok(vec![]); // delay the press
                } else if value == REPEAT {
                    if let Some(state) = self.multi_purpose_keys.get_mut(&key) {
                        return Ok(state.repeat());
                    }
                } else if value == RELEASE {
                    if let Some(state) = self.multi_purpose_keys.remove(&key) {
                        return Ok(state.release());
                    }
                } else {
                    panic!("unexpected key event value: {}", value);
                }
                // fallthrough on state discrepancy
                vec![(key, value)]
            }
            ModmapAction::PressReleaseKey(PressReleaseKey { press, release }) => {
                // Just hook actions, and then emit the original event. We might want to
                // support reordering the key event and dispatched actions later.
                if value == PRESS || value == RELEASE {
                    self.dispatch_actions(
                        &(if value == PRESS { press } else { release })
                            .into_iter()
                            .map(|action| TaggedAction {
                                action,
                                exact_match: false,
                            })
                            .collect(),
                        &key,
                    )?;
                }
                // Dispatch the original key as well
                vec![(key, value)]
            }
        };
        Ok(keys)
    }

    fn flush_timeout_keys(&mut self, key_values: Vec<(Key, i32)>) -> Vec<(Key, i32)> {
        let mut flush = false;
        for (_, value) in key_values.iter() {
            if *value == PRESS {
                flush = true;
                break;
            }
        }

        if flush {
            let mut flushed: Vec<(Key, i32)> = vec![];
            for (_, state) in self.multi_purpose_keys.iter_mut() {
                flushed.extend(state.force_held());
            }
            flushed.extend(key_values);
            flushed
        } else {
            key_values
        }
    }

    fn find_modmap(&mut self, config: &Config, key: &Key, input_device: &InputDevice) -> Option<ModmapAction> {
        for modmap in &config.modmap {
            if let Some(key_action) = modmap.remap.get(key) {
                if let Some(application_matcher) = &modmap.application {
                    if !self.match_application(application_matcher) {
                        continue;
                    }
                }
                return Some(key_action.clone());
            }
        }
        None
    }

    fn find_keymap(&mut self, config: &Config, key: &Key, input_device: &InputDevice) -> Result<Option<Vec<TaggedAction>>, Box<dyn Error>> {
        if !self.override_remaps.is_empty() {
            let entries: Vec<OverrideEntry> = self
                .override_remaps
                .iter()
                .flat_map(|map| map.get(key).cloned().unwrap_or_default())
                .collect();

            if !entries.is_empty() {
                self.remove_override()?;

                for exact_match in [true, false] {
                    let mut remaps = vec![];
                    for entry in &entries {
                        if entry.exact_match && !exact_match {
                            continue;
                        }
                        let (extra_modifiers, missing_modifiers) = self.diff_modifiers(&entry.modifiers);
                        if (exact_match && extra_modifiers.len() > 0) || missing_modifiers.len() > 0 {
                            continue;
                        }

                        let actions = with_extra_modifiers(&entry.actions, &extra_modifiers, entry.exact_match);
                        let is_remap = is_remap(&entry.actions);

                        // If the first/top match was a remap, continue to find rest of the eligible remaps for this key
                        if remaps.is_empty() && !is_remap {
                            return Ok(Some(actions));
                        } else if is_remap {
                            remaps.extend(actions);
                        }
                    }
                    if !remaps.is_empty() {
                        return Ok(Some(remaps));
                    }
                }
            }
            // An override remap is set but not used. Flush the pending key.
            self.timeout_override()?;
        }

        if let Some(entries) = config.keymap_table.get(key) {
            for exact_match in [true, false] {
                let mut remaps = vec![];
                for entry in entries {
                    if entry.exact_match && !exact_match {
                        continue;
                    }
                    let (extra_modifiers, missing_modifiers) = self.diff_modifiers(&entry.modifiers);
                    if (exact_match && extra_modifiers.len() > 0) || missing_modifiers.len() > 0 {
                        continue;
                    }
                    if let Some(application_matcher) = &entry.application {
                        if !self.match_application(application_matcher) {
                            continue;
                        }
                    }
                    if let Some(modes) = &entry.mode {
                        if !modes.contains(&self.mode) {
                            continue;
                        }
                    }

                    let actions = with_extra_modifiers(&entry.actions, &extra_modifiers, entry.exact_match);
                    let is_remap = is_remap(&entry.actions);

                    // If the first/top match was a remap, continue to find rest of the eligible remaps for this key
                    if remaps.is_empty() && !is_remap {
                        return Ok(Some(actions));
                    } else if is_remap {
                        remaps.extend(actions)
                    }
                }
                if !remaps.is_empty() {
                    return Ok(Some(remaps));
                }
            }
        }
        Ok(None)
    }

    fn dispatch_actions(&mut self, actions: &Vec<TaggedAction>, key: &Key) -> Result<(), Box<dyn Error>> {
        for action in actions {
            self.dispatch_action(action, key)?;
        }
        Ok(())
    }

    fn dispatch_action(&mut self, action: &TaggedAction, key: &Key) -> Result<(), Box<dyn Error>> {
        match &action.action {
            KeymapAction::KeyPress(key_press) => self.send_key_press(key_press),
            KeymapAction::Remap(Remap {
                remap,
                timeout,
                timeout_key,
            }) => {
                let set_timeout = self.override_remaps.is_empty();
                self.override_remaps
                    .push(build_override_table(remap, action.exact_match));

                // Set timeout only if this is the first of multiple eligible remaps,
                // so the behaviour is consistent with how current normal keymap override works
                if set_timeout {
                    if let Some(timeout) = timeout {
                        let expiration = Expiration::OneShot(TimeSpec::from_duration(*timeout));
                        // TODO: Consider handling the timer in ActionDispatcher
                        self.override_timer.unset()?;
                        self.override_timer.set(expiration, TimerSetTimeFlags::empty())?;
                        self.override_timeout_key = timeout_key.or_else(|| Some(*key));
                    }
                }
            }
            KeymapAction::Launch(command) => self.run_command(command.clone()),
            KeymapAction::SetMode(mode) => {
                self.mode = mode.clone();
                println!("mode: {}", mode);
            }
            KeymapAction::SetMark(set) => self.mark_set = *set,
            KeymapAction::WithMark(key_press) => self.send_key_press(&self.with_mark(key_press)),
            KeymapAction::EscapeNextKey(escape_next_key) => self.escape_next_key = *escape_next_key,
            KeymapAction::SetExtraModifiers(keys) => {
                self.extra_modifiers.clear();
                for key in keys {
                    self.extra_modifiers.insert(*key);
                }
            }
        }
        Ok(())
    }

    fn send_key_press(&mut self, key_press: &KeyPress) {
        // Build extra or missing modifiers. Note that only MODIFIER_KEYS are handled
        // because logical modifiers shouldn't make an impact outside xremap.
        let (mut extra_modifiers, mut missing_modifiers) = self.diff_modifiers(&key_press.modifiers);
        extra_modifiers.retain(|key| MODIFIER_KEYS.contains(&key) && !self.extra_modifiers.contains(&key));
        missing_modifiers.retain(|key| MODIFIER_KEYS.contains(&key));

        // Emulate the modifiers of KeyPress
        self.send_keys(&missing_modifiers, PRESS);
        self.send_keys(&extra_modifiers, RELEASE);

        // Press the main key
        self.send_key(&key_press.key, PRESS);
        self.send_key(&key_press.key, RELEASE);

        self.send_action(Action::Delay(self.keypress_delay));

        // Resurrect the original modifiers
        self.send_keys(&extra_modifiers, PRESS);
        self.send_keys(&missing_modifiers, RELEASE);
    }

    fn with_mark(&self, key_press: &KeyPress) -> KeyPress {
        if self.mark_set && !self.match_modifier(&Modifier::Shift) {
            let mut modifiers = key_press.modifiers.clone();
            modifiers.push(Modifier::Shift);
            KeyPress {
                key: key_press.key,
                modifiers,
            }
        } else {
            key_press.clone()
        }
    }

    fn run_command(&mut self, command: Vec<String>) {
        self.send_action(Action::Command(command));
    }

    // Return (extra_modifiers, missing_modifiers)
    fn diff_modifiers(&self, modifiers: &Vec<Modifier>) -> (Vec<Key>, Vec<Key>) {
        let extra_modifiers: Vec<Key> = self
            .modifiers
            .iter()
            .filter(|modifier| !contains_modifier(modifiers, modifier))
            .map(|modifier| modifier.clone())
            .collect();
        let missing_modifiers: Vec<Key> = modifiers
            .iter()
            .filter_map(|modifier| {
                if self.match_modifier(modifier) {
                    None
                } else {
                    match modifier {
                        Modifier::Shift => Some(Key::KEY_LEFTSHIFT),
                        Modifier::Control => Some(Key::KEY_LEFTCTRL),
                        Modifier::Alt => Some(Key::KEY_LEFTALT),
                        Modifier::Windows => Some(Key::KEY_LEFTMETA),
                        Modifier::Key(key) => Some(*key),
                    }
                }
            })
            .collect();
        return (extra_modifiers, missing_modifiers);
    }

    fn match_modifier(&self, modifier: &Modifier) -> bool {
        match modifier {
            Modifier::Shift => {
                self.modifiers.contains(&Key::KEY_LEFTSHIFT) || self.modifiers.contains(&Key::KEY_RIGHTSHIFT)
            }
            Modifier::Control => {
                self.modifiers.contains(&Key::KEY_LEFTCTRL) || self.modifiers.contains(&Key::KEY_RIGHTCTRL)
            }
            Modifier::Alt => self.modifiers.contains(&Key::KEY_LEFTALT) || self.modifiers.contains(&Key::KEY_RIGHTALT),
            Modifier::Windows => {
                self.modifiers.contains(&Key::KEY_LEFTMETA) || self.modifiers.contains(&Key::KEY_RIGHTMETA)
            }
            Modifier::Key(key) => self.modifiers.contains(key),
        }
    }

    fn match_application(&mut self, application_matcher: &Application) -> bool {
        // Lazily fill the wm_class cache
        if self.application_cache.is_none() {
            match self.application_client.current_application() {
                Some(application) => self.application_cache = Some(application),
                None => self.application_cache = Some(String::new()),
            }
        }

        if let Some(application) = &self.application_cache {
            if let Some(application_only) = &application_matcher.only {
                return application_only.iter().any(|m| m.matches(application));
            }
            if let Some(application_not) = &application_matcher.not {
                return application_not.iter().all(|m| !m.matches(application));
            }
        }
        false
    }

    fn update_modifier(&mut self, key: Key, value: i32) {
        if value == PRESS {
            self.modifiers.insert(key);
        } else if value == RELEASE {
            self.modifiers.remove(&key);
        }
    }
}

fn is_remap(actions: &Vec<KeymapAction>) -> bool {
    actions.iter().all(|x| match x {
        KeymapAction::Remap(..) => true,
        _ => false,
    })
}

fn with_extra_modifiers(
    actions: &Vec<KeymapAction>,
    extra_modifiers: &Vec<Key>,
    exact_match: bool,
) -> Vec<TaggedAction> {
    let mut result: Vec<TaggedAction> = vec![];
    if extra_modifiers.len() > 0 {
        // Virtually release extra modifiers so that they won't be physically released on KeyPress
        result.push(TaggedAction {
            action: KeymapAction::SetExtraModifiers(extra_modifiers.clone()),
            exact_match,
        });
    }
    result.extend(actions.iter().map(|action| TaggedAction {
        action: action.clone(),
        exact_match,
    }));
    if extra_modifiers.len() > 0 {
        // Resurrect the modifier status
        result.push(TaggedAction {
            action: KeymapAction::SetExtraModifiers(vec![]),
            exact_match,
        });
    }
    return result;
}

fn contains_modifier(modifiers: &Vec<Modifier>, key: &Key) -> bool {
    for modifier in modifiers {
        if match modifier {
            Modifier::Shift => key == &Key::KEY_LEFTSHIFT || key == &Key::KEY_RIGHTSHIFT,
            Modifier::Control => key == &Key::KEY_LEFTCTRL || key == &Key::KEY_RIGHTCTRL,
            Modifier::Alt => key == &Key::KEY_LEFTALT || key == &Key::KEY_RIGHTALT,
            Modifier::Windows => key == &Key::KEY_LEFTMETA || key == &Key::KEY_RIGHTMETA,
            Modifier::Key(modifier_key) => key == modifier_key,
        } {
            return true;
        }
    }
    false
}

lazy_static! {
    static ref MODIFIER_KEYS: [Key; 8] = [
        // Shift
        Key::KEY_LEFTSHIFT,
        Key::KEY_RIGHTSHIFT,
        // Control
        Key::KEY_LEFTCTRL,
        Key::KEY_RIGHTCTRL,
        // Alt
        Key::KEY_LEFTALT,
        Key::KEY_RIGHTALT,
        // Windows
        Key::KEY_LEFTMETA,
        Key::KEY_RIGHTMETA,
    ];
}

// ---

fn is_pressed(value: i32) -> bool {
    value == PRESS || value == REPEAT
}

// InputEvent#value
static RELEASE: i32 = 0;
static PRESS: i32 = 1;
static REPEAT: i32 = 2;

// ---

#[derive(Debug)]
struct MultiPurposeKeyState {
    held: Key,
    alone: Key,
    // Some if the first press is still delayed, None if already considered held.
    alone_timeout_at: Option<Instant>,
}

impl MultiPurposeKeyState {
    fn repeat(&mut self) -> Vec<(Key, i32)> {
        if let Some(alone_timeout_at) = &self.alone_timeout_at {
            if Instant::now() < *alone_timeout_at {
                vec![] // still delay the press
            } else {
                self.alone_timeout_at = None; // timeout
                vec![(self.held, PRESS)]
            }
        } else {
            vec![(self.held, REPEAT)]
        }
    }

    fn release(&self) -> Vec<(Key, i32)> {
        if let Some(alone_timeout_at) = &self.alone_timeout_at {
            if Instant::now() < *alone_timeout_at {
                // dispatch the delayed press and this release
                vec![(self.alone, PRESS), (self.alone, RELEASE)]
            } else {
                // too late. dispatch the held key
                vec![(self.held, PRESS), (self.held, RELEASE)]
            }
        } else {
            vec![(self.held, RELEASE)]
        }
    }

    fn force_held(&mut self) -> Vec<(Key, i32)> {
        if self.alone_timeout_at.is_some() {
            self.alone_timeout_at = None;
            vec![(self.held, PRESS)]
        } else {
            vec![]
        }
    }
}
