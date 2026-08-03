//! Host-console multiplexing for mandatory guest virtual serial devices.

extern crate alloc;

use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    sync::Arc,
    vec::Vec,
};

use anyhow::{Result, bail};
use ax_kspin::SpinRaw as Mutex;
use ax_std::os::arceos::modules::ax_task;
use axvm::{AxVMRef, SerialBackend, VMId, VmStatus};
use spin::LazyLock;

const CTRL_A: u8 = 0x01;
const INPUT_QUEUE_CAPACITY: usize = 4096;

static GUEST_CONSOLE_MUX: LazyLock<GuestConsoleMux> = LazyLock::new(GuestConsoleMux::new);

/// Result of routing one byte read from the host console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleInputEvent {
    /// The byte belongs to the Axvisor shell.
    ShellByte(u8),
    /// The byte was consumed by the attached guest or an escape sequence.
    Consumed,
    /// The escape sequence attached the named guest.
    Attached(VMId),
    /// The escape sequence returned from the named guest to the shell.
    Detached(VMId),
    /// The user requested console escape help.
    Help,
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
    state: Mutex<ConsoleState>,
    output_lock: Mutex<()>,
}

#[derive(Debug, Default)]
struct ConsoleState {
    guests: BTreeMap<VMId, GuestState>,
    running: BTreeSet<VMId>,
    attached: Option<VMId>,
    last_attached: Option<VMId>,
    escape_pending: bool,
    output_owner: Option<VMId>,
}

#[derive(Debug)]
struct GuestState {
    input: VecDeque<u8>,
    pending_output: VecDeque<u8>,
    output_at_line_start: bool,
}

impl Default for GuestState {
    fn default() -> Self {
        Self {
            input: VecDeque::new(),
            pending_output: VecDeque::new(),
            output_at_line_start: true,
        }
    }
}

#[derive(Debug)]
struct GuestSerialBackend {
    vm_id: VMId,
    core: Arc<ConsoleCore>,
}

impl GuestConsoleMux {
    fn new() -> Self {
        Self {
            core: Arc::new(ConsoleCore {
                state: Mutex::new(ConsoleState::default()),
                output_lock: Mutex::new(()),
            }),
        }
    }

    fn serial_backend(&self, vm_id: VMId) -> Arc<dyn SerialBackend> {
        self.core.state.lock().guests.entry(vm_id).or_default();
        Arc::new(GuestSerialBackend {
            vm_id,
            core: self.core.clone(),
        })
    }

