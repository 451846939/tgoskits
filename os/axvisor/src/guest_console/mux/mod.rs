//! Host-console multiplexing for mandatory guest virtual serial devices.

use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

use anyhow::{Result, bail};
use ax_std::os::arceos::modules::ax_sync::spin::SpinNoPreempt;
use axvm::{SerialBackend, SerialBackendFactory, VMId, VmStatus};
use core::{
    ops::Bound::{Excluded, Unbounded},
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use heapless::Deque;
use std::sync::{LazyLock, Mutex, MutexGuard};

use super::host::write_host_bytes;

mod output;

use output::GuestOutputMux;

// Terminals encode Alt as a leading ESC, so Ctrl-Alt-[ is ESC ESC and
// Ctrl-Alt-] is ESC followed by the Ctrl-] byte (group separator).
const ESC: u8 = 0x1b;
const CTRL_H: u8 = 0x08;
const CTRL_RIGHT_BRACKET: u8 = 0x1d;
const INPUT_QUEUE_CAPACITY: usize = 4096;
const OUTPUT_FRAME_BYTES: usize = 256;
const OUTPUT_QUEUE_CAPACITY: usize = 64;
const OUTPUT_PUBLISH_BUDGET: usize = 4;
const OUTPUT_DRAIN_BUDGET: usize = 32;

static GUEST_CONSOLE_MUX: LazyLock<GuestConsoleMux> = LazyLock::new(GuestConsoleMux::new);

/// Result of routing one byte read from the host console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleInputEvent {
    /// The byte belongs to the Axvisor shell.
    ShellByte(u8),
    /// Two bytes belong to the Axvisor shell in the given order.
    ShellSequence(u8, u8),
    /// The byte was consumed by the attached guest or a shortcut prefix.
    Consumed,
    /// A shortcut attached the named guest.
    Attached(VMId),
    /// A shortcut returned from the named guest to the shell.
    Detached(VMId),
    /// No running guest is available for attachment.
    NoRunningGuest,
}

#[derive(Debug)]
struct RoutedInput {
    event: ConsoleInputEvent,
    wake_vm: Option<VMId>,
}

/// Application-owned host console multiplexer.
///
/// The multiplexer is the only reader of the physical host console. Each VM
/// gets a [`SerialBackend`] backed by its own bounded RX queue. Guest output is
/// serialized here before it reaches the host UART.
#[derive(Debug)]
pub struct GuestConsoleMux {
    core: Arc<ConsoleCore>,
}

#[derive(Debug)]
struct ConsoleCore {
    /// Task-context control state. vCPU device callbacks never acquire it.
    state: Mutex<ConsoleState>,
    /// Fixed-capacity publication gate shared by vCPU device callbacks.
    output_ingress: SpinNoPreempt<Deque<GuestOutputFrame, OUTPUT_QUEUE_CAPACITY>>,
    /// Formatting state owned by the task-context console consumer.
    output: Mutex<ConsoleOutputState>,
    running_count: AtomicUsize,
    output_epoch: AtomicU64,
}

#[derive(Debug, Default)]
struct ConsoleState {
    guests: BTreeMap<VMId, GuestState>,
    running: BTreeSet<VMId>,
    attached: Option<VMId>,
    last_attached: Option<VMId>,
    shortcut_prefix_pending: bool,
    next_backend_generation: u64,
}

