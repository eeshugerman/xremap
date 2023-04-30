use evdev::{EventType, InputEvent, Key};

use crate::device::InputDevice;

// Input to EventHandler. This should only contain things that are easily testable.
#[derive(Debug)]
pub enum Event<'a> {
    // InputEvent (EventType::KEY) sent from evdev
    KeyEvent(KeyEvent<'a>),
    // InputEvent (EventType::Relative) sent from evdev
    RelativeEvent(RelativeEvent<'a>),
    // Any other InputEvent type sent from evdev
    OtherEvents(InputEvent),
    // Timer for nested override reached its timeout
    OverrideTimeout,
}

#[derive(Debug)]
pub struct KeyEvent<'a> {
    pub device: &'a InputDevice,
    pub key: Key,
    value: KeyValue,
}

#[derive(Debug)]
pub struct RelativeEvent<'a> {
    pub device: &'a InputDevice,
    pub code: u16,
    pub value: i32,
}

#[derive(Debug)]
pub enum KeyValue {
    Press,
    Release,
    Repeat,
}
impl<'a> Event<'a> {
    // Convert evdev's raw InputEvent to xremap's internal Event
    pub fn new(device: &InputDevice, event: InputEvent) -> Event {
        let event = match event.event_type() {
            EventType::KEY => Event::KeyEvent(KeyEvent::new_with(device, event.code(), event.value())),
            EventType::RELATIVE => Event::RelativeEvent(RelativeEvent::new_with(device, event.code(), event.value())),
            _ => Event::OtherEvents(event),
        };
        event
    }
}

impl<'a> KeyEvent<'a> {
    // Constructor with newer interface
    pub fn new(device: &'a InputDevice, key: Key, value: KeyValue) -> KeyEvent {
        KeyEvent { device, key, value }
    }

    // Constructor with legacy interface
    pub fn new_with(device: &InputDevice, code: u16, value: i32) -> KeyEvent {
        let key = Key::new(code);
        let value = KeyValue::new(value).unwrap();
        KeyEvent::new(device, key, value)
    }

    pub fn code(&self) -> u16 {
        self.key.code()
    }

    pub fn value(&self) -> i32 {
        self.value.value()
    }
}

// constructor for relative events.
impl<'a> RelativeEvent<'a> {
    pub fn new_with(device: &'a InputDevice, code: u16, value: i32) -> RelativeEvent {
        RelativeEvent { device, code, value }
    }
}

impl KeyValue {
    pub fn new(value: i32) -> Option<KeyValue> {
        let event_value = match value {
            0 => KeyValue::Release,
            1 => KeyValue::Press,
            2 => KeyValue::Repeat,
            _ => return None,
        };
        Some(event_value)
    }

    fn value(&self) -> i32 {
        match self {
            KeyValue::Release => 0,
            KeyValue::Press => 1,
            KeyValue::Repeat => 2,
        }
    }
}