    fn set_running(&self, running: impl IntoIterator<Item = VMId>) -> Option<VMId> {
        let mut state = self.core.state.lock();
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
            state.escape_pending = false;
        }
        if state
            .output_owner
            .is_some_and(|vm_id| !state.running.contains(&vm_id))
        {
            state.output_owner = None;
        }
        detached
    }

    fn mark_running(&self, vm_id: VMId) {
        let mut state = self.core.state.lock();
        state.running.insert(vm_id);
        state.guests.entry(vm_id).or_default();
    }

    fn mark_stopped(&self, vm_id: VMId) -> bool {
        let mut state = self.core.state.lock();
        state.running.remove(&vm_id);
        if let Some(guest) = state.guests.get_mut(&vm_id) {
            guest.pending_output.clear();
        }
        if state.output_owner == Some(vm_id) {
            state.output_owner = None;
        }
        if state.attached == Some(vm_id) {
            state.attached = None;
            state.escape_pending = false;
            return true;
        }
        false
    }

    fn remove(&self, vm_id: VMId) -> bool {
        let mut state = self.core.state.lock();
        state.running.remove(&vm_id);
        state.guests.remove(&vm_id);
        if state.output_owner == Some(vm_id) {
            state.output_owner = None;
        }
        if state.last_attached == Some(vm_id) {
            state.last_attached = None;
        }
        if state.attached == Some(vm_id) {
            state.attached = None;
            state.escape_pending = false;
            return true;
        }
        false
    }

    fn attach_default(&self, running: impl IntoIterator<Item = VMId>) -> Option<VMId> {
        self.set_running(running);
        let mut state = self.core.state.lock();
        let vm_id = state.running.first().copied()?;
        state.attached = Some(vm_id);
        state.last_attached = Some(vm_id);
        Some(vm_id)
    }

    fn attach(&self, vm_id: VMId) -> bool {
        let mut state = self.core.state.lock();
        if !state.running.contains(&vm_id) {
            return false;
        }
        state.attached = Some(vm_id);
        state.last_attached = Some(vm_id);
        state.escape_pending = false;
        true
    }

    fn attached_vm(&self) -> Option<VMId> {
        self.core.state.lock().attached
    }

    fn route_host_byte(&self, byte: u8) -> RoutedInput {
        let mut state = self.core.state.lock();

        if state.escape_pending {
            state.escape_pending = false;
            return match byte {
                b'c' => {
                    if let Some(vm_id) = state.attached.take() {
                        RoutedInput {
                            event: ConsoleInputEvent::Detached(vm_id),
                            wake_vm: None,
                        }
                    } else {
                        let vm_id = state
                            .last_attached
                            .filter(|vm_id| state.running.contains(vm_id))
                            .or_else(|| state.running.first().copied());
                        match vm_id {
                            Some(vm_id) => {
                                state.attached = Some(vm_id);
                                state.last_attached = Some(vm_id);
                                RoutedInput {
                                    event: ConsoleInputEvent::Attached(vm_id),
                                    wake_vm: None,
                                }
                            }
                            None => RoutedInput {
                                event: ConsoleInputEvent::NoRunningGuest,
                                wake_vm: None,
                            },
                        }
                    }
                }
                b'a' => match state.attached {
                    Some(vm_id) => {
                        enqueue_guest_input(&mut state, vm_id, &[CTRL_A]);
                        RoutedInput {
                            event: ConsoleInputEvent::Consumed,
                            wake_vm: Some(vm_id),
                        }
                    }
                    None => RoutedInput {
                        event: ConsoleInputEvent::ShellByte(CTRL_A),
                        wake_vm: None,
                    },
                },
                b'h' => RoutedInput {
                    event: ConsoleInputEvent::Help,
                    wake_vm: None,
                },
                byte => match state.attached {
                    Some(vm_id) => {
                        enqueue_guest_input(&mut state, vm_id, &[CTRL_A, byte]);
                        RoutedInput {
                            event: ConsoleInputEvent::Consumed,
                            wake_vm: Some(vm_id),
                        }
                    }
                    None => RoutedInput {
                        event: ConsoleInputEvent::ShellByte(byte),
                        wake_vm: None,
                    },
                },
            };
        }

        if byte == CTRL_A {
            state.escape_pending = true;
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

impl ConsoleCore {
    fn read_guest_input(&self, vm_id: VMId, buffer: &mut [u8]) -> usize {
        let mut state = self.state.lock();
        let Some(guest) = state.guests.get_mut(&vm_id) else {
            return 0;
        };
        let read_len = buffer.len().min(guest.input.len());
        for byte in &mut buffer[..read_len] {
            *byte = guest
                .input
                .pop_front()
                .expect("guest input queue length was checked");
        }
        read_len
    }

    fn format_guest_output(&self, vm_id: VMId, bytes: &[u8]) -> Vec<u8> {
        let mut state = self.state.lock();
        let multiple_running = state.running.len() > 1;
        state
            .guests
            .entry(vm_id)
            .or_default()
            .pending_output
            .extend(bytes);

        if !multiple_running {
            state.output_owner = None;
            let guest = state
                .guests
                .get_mut(&vm_id)
                .expect("guest output queue was just created");
            let mut output = Vec::with_capacity(guest.pending_output.len());
            output.extend(guest.pending_output.drain(..));
            for &byte in &output {
                guest.output_at_line_start = byte == b'\n';
            }
            return output;
        }

        if state.output_owner.is_none() {
            state.output_owner = Some(vm_id);
        }

        let mut output = Vec::with_capacity(bytes.len() + 16);
        loop {
            let Some(owner) = state.output_owner else {
                let next = state
                    .guests
                    .iter()
                    .find_map(|(&id, guest)| (!guest.pending_output.is_empty()).then_some(id));
                let Some(next) = next else {
                    break;
                };
                state.output_owner = Some(next);
                continue;
            };

            let guest = state
                .guests
                .get_mut(&owner)
                .expect("output owner must have guest state");
            if guest.pending_output.is_empty() {
                if guest.output_at_line_start {
                    state.output_owner = None;
                    continue;
                }
                break;
            }

            if guest.output_at_line_start {
                output.extend_from_slice(format!("[VM {owner}] ").as_bytes());
                guest.output_at_line_start = false;
            }
            while let Some(byte) = guest.pending_output.pop_front() {
                output.push(byte);
                if byte == b'\n' {
                    guest.output_at_line_start = true;
                    state.output_owner = None;
                    break;
                }
            }
            if state.output_owner == Some(owner) {
                break;
            }
        }
        output
    }

    fn write_guest_output(&self, vm_id: VMId, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let _output_guard = self.output_lock.lock();
        let output = self.format_guest_output(vm_id, bytes);
        ax_hal::console::write_bytes(&output);
    }
}

impl SerialBackend for GuestSerialBackend {
    fn write(&self, bytes: &[u8]) {
        self.core.write_guest_output(self.vm_id, bytes);
    }

    fn read(&self, buffer: &mut [u8]) -> usize {
        self.core.read_guest_input(self.vm_id, buffer)
    }
}

fn enqueue_guest_input(state: &mut ConsoleState, vm_id: VMId, bytes: &[u8]) {
    let guest = state.guests.entry(vm_id).or_default();
    let available = INPUT_QUEUE_CAPACITY.saturating_sub(guest.input.len());
    guest.input.extend(bytes.iter().copied().take(available));
}

/// Create or return the serial backend associated with one VM.
pub fn serial_backend(vm_id: VMId) -> Arc<dyn SerialBackend> {
    GUEST_CONSOLE_MUX.serial_backend(vm_id)
}

fn console_reader_isolation_cpu(
    host_cpu_count: usize,
    vcpu_masks: impl IntoIterator<Item = Option<usize>>,
) -> Option<usize> {
    let tracked_cpu_count = host_cpu_count.min(usize::BITS as usize);
    if tracked_cpu_count == 0 {
        return None;
    }

    let online_bits = if tracked_cpu_count == usize::BITS as usize {
        usize::MAX
    } else {
        (1usize << tracked_cpu_count) - 1
    };
    let guest_bits = vcpu_masks.into_iter().fold(0usize, |used, mask| {
        let requested = mask.unwrap_or(online_bits) & online_bits;
        // A missing, empty, or offline-only mask lets the runtime choose a
        // fallback CPU, so no CPU can be proven host-only in that case.
        used | if requested == 0 {
            online_bits
        } else {
            requested
        }
    });
    let host_only_bits = online_bits & !guest_bits;
    (host_only_bits != 0).then(|| host_only_bits.trailing_zeros() as usize)
}

/// Configures the polling owner for physical host-console input.
///
/// The console multiplexer remains the only physical UART reader. Input IRQs
/// stay disabled until the host UART IRQ contract can transfer received bytes
/// to that owner without introducing a second reader.
pub fn configure_host_console_reader(vms: &[AxVMRef]) -> Result<()> {
    ax_hal::console::set_input_irq_enabled(false);

    let isolation_cpu = console_reader_isolation_cpu(
        ax_hal::cpu_num(),
        vms.iter()
            .flat_map(|vm| vm.vcpu_snapshots())
            .map(|vcpu| vcpu.phys_cpu_set),
    );

    // Temporary polling and scheduler-isolation workaround.
    //
    // Trigger: a pinned, continuously runnable vCPU can starve the polling
    // console reader on the cooperative FIFO scheduler. The current raw UART
    // IRQ contract only observes interrupt status; it neither drains RX into a
    // mux-owned queue nor wakes this task. Enabling that IRQ can therefore
    // leave a level source asserted and starve the only code allowed to consume
    // the receive register. Keep RX IRQs disabled and, when the validated VM
    // topology leaves a CPU outside every explicit vCPU mask, place the reader
    // there before any vCPU task starts.
    //
    // Cost and safety boundary: host input remains polling-driven, and the
    // Axvisor management task loses migration freedom on a topology-proven
    // host-only CPU. The workaround never rewrites a vCPU mask, never takes a
    // CPU that a vCPU may use, and leaves the task affinity unchanged when any
    // vCPU has no usable explicit mask. It also does not switch the scheduler
    // globally, which would affect unrelated architectures and expose existing
    // scheduler defects.
    //
    // Removal conditions: re-enable UART RX IRQs only after its top half drains
    // RX into a bounded mux-owned queue and performs an IRQ-safe task wake.
    // Remove the CPU placement once same-CPU FIFO fairness guarantees progress
    // for a polling host service alongside a runnable vCPU.
    let Some(owner_cpu) = isolation_cpu else {
        return Ok(());
    };
    let owner_affinity = ax_task::AxCpuMask::one_shot(owner_cpu);
    if !ax_task::set_current_affinity(owner_affinity) {
        bail!("failed to pin the host console reader to CPU {owner_cpu}");
    }
    let actual_owner_cpu = ax_hal::percpu::this_cpu_id();
    if actual_owner_cpu != owner_cpu {
        bail!(
            "host console reader affinity selected CPU {owner_cpu}, but migration ended on CPU \
             {actual_owner_cpu}"
        );
    }

    Ok(())
}

/// Read at most one byte from the physical host console.
///
/// No other Axvisor component may call the platform console input API.
pub fn read_host_byte() -> Option<u8> {
    let mut byte = [0u8; 1];
    (ax_hal::console::read_bytes(&mut byte) == 1).then_some(byte[0])
}

/// Route one host byte through the console escape and attachment state machine.
pub fn route_host_byte(byte: u8) -> ConsoleInputEvent {
    let routed = GUEST_CONSOLE_MUX.route_host_byte(byte);
    if let Some(vm_id) = routed.wake_vm
        && let Err(error) = crate::manager::AxvmManager::notify_vm(vm_id)
    {
        warn!("failed to wake VM[{vm_id}] for console input: {error:#}");
    }
    routed.event
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
mod tests {
    use super::*;

    #[test]
    fn lowest_running_vm_is_default_and_input_only_reaches_foreground() {
        let mux = GuestConsoleMux::new();
        let backend_1 = mux.serial_backend(1);
        let backend_2 = mux.serial_backend(2);

        assert_eq!(mux.attach_default([2, 1]), Some(1));
        assert_eq!(mux.route_host_byte(b'x').event, ConsoleInputEvent::Consumed);

        let mut input = [0u8; 2];
        assert_eq!(backend_1.read(&mut input), 1);
        assert_eq!(input[0], b'x');
        assert_eq!(backend_2.read(&mut input), 0);
    }

    #[test]
    fn escape_sequences_switch_modes_and_forward_raw_ctrl_a() {
        let mux = GuestConsoleMux::new();
        let backend = mux.serial_backend(7);
        assert_eq!(mux.attach_default([7]), Some(7));

        assert_eq!(
            mux.route_host_byte(CTRL_A).event,
            ConsoleInputEvent::Consumed
        );
        assert_eq!(mux.route_host_byte(b'a').event, ConsoleInputEvent::Consumed);
        let mut input = [0u8; 1];
        assert_eq!(backend.read(&mut input), 1);
        assert_eq!(input[0], CTRL_A);

        mux.route_host_byte(CTRL_A);
        assert_eq!(
            mux.route_host_byte(b'c').event,
            ConsoleInputEvent::Detached(7)
        );
        mux.route_host_byte(CTRL_A);
        assert_eq!(
            mux.route_host_byte(b'c').event,
            ConsoleInputEvent::Attached(7)
        );
    }

    #[test]
    fn stopping_foreground_guest_returns_to_shell() {
        let mux = GuestConsoleMux::new();
        mux.serial_backend(3);
        mux.attach_default([3]);

        assert_eq!(mux.set_running([]), Some(3));
        assert_eq!(mux.attached_vm(), None);
    }

    #[test]
    fn multiple_running_guests_receive_line_prefixes() {
        let mux = GuestConsoleMux::new();
        mux.serial_backend(1);
        mux.set_running([1]);
        assert_eq!(mux.core.format_guest_output(1, b"boot\n"), b"boot\n");

        mux.set_running([1, 2]);
        assert_eq!(
            mux.core.format_guest_output(1, b"ready\nprompt"),
            b"[VM 1] ready\n[VM 1] prompt"
        );
        assert!(mux.core.format_guest_output(2, b"other\n").is_empty());
        assert_eq!(
            mux.core.format_guest_output(1, b"> \n"),
            b"> \n[VM 2] other\n"
        );
    }

    #[test]
    fn console_owner_prefers_a_cpu_excluded_by_every_vcpu() {
        assert_eq!(console_reader_isolation_cpu(4, [Some(0b0001)]), Some(1));
        assert_eq!(
            console_reader_isolation_cpu(4, [Some(0b0010), Some(0b1000)]),
            Some(0)
        );
    }

    #[test]
    fn console_owner_is_not_pinned_when_no_host_only_cpu_is_proven() {
        assert_eq!(console_reader_isolation_cpu(4, [None]), None);
        assert_eq!(console_reader_isolation_cpu(4, [Some(0)]), None);
        assert_eq!(console_reader_isolation_cpu(4, [Some(usize::MAX)]), None);
    }
}