#[derive(Debug, Default)]
struct ConsoleOutputState {
    mux: GuestOutputMux,
    observed_epoch: u64,
    last_backend: Option<(VMId, BackendGeneration)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackendGeneration(u64);

#[derive(Debug, Default)]
struct GuestState {
    endpoint: Option<Arc<GuestIoEndpoint>>,
}

#[derive(Debug)]
struct GuestIoEndpoint {
    generation: BackendGeneration,
    active: AtomicBool,
    input: SpinNoPreempt<VecDeque<u8>>,
}

#[derive(Clone, Copy, Debug)]
struct GuestOutputFrame {
    vm_id: VMId,
    generation: BackendGeneration,
    len: u16,
    bytes: [u8; OUTPUT_FRAME_BYTES],
}

#[derive(Debug)]
struct GuestSerialBackend {
    vm_id: VMId,
    endpoint: Arc<GuestIoEndpoint>,
    core: Arc<ConsoleCore>,
}

#[derive(Debug)]
struct GuestSerialBackendFactory {
    vm_id: VMId,
    core: Arc<ConsoleCore>,
}

impl GuestConsoleMux {
    fn new() -> Self {
        Self {
            core: Arc::new(ConsoleCore {
                state: Mutex::new(ConsoleState::default()),
                output_ingress: SpinNoPreempt::new(Deque::new()),
                output: Mutex::new(ConsoleOutputState::default()),
                running_count: AtomicUsize::new(0),
                output_epoch: AtomicU64::new(0),
            }),
        }
    }

    fn set_running(&self, running: impl IntoIterator<Item = VMId>) -> Option<VMId> {
        let mut state = self.core.lock_state();
        state.running.clear();
        for vm_id in running {
            state.running.insert(vm_id);
            state.guests.entry(vm_id).or_default();
        }

        let detached = state
            .attached
            .filter(|vm_id| !state.running.contains(vm_id));
        if detached.is_some() {
            state.attached = None;
            state.shortcut_prefix_pending = false;
        }
        self.core
            .running_count
            .store(state.running.len(), Ordering::Release);
        self.core.invalidate_output();
        detached
    }

    fn mark_running(&self, vm_id: VMId) {
        let mut state = self.core.lock_state();
        state.running.insert(vm_id);
        state.guests.entry(vm_id).or_default();
        self.core
            .running_count
            .store(state.running.len(), Ordering::Release);
        self.core.invalidate_output();
    }

    fn mark_stopped(&self, vm_id: VMId) -> bool {
        let mut state = self.core.lock_state();
        state.running.remove(&vm_id);
        if let Some(guest) = state.guests.get_mut(&vm_id) {
            if let Some(endpoint) = guest.endpoint.take() {
                endpoint.deactivate();
            }
        }
        self.core
            .running_count
            .store(state.running.len(), Ordering::Release);
        self.core.invalidate_output();
        if state.attached == Some(vm_id) {
            state.attached = None;
            state.shortcut_prefix_pending = false;
            return true;
        }
        false
    }

    fn remove(&self, vm_id: VMId) -> bool {
        let mut state = self.core.lock_state();
        state.running.remove(&vm_id);
        if let Some(guest) = state.guests.remove(&vm_id)
            && let Some(endpoint) = guest.endpoint
        {
            endpoint.deactivate();
        }
        self.core
            .running_count
            .store(state.running.len(), Ordering::Release);
        self.core.invalidate_output();
        if state.last_attached == Some(vm_id) {
            state.last_attached = None;
        }
        if state.attached == Some(vm_id) {
            state.attached = None;
            state.shortcut_prefix_pending = false;
            return true;
        }
        false
    }

    fn attach_default(&self, running: impl IntoIterator<Item = VMId>) -> Option<VMId> {
        self.set_running(running);
        let mut state = self.core.lock_state();
        let vm_id = state.running.first().copied()?;
        state.attached = Some(vm_id);
        state.last_attached = Some(vm_id);
        Some(vm_id)
    }

    fn attach(&self, vm_id: VMId) -> bool {
        let mut state = self.core.lock_state();
        if !state.running.contains(&vm_id) {
            return false;
        }
        state.attached = Some(vm_id);
        state.last_attached = Some(vm_id);
        state.shortcut_prefix_pending = false;
        true
    }

    fn attached_vm(&self) -> Option<VMId> {
        self.core.lock_state().attached
    }

