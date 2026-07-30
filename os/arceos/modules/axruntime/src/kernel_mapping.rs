//! Global kernel mapping attribute updates.

use ax_hal::paging::MappingFlags;
use ax_memory_addr::VirtAddr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KernelMappingError;

trait KernelMappingRuntime {
    fn protect(
        &self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
    ) -> Result<(), KernelMappingError>;

    fn flush_all_cpus(&self, start: VirtAddr, size: usize);
}

struct AxHalKernelMappingRuntime;

impl KernelMappingRuntime for AxHalKernelMappingRuntime {
    fn protect(
        &self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
    ) -> Result<(), KernelMappingError> {
        ax_mm::kernel_aspace()
            .lock()
            .protect(start, size, flags)
            .map_err(|_| KernelMappingError)
    }

    fn flush_all_cpus(&self, start: VirtAddr, size: usize) {
        ax_hal::cache::flush_tlb_range_all_cpus(start, size);
    }
}

/// Changes attributes in the shared kernel page table and completes the
/// system-wide TLB invalidation before returning.
///
/// Releasing pages after only a local invalidation can leave another CPU using
/// the old memory type. In particular, reusing a former uncached DMA page for
/// an atomic object can then fault on architectures without an external global
/// monitor.
pub(crate) fn protect_kernel_range(
    start: VirtAddr,
    size: usize,
    flags: MappingFlags,
) -> Result<(), KernelMappingError> {
    protect_kernel_range_with(&AxHalKernelMappingRuntime, start, size, flags)
}

fn protect_kernel_range_with(
    runtime: &impl KernelMappingRuntime,
    start: VirtAddr,
    size: usize,
    flags: MappingFlags,
) -> Result<(), KernelMappingError> {
    runtime.protect(start, size, flags)?;
    // The page-table lock is released by `protect` before the synchronous
    // cross-CPU operation. This avoids holding the IRQ-safe mapping gate while
    // remote CPUs acknowledge the invalidation.
    runtime.flush_all_cpus(start, size);
    Ok(())
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    struct ModelRuntime {
        phase: Cell<u8>,
        flushed_start: Cell<Option<VirtAddr>>,
        flushed_size: Cell<usize>,
    }

    impl ModelRuntime {
        const fn new() -> Self {
            Self {
                phase: Cell::new(0),
                flushed_start: Cell::new(None),
                flushed_size: Cell::new(0),
            }
        }
    }

    impl KernelMappingRuntime for ModelRuntime {
        fn protect(
            &self,
            _start: VirtAddr,
            _size: usize,
            _flags: MappingFlags,
        ) -> Result<(), KernelMappingError> {
            assert_eq!(self.phase.replace(1), 0);
            Ok(())
        }

        fn flush_all_cpus(&self, start: VirtAddr, size: usize) {
            assert_eq!(self.phase.replace(2), 1);
            self.flushed_start.set(Some(start));
            self.flushed_size.set(size);
        }
    }

    #[test]
    fn kernel_mapping_attribute_update_reaches_all_cpus_after_pte_update() {
        let runtime = ModelRuntime::new();
        let start = VirtAddr::from(0x4000);

        protect_kernel_range_with(&runtime, start, 0x2000, MappingFlags::READ).unwrap();

        assert_eq!(runtime.phase.get(), 2);
        assert_eq!(runtime.flushed_start.get(), Some(start));
        assert_eq!(runtime.flushed_size.get(), 0x2000);
    }
}
