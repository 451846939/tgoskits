//! Reusable 16550-compatible UART register core.

use alloc::sync::Arc;

use ax_kspin::SpinRaw;
use axdevice_base::{AccessWidth, DeviceError, DeviceResult, IrqLine};

use super::{SerialBackend, fifo::ByteFifo};

const REG_RBR_THR_DLL: usize = 0;
const REG_IER_DLM: usize = 1;
const REG_IIR_FCR: usize = 2;
const REG_LCR: usize = 3;
const REG_MCR: usize = 4;
const REG_LSR: usize = 5;
const REG_MSR: usize = 6;
const REG_SCR: usize = 7;

const IER_RX_AVAILABLE: u8 = 1 << 0;
const IER_THR_EMPTY: u8 = 1 << 1;

const IIR_NO_INTERRUPT: u8 = 0x01;
const IIR_THR_EMPTY: u8 = 0x02;
const IIR_RX_AVAILABLE: u8 = 0x04;
const IIR_FIFO_16550A: u8 = 0xc0;

const FCR_CLEAR_RX: u8 = 1 << 1;
const LCR_DLAB: u8 = 1 << 7;
const MCR_LOOPBACK: u8 = 1 << 4;

const LSR_DATA_READY: u8 = 1 << 0;
const LSR_OVERRUN_ERROR: u8 = 1 << 1;
const LSR_THR_EMPTY: u8 = 1 << 5;
const LSR_TRANSMITTER_EMPTY: u8 = 1 << 6;

const MSR_DCD: u8 = 1 << 7;
const MSR_DSR: u8 = 1 << 5;
const MSR_CTS: u8 = 1 << 4;

const FIFO_CAPACITY: usize = 256;

struct Uart16550State {
    ier: u8,
    fcr: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    overrun: bool,
    tx_interrupt_pending: bool,
    rx_fifo: ByteFifo<FIFO_CAPACITY>,
}

impl Uart16550State {
    const fn new() -> Self {
        Self {
            ier: 0,
            fcr: 0,
            lcr: 0x03,
            mcr: 0,
            scr: 0,
            dll: 1,
            dlm: 0,
            overrun: false,
            tx_interrupt_pending: true,
            rx_fifo: ByteFifo::new(),
        }
    }

    const fn dlab(&self) -> bool {
        self.lcr & LCR_DLAB != 0
    }

    fn push_rx(&mut self, byte: u8) {
        if !self.rx_fifo.push(byte) {
            self.overrun = true;
        }
    }

    fn line_status(&mut self) -> u8 {
        let mut status = LSR_THR_EMPTY | LSR_TRANSMITTER_EMPTY;
        if !self.rx_fifo.is_empty() {
            status |= LSR_DATA_READY;
        }
        if core::mem::take(&mut self.overrun) {
            status |= LSR_OVERRUN_ERROR;
        }
        status
    }

    const fn pending_interrupt(&self) -> u8 {
        if self.ier & IER_RX_AVAILABLE != 0 && !self.rx_fifo.is_empty() {
            IIR_RX_AVAILABLE
        } else if self.ier & IER_THR_EMPTY != 0 && self.tx_interrupt_pending {
            IIR_THR_EMPTY
        } else {
            IIR_NO_INTERRUPT
        }
    }

    const fn irq_asserted(&self) -> bool {
        self.pending_interrupt() != IIR_NO_INTERRUPT
    }
}

/// 16550-compatible UART core with an external byte backend and virtual IRQ.
pub struct Uart16550 {
    state: SpinRaw<Uart16550State>,
    backend: Arc<dyn SerialBackend>,
    irq: IrqLine,
}

impl Uart16550 {
    /// Creates a powered-on 16550 UART.
    pub fn new(backend: Arc<dyn SerialBackend>, irq: IrqLine) -> Self {
        Self {
            state: SpinRaw::new(Uart16550State::new()),
            backend,
            irq,
        }
    }

    /// Polls backend input into the receive FIFO and refreshes the level IRQ.
    pub fn poll(&self) -> DeviceResult {
        let mut bytes = [0; 64];
        let count = self.backend.read(&mut bytes).min(bytes.len());
        let asserted = {
            let mut state = self.state.lock();
            for &byte in &bytes[..count] {
                state.push_rx(byte);
            }
            state.irq_asserted()
        };
        self.signal_irq(asserted)
    }

    /// Reads one UART register.
    pub fn read(&self, register: usize, width: AccessWidth) -> DeviceResult<u64> {
        if width != AccessWidth::Byte {
            return Err(DeviceError::InvalidWidth {
                expected: AccessWidth::Byte,
                actual: width,
            });
        }

        let (value, asserted) = {
            let mut state = self.state.lock();
            let value = match register {
                REG_RBR_THR_DLL if state.dlab() => state.dll,
                REG_RBR_THR_DLL => state.rx_fifo.pop().unwrap_or(0),
                REG_IER_DLM if state.dlab() => state.dlm,
                REG_IER_DLM => state.ier,
                REG_IIR_FCR => {
                    let interrupt = state.pending_interrupt();
                    if interrupt == IIR_THR_EMPTY {
                        state.tx_interrupt_pending = false;
                    }
                    IIR_FIFO_16550A | interrupt
                }
                REG_LCR => state.lcr,
                REG_MCR => state.mcr,
                REG_LSR => state.line_status(),
                REG_MSR => MSR_DCD | MSR_DSR | MSR_CTS,
                REG_SCR => state.scr,
                _ => 0,
            };
            (value, state.irq_asserted())
        };
        self.signal_irq(asserted)?;
        Ok(value as u64)
    }

    /// Writes one UART register.
    pub fn write(&self, register: usize, width: AccessWidth, value: u64) -> DeviceResult {
        if width != AccessWidth::Byte {
            return Err(DeviceError::InvalidWidth {
                expected: AccessWidth::Byte,
                actual: width,
            });
        }

        let byte = value as u8;
        let (output, asserted) = {
            let mut state = self.state.lock();
            let mut output = None;
            match register {
                REG_RBR_THR_DLL if state.dlab() => state.dll = byte,
                REG_RBR_THR_DLL => {
                    if state.mcr & MCR_LOOPBACK != 0 {
                        state.push_rx(byte);
                    } else {
                        output = Some(byte);
                    }
                    state.tx_interrupt_pending = true;
                }
                REG_IER_DLM if state.dlab() => state.dlm = byte,
                REG_IER_DLM => {
                    let old = state.ier;
                    state.ier = byte & 0x0f;
                    if old & IER_THR_EMPTY == 0 && state.ier & IER_THR_EMPTY != 0 {
                        state.tx_interrupt_pending = true;
                    }
                }
                REG_IIR_FCR => {
                    state.fcr = byte;
                    if byte & FCR_CLEAR_RX != 0 {
                        state.rx_fifo.clear();
                        state.overrun = false;
                    }
                }
                REG_LCR => state.lcr = byte,
                REG_MCR => state.mcr = byte,
                REG_LSR | REG_MSR => {}
                REG_SCR => state.scr = byte,
                _ => {}
            }
            (output, state.irq_asserted())
        };

        if let Some(byte) = output {
            self.backend.write(core::slice::from_ref(&byte));
        }
        self.signal_irq(asserted)
    }

    fn signal_irq(&self, asserted: bool) -> DeviceResult {
        let result = if asserted {
            self.irq.raise()
        } else {
            self.irq.lower()
        };
        result.map_err(|error| DeviceError::Backend {
            operation: "signal 16550 IRQ",
            detail: alloc::format!("{error}"),
        })
    }
}
