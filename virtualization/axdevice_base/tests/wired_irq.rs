use std::sync::{Arc, Mutex};

use axdevice_base::{
    ControllerInputId, InterruptControllerId, InterruptEndpoint, InterruptTriggerMode, IrqError,
    IrqResult, WiredIrqInput, WiredIrqSink,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IrqEvent {
    SetLevel(ControllerInputId, bool),
    Pulse(ControllerInputId),
}

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<IrqEvent>>,
}

impl RecordingSink {
    fn events(&self) -> Vec<IrqEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl WiredIrqSink for RecordingSink {
    fn set_level(&self, input: ControllerInputId, asserted: bool) -> IrqResult {
        self.events
            .lock()
            .unwrap()
            .push(IrqEvent::SetLevel(input, asserted));
        Ok(())
    }

    fn pulse(&self, input: ControllerInputId) -> IrqResult {
        self.events.lock().unwrap().push(IrqEvent::Pulse(input));
        Ok(())
    }
}

#[test]
fn shared_level_sources_use_wired_or_semantics() {
    let sink = Arc::new(RecordingSink::default());
    let input = WiredIrqInput::new(
        InterruptControllerId::new(2),
        ControllerInputId::new(41),
        InterruptTriggerMode::LevelTriggered,
        sink.clone(),
    );
    let first = input.connect().unwrap();
    let second = input.connect().unwrap();

    first.raise().unwrap();
    second.raise().unwrap();
    first.lower().unwrap();
    assert_eq!(
        sink.events(),
        vec![IrqEvent::SetLevel(ControllerInputId::new(41), true)]
    );

    second.lower().unwrap();
    assert_eq!(
        sink.events(),
        vec![
            IrqEvent::SetLevel(ControllerInputId::new(41), true),
            IrqEvent::SetLevel(ControllerInputId::new(41), false),
        ]
    );
}

#[test]
fn dropping_an_asserted_source_releases_the_aggregate_level() {
    let sink = Arc::new(RecordingSink::default());
    let input = WiredIrqInput::new(
        InterruptControllerId::new(0),
        ControllerInputId::new(33),
        InterruptTriggerMode::LevelTriggered,
        sink.clone(),
    );

    let line = input.connect().unwrap();
    line.raise().unwrap();
    drop(line);

    assert_eq!(
        sink.events(),
        vec![
            IrqEvent::SetLevel(ControllerInputId::new(33), true),
            IrqEvent::SetLevel(ControllerInputId::new(33), false),
        ]
    );
}

#[test]
fn trigger_mismatch_reports_the_typed_endpoint() {
    let sink = Arc::new(RecordingSink::default());
    let input = WiredIrqInput::new(
        InterruptControllerId::new(7),
        ControllerInputId::new(9),
        InterruptTriggerMode::EdgeTriggered,
        sink,
    );
    let line = input.connect().unwrap();

    assert!(matches!(
        line.raise(),
        Err(IrqError::InvalidTriggerMode {
            endpoint: InterruptEndpoint::Wired {
                controller,
                input,
            },
            operation: "raise",
            ..
        }) if controller == InterruptControllerId::new(7)
            && input == ControllerInputId::new(9)
    ));
}
