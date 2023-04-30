use std::time::Duration;

use evdev::{InputEvent, Key};

use crate::event::{KeyEvent, KeyValue, RelativeEvent};

#[derive(Debug)]
pub struct RelativeEventAction { code: u16, value: i32 }

// Input to ActionDispatcher. This should only contain things that are easily testable.
#[derive(Debug)]
pub enum Action {
    // InputEvent (EventType::KEY) sent to evdev
    KeyEvent { key: Key, value: KeyValue },
    // InputEvent (EventType::RELATIVE, NOT mouse movement events) sent to evdev
    RelativeEvent(RelativeEventAction),
    // InputEvent (EventType::RELATIVE, ONLY mouse movement events) a collection of mouse movement sent to evdev
    MouseMovementEventCollection(Vec<RelativeEventAction>),
    // InputEvent of any event types. It's discouraged to use this for testing because
    // we don't have full control over timeval and it's not pattern-matching friendly.
    InputEvent(InputEvent),
    // Run a command
    Command(Vec<String>),
    // keypress_delay_ms
    Delay(Duration),
}
