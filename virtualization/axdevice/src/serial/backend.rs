//! Host-facing character backend for virtual serial ports.

use core::fmt::Debug;

/// Bidirectional byte stream used by a virtual serial device.
///
/// The backend owns neither UART registers nor interrupt state. Implementations
/// may connect the stream to a terminal multiplexer, a test buffer, or another
/// host service.
pub trait SerialBackend: Send + Sync + Debug {
    /// Writes bytes emitted by the guest.
    fn write(&self, bytes: &[u8]);

    /// Reads host-provided bytes into `buffer` without blocking.
    fn read(&self, buffer: &mut [u8]) -> usize;
}

/// Backend used when no terminal service is attached.
#[derive(Debug, Default)]
pub struct NullSerialBackend;

impl SerialBackend for NullSerialBackend {
    fn write(&self, _bytes: &[u8]) {}

    fn read(&self, _buffer: &mut [u8]) -> usize {
        0
    }
}