    fn route_host_byte(&self, byte: u8) -> RoutedInput {
        let mut state = self.core.lock_state();

        if state.shortcut_prefix_pending {
            state.shortcut_prefix_pending = false;
            return match byte {
                CTRL_H => match state.attached.take() {
                    Some(vm_id) => RoutedInput {
                        event: ConsoleInputEvent::Detached(vm_id),
                        wake_vm: None,
                    },
                    None => RoutedInput {
                        event: ConsoleInputEvent::Consumed,
                        wake_vm: None,
                    },
                },
                ESC => switch_guest(&mut state, GuestSwitchDirection::Previous),
                CTRL_RIGHT_BRACKET => switch_guest(&mut state, GuestSwitchDirection::Next),
                byte => match state.attached {
                    Some(vm_id) => {
                        enqueue_guest_input(&mut state, vm_id, &[ESC, byte]);
                        RoutedInput {
                            event: ConsoleInputEvent::Consumed,
                            wake_vm: Some(vm_id),
                        }
                    }
                    None => RoutedInput {
                        event: ConsoleInputEvent::ShellSequence(ESC, byte),
                        wake_vm: None,
                    },
                },
            };
        }

        if byte == ESC {
            state.shortcut_prefix_pending = true;
            return RoutedInput {
                event: ConsoleInputEvent::Consumed,
                wake_vm: None,
            };
        }

        match state.attached {
            Some(vm_id) => {
                enqueue_guest_input(&mut state, vm_id, &[byte]);
                RoutedInput {
                    event: ConsoleInputEvent::Consumed,
                    wake_vm: Some(vm_id),
                }
            }
            None => RoutedInput {
                event: ConsoleInputEvent::ShellByte(byte),
                wake_vm: None,
            },
        }
    }
}

#[derive(Clone, Copy)]
enum GuestSwitchDirection {
    Previous,
    Next,
}

fn switch_guest(state: &mut ConsoleState, direction: GuestSwitchDirection) -> RoutedInput {
    let anchor = state.attached.or(state.last_attached);
    let vm_id = match (direction, anchor) {
        (GuestSwitchDirection::Previous, Some(anchor)) => state
            .running
            .range(..anchor)
            .next_back()
            .copied()
            .or_else(|| state.running.last().copied()),
        (GuestSwitchDirection::Next, Some(anchor)) => state
            .running
            .range((Excluded(anchor), Unbounded))
            .next()
            .copied()
            .or_else(|| state.running.first().copied()),
        (GuestSwitchDirection::Previous, None) => state.running.last().copied(),
        (GuestSwitchDirection::Next, None) => state.running.first().copied(),
    };
    let Some(vm_id) = vm_id else {
        return RoutedInput {
            event: ConsoleInputEvent::NoRunningGuest,
            wake_vm: None,
        };
    };

    state.attached = Some(vm_id);
    state.last_attached = Some(vm_id);
    RoutedInput {
        event: ConsoleInputEvent::Attached(vm_id),
        wake_vm: None,
    }
}

impl ConsoleCore {
    fn lock_state(&self) -> MutexGuard<'_, ConsoleState> {
        self.state
            .lock()
            .expect("guest console state mutex poisoned")
    }

