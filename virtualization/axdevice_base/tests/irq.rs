// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");

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

struct MockIrqSink {
    events: Mutex<Vec<IrqEvent>>,
    error: Option<IrqError>,
}

impl MockIrqSink {
    fn new(error: Option<IrqError>) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            error,
        }
    }

    fn events(&self) -> Vec<IrqEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl WiredIrqSink for MockIrqSink {
    fn set_level(&self, input: ControllerInputId, asserted: bool) -> IrqResult {
        if let Some(error) = self.error.clone() {
            return Err(error);
        }
        self.events
            .lock()
            .unwrap()
            .push(IrqEvent::SetLevel(input, asserted));
        Ok(())
    }

    fn pulse(&self, input: ControllerInputId) -> IrqResult {
        if let Some(error) = self.error.clone() {
            return Err(error);
        }
        self.events.lock().unwrap().push(IrqEvent::Pulse(input));
        Ok(())
    }
}

fn input(
    input: ControllerInputId,
    trigger: InterruptTriggerMode,
    sink: Arc<MockIrqSink>,
) -> WiredIrqInput {
    WiredIrqInput::new(InterruptControllerId::new(0), input, trigger, sink)
}

#[test]
fn edge_line_pulses_sink() {
    let sink = Arc::new(MockIrqSink::new(None));
    let line = input(
        ControllerInputId::new(4),
        InterruptTriggerMode::EdgeTriggered,
        sink.clone(),
    )
    .connect()
    .unwrap();

    assert_eq!(line.pulse(), Ok(()));
    assert_eq!(
        sink.events(),
        vec![IrqEvent::Pulse(ControllerInputId::new(4))]
    );
}

#[test]
fn level_line_raises_and_lowers_sink() {
    let sink = Arc::new(MockIrqSink::new(None));
    let line = input(
        ControllerInputId::new(33),
        InterruptTriggerMode::LevelTriggered,
        sink.clone(),
    )
    .connect()
    .unwrap();

    assert_eq!(line.raise(), Ok(()));
    assert_eq!(line.lower(), Ok(()));
    assert_eq!(
        sink.events(),
        vec![
            IrqEvent::SetLevel(ControllerInputId::new(33), true),
            IrqEvent::SetLevel(ControllerInputId::new(33), false),
        ]
    );
}

#[test]
fn mismatched_line_operations_return_invalid_input() {
    let sink = Arc::new(MockIrqSink::new(None));
    let edge_line = input(
        ControllerInputId::new(4),
        InterruptTriggerMode::EdgeTriggered,
        sink.clone(),
    )
    .connect()
    .unwrap();
    let level_line = input(
        ControllerInputId::new(33),
        InterruptTriggerMode::LevelTriggered,
        sink.clone(),
    )
    .connect()
    .unwrap();

    assert!(matches!(
        edge_line.raise(),
        Err(IrqError::InvalidTriggerMode {
            operation: "raise",
            ..
        })
    ));
    assert!(matches!(
        edge_line.lower(),
        Err(IrqError::InvalidTriggerMode {
            operation: "lower",
            ..
        })
    ));
    assert!(matches!(
        level_line.pulse(),
        Err(IrqError::InvalidTriggerMode {
            operation: "pulse",
            ..
        })
    ));
    assert!(sink.events().is_empty());
}

#[test]
fn sink_errors_are_propagated() {
    let endpoint = InterruptEndpoint::Wired {
        controller: InterruptControllerId::new(0),
        input: ControllerInputId::new(4),
    };
    let backend_error = IrqError::Backend {
        endpoint,
        operation: "signal",
        detail: "controller unavailable".into(),
    };
    let sink = Arc::new(MockIrqSink::new(Some(backend_error.clone())));
    let edge_line = input(
        ControllerInputId::new(4),
        InterruptTriggerMode::EdgeTriggered,
        sink.clone(),
    )
    .connect()
    .unwrap();
    let level_line = input(
        ControllerInputId::new(33),
        InterruptTriggerMode::LevelTriggered,
        sink,
    )
    .connect()
    .unwrap();

    assert_eq!(edge_line.pulse(), Err(backend_error.clone()));
    assert_eq!(level_line.raise(), Err(backend_error));
    assert_eq!(level_line.lower(), Ok(()));
}