    fn lock_output(&self) -> MutexGuard<'_, ConsoleOutputState> {
        self.output
            .lock()
            .expect("guest console output mutex poisoned")
    }

    fn create_serial_backend(self: &Arc<Self>, vm_id: VMId) -> Arc<GuestSerialBackend> {
        let endpoint = {
            let mut state = self.lock_state();
            state.next_backend_generation = state
                .next_backend_generation
                .checked_add(1)
                .expect("guest serial backend generation exhausted");
            let generation = BackendGeneration(state.next_backend_generation);
            let endpoint = Arc::new(GuestIoEndpoint::new(generation));
            if let Some(previous) = state
                .guests
                .get_mut(&vm_id)
                .and_then(|guest| guest.endpoint.take())
            {
                previous.deactivate();
            }
            state.guests.insert(
                vm_id,
                GuestState {
                    endpoint: Some(endpoint.clone()),
                },
            );
            endpoint
        };
        self.invalidate_output();
        Arc::new(GuestSerialBackend {
            vm_id,
            endpoint,
            core: self.clone(),
        })
    }

    fn invalidate_output(&self) {
        self.output_epoch.fetch_add(1, Ordering::AcqRel);
    }

    #[cfg(test)]
    fn format_guest_output(
        &self,
        vm_id: VMId,
        generation: BackendGeneration,
        bytes: &[u8],
    ) -> Option<alloc::vec::Vec<u8>> {
        let state = self.lock_state();
        state
            .guests
            .get(&vm_id)
            .and_then(|guest| guest.endpoint.as_ref())
            .filter(|endpoint| endpoint.generation == generation && endpoint.is_active())?;
        drop(state);
        let mut output = self.lock_output();
        Some(
            output
                .mux
                .format(vm_id, self.running_count.load(Ordering::Acquire) > 1, bytes),
        )
    }

    fn publish_guest_output(&self, vm_id: VMId, generation: BackendGeneration, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let Some(mut ingress) = self.output_ingress.try_lock() else {
            // Console output is observational. Contention must never turn a
            // guest MMIO write into a sleeping or spinning wait.
            return;
        };
        for chunk in bytes.chunks(OUTPUT_FRAME_BYTES).take(OUTPUT_PUBLISH_BUDGET) {
            let mut frame = GuestOutputFrame {
                vm_id,
                generation,
                len: u16::try_from(chunk.len()).expect("guest output frame length fits in u16"),
                bytes: [0; OUTPUT_FRAME_BYTES],
            };
            frame.bytes[..chunk.len()].copy_from_slice(chunk);
            if ingress.push_back(frame).is_err() {
                break;
            }
        }
    }

    fn drain_guest_output_with(&self, budget: usize, mut write: impl FnMut(&[u8])) -> usize {
        let mut drained = 0;
        while drained < budget {
            let frame = {
                let mut ingress = self.output_ingress.lock();
                ingress.pop_front()
            };
            let Some(frame) = frame else {
                break;
            };
            drained += 1;

            let active = {
                let state = self.lock_state();
                state
                    .guests
                    .get(&frame.vm_id)
                    .and_then(|guest| guest.endpoint.as_ref())
                    .is_some_and(|endpoint| {
                        endpoint.generation == frame.generation && endpoint.is_active()
                    })
            };
            if !active {
                continue;
            }

            let mut output = self.lock_output();
            let epoch = self.output_epoch.load(Ordering::Acquire);
            if output.observed_epoch != epoch {
                output.mux.reset_all();
                output.observed_epoch = epoch;
            }
            if output.last_backend.is_some_and(|(owner, previous)| {
                owner == frame.vm_id && previous != frame.generation
            }) {
                output.mux.reset_all();
            }
            output.last_backend = Some((frame.vm_id, frame.generation));
            let formatted = output.mux.format(
                frame.vm_id,
                self.running_count.load(Ordering::Acquire) > 1,
                &frame.bytes[..usize::from(frame.len)],
            );
            drop(output);
            write(&formatted);
        }
        drained
    }

    fn drain_guest_output(&self) {
        self.drain_guest_output_with(OUTPUT_DRAIN_BUDGET, write_host_bytes);
    }
}

impl GuestIoEndpoint {
    fn new(generation: BackendGeneration) -> Self {
        Self {
            generation,
            active: AtomicBool::new(true),
            input: SpinNoPreempt::new(VecDeque::with_capacity(INPUT_QUEUE_CAPACITY)),
        }
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }

    fn enqueue(&self, bytes: &[u8]) {
        if !self.is_active() {
            return;
        }
        let mut input = self.input.lock();
        if !self.is_active() {
            return;
        }
        let available = INPUT_QUEUE_CAPACITY.saturating_sub(input.len());
        input.extend(bytes.iter().copied().take(available));
    }

    fn read(&self, buffer: &mut [u8]) -> usize {
        if !self.is_active() {
            return 0;
        }
        let mut input = self.input.lock();
        if !self.is_active() {
            return 0;
        }
        let read_len = buffer.len().min(input.len());
        for byte in &mut buffer[..read_len] {
            *byte = input
                .pop_front()
                .expect("guest input queue length was checked");
        }
        read_len
    }
}

impl SerialBackend for GuestSerialBackend {
    fn write(&self, bytes: &[u8]) {
        if !self.endpoint.is_active() {
            return;
        }
        self.core
            .publish_guest_output(self.vm_id, self.endpoint.generation, bytes);
    }

    fn read(&self, buffer: &mut [u8]) -> usize {
        self.endpoint.read(buffer)
    }
}

impl SerialBackendFactory for GuestSerialBackendFactory {
    fn create(&self) -> Arc<dyn SerialBackend> {
        self.core.create_serial_backend(self.vm_id)
    }
}

fn enqueue_guest_input(state: &mut ConsoleState, vm_id: VMId, bytes: &[u8]) {
    if let Some(endpoint) = state
        .guests
        .get(&vm_id)
        .and_then(|guest| guest.endpoint.as_ref())
    {
        endpoint.enqueue(bytes);
    }
}

/// Returns the factory that provisions one backend per VM device generation.
pub fn serial_backend_factory(vm_id: VMId) -> Arc<dyn SerialBackendFactory> {
    Arc::new(GuestSerialBackendFactory {
        vm_id,
        core: GUEST_CONSOLE_MUX.core.clone(),
    })
}

/// Route one host byte through the console shortcut and attachment state machine.
pub fn route_host_byte(byte: u8) -> ConsoleInputEvent {
    let routed = GUEST_CONSOLE_MUX.route_host_byte(byte);
    if let Some(vm_id) = routed.wake_vm
        && let Err(error) = crate::manager::AxvmManager::notify_vm(vm_id)
    {
        warn!("failed to wake VM[{vm_id}] for console input: {error:#}");
    }
    routed.event
}

/// Drains a bounded batch of guest output from task context.
pub fn drain_guest_output() {
    GUEST_CONSOLE_MUX.core.drain_guest_output();
}

/// Attach the lowest-ID member of the default running VM set.
pub fn attach_default(running: impl IntoIterator<Item = VMId>) -> Option<VMId> {
    GUEST_CONSOLE_MUX.attach_default(running)
}

/// Attach one running VM to the host console.
pub fn attach(vm_id: VMId) -> Result<()> {
    let Some(vm) = crate::manager::AxvmManager::vm_by_id(vm_id) else {
        bail!("VM[{vm_id}] not found");
    };
    if vm.status() != VmStatus::Running {
        bail!("VM[{vm_id}] is not running");
    }
    GUEST_CONSOLE_MUX.mark_running(vm_id);
    if !GUEST_CONSOLE_MUX.attach(vm_id) {
        bail!("VM[{vm_id}] is not available for console attachment");
    }
    Ok(())
}

/// Record a VM transition to Running.
pub fn mark_running(vm_id: VMId) {
    GUEST_CONSOLE_MUX.mark_running(vm_id);
}

/// Record a VM transition away from Running.
pub fn mark_stopped(vm_id: VMId) -> bool {
    GUEST_CONSOLE_MUX.mark_stopped(vm_id)
}

/// Remove all console state associated with a deleted VM.
pub fn remove(vm_id: VMId) -> bool {
    GUEST_CONSOLE_MUX.remove(vm_id)
}

/// Reconcile console attachment and prefixing against the actual VM registry.
pub fn reconcile_vm_states() -> Option<VMId> {
    let running = crate::manager::AxvmManager::vm_list()
        .into_iter()
        .filter(|vm| vm.status() == VmStatus::Running)
        .map(|vm| vm.id());
    GUEST_CONSOLE_MUX.set_running(running)
}

/// Return the currently attached guest, if any.
pub fn attached_vm() -> Option<VMId> {
    GUEST_CONSOLE_MUX.attached_vm()
}

#[cfg(test)]
mod tests;
