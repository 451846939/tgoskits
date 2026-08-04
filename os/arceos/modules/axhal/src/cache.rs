//! Cache, TLB, and modified-text synchronization helpers.

#[cfg(all(not(target_arch = "aarch64"), feature = "ipi"))]
use core::sync::atomic::{AtomicBool, Ordering};

use ax_memory_addr::{PAGE_SIZE_4K, VirtAddr};

#[cfg(all(not(target_arch = "aarch64"), feature = "ipi"))]
static REMOTE_TLB_SHOOTDOWN_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enables synchronous remote TLB invalidation after the primary IPI path is ready.
///
/// Before this transition, only the boot CPU can have cached runtime page-table
/// translations. Each secondary CPU performs a full local flush immediately
/// before publishing its IPI readiness.
pub fn enable_remote_tlb_shootdown() {
    #[cfg(all(not(target_arch = "aarch64"), feature = "ipi"))]
    REMOTE_TLB_SHOOTDOWN_ENABLED.store(true, Ordering::Release);
}

/// Failure while synchronously invalidating a kernel TLB range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlbShootdownError {
    /// The target CPU is offline.
    CpuOffline,
    /// The synchronous cross-CPU call timed out.
    Timeout,
    /// This configuration has no cross-CPU invalidation backend.
    Unsupported,
    /// The platform rejected the cross-CPU operation.
    Platform,
}

/// Flushes the TLB entries covering a virtual-address range on the current CPU.
pub fn flush_tlb_range(start: VirtAddr, size: usize) {
    for offset in (0..size).step_by(PAGE_SIZE_4K) {
        ax_cpu::asm::flush_tlb(Some(start + offset));
    }
}

/// Flushes the complete local TLB.
pub fn flush_tlb_all() {
    ax_cpu::asm::flush_tlb(None);
}

/// Flushes the TLB entries covering a virtual-address range on all available CPUs.
pub fn flush_tlb_range_all_cpus(start: VirtAddr, size: usize) -> Result<(), TlbShootdownError> {
    #[cfg(target_arch = "aarch64")]
    {
        // AArch64 uses inner-shareable TLBI instructions, so the local call
        // already broadcasts and waiting for per-CPU IPIs would duplicate it.
        flush_tlb_range(start, size);
        Ok(())
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        #[cfg(feature = "ipi")]
        {
            if !REMOTE_TLB_SHOOTDOWN_ENABLED.load(Ordering::Acquire) {
                flush_tlb_range(start, size);
                return Ok(());
            }
        }
        #[cfg(feature = "ipi")]
        let _guard = ax_kernel_guard::NoPreempt::new();
        flush_tlb_range_all_cpus_with(&AxHalTlbShootdown, start, size)
    }
}

#[cfg(not(target_arch = "aarch64"))]
trait TlbShootdown {
    fn cpu_count(&self) -> usize;
    fn current_cpu(&self) -> usize;
    fn cpu_online(&self, cpu_id: usize) -> bool;
    fn flush_remote(
        &self,
        cpu_id: usize,
        start: VirtAddr,
        size: usize,
    ) -> Result<(), TlbShootdownError>;
    fn flush_local(&self, start: VirtAddr, size: usize);
}

#[cfg(not(target_arch = "aarch64"))]
struct AxHalTlbShootdown;

#[cfg(not(target_arch = "aarch64"))]
impl TlbShootdown for AxHalTlbShootdown {
    fn cpu_count(&self) -> usize {
        crate::cpu_num()
    }

    fn current_cpu(&self) -> usize {
        crate::percpu::this_cpu_id()
    }

    fn cpu_online(&self, cpu_id: usize) -> bool {
        #[cfg(feature = "irq")]
        {
            crate::irq::is_cpu_online(cpu_id)
        }
        #[cfg(not(feature = "irq"))]
        {
            cpu_id < crate::cpu_num()
        }
    }

    fn flush_remote(
        &self,
        cpu_id: usize,
        start: VirtAddr,
        size: usize,
    ) -> Result<(), TlbShootdownError> {
        #[cfg(feature = "ipi")]
        {
            let arg = FlushRangeArg {
                start: start.as_usize(),
                size,
            };
            let arg_ptr = &arg as *const FlushRangeArg as *mut ();
            // SAFETY: the callback is synchronous, so `arg` remains live until
            // the target CPU finishes reading it.
            unsafe {
                crate::irq::run_on_cpu_sync(
                    crate::irq::CpuId(cpu_id),
                    flush_tlb_range_thunk,
                    arg_ptr,
                )
            }
            .map_err(|err| match err {
                crate::irq::IrqError::CpuOffline => TlbShootdownError::CpuOffline,
                crate::irq::IrqError::Timeout => TlbShootdownError::Timeout,
                crate::irq::IrqError::Unsupported => TlbShootdownError::Unsupported,
                _ => TlbShootdownError::Platform,
            })
        }
        #[cfg(not(feature = "ipi"))]
        {
            let _ = (cpu_id, start, size);
            Err(TlbShootdownError::Unsupported)
        }
    }

    fn flush_local(&self, start: VirtAddr, size: usize) {
        flush_tlb_range(start, size);
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn flush_tlb_range_all_cpus_with(
    runtime: &impl TlbShootdown,
    start: VirtAddr,
    size: usize,
) -> Result<(), TlbShootdownError> {
    let current_cpu = runtime.current_cpu();
    for cpu_id in 0..runtime.cpu_count() {
        if cpu_id == current_cpu || !runtime.cpu_online(cpu_id) {
            continue;
        }
        runtime.flush_remote(cpu_id, start, size)?;
    }
    runtime.flush_local(start, size);
    Ok(())
}

/// Flushes the complete TLB on every available CPU.
pub fn flush_tlb_all_cpus() {
    #[cfg(target_arch = "aarch64")]
    {
        flush_tlb_all();
    }
    #[cfg(all(not(target_arch = "aarch64"), feature = "ipi"))]
    {
        if !REMOTE_TLB_SHOOTDOWN_ENABLED.load(Ordering::Acquire) {
            flush_tlb_all();
            return;
        }
        let _guard = ax_kernel_guard::NoPreempt::new();
        let current_cpu = crate::percpu::this_cpu_id();

        for cpu_id in 0..crate::cpu_num() {
            if cpu_id == current_cpu {
                continue;
            }
            // SAFETY: the callback has no argument and completes synchronously
            // before this call returns.
            let result = unsafe {
                crate::irq::run_on_cpu_sync(
                    crate::irq::CpuId(cpu_id),
                    flush_tlb_all_thunk,
                    core::ptr::null_mut(),
                )
            };
            match result {
                Ok(()) | Err(crate::irq::IrqError::CpuOffline) => {}
                Err(error) => panic!("remote full TLB shootdown IPI failed: {error:?}"),
            }
        }
        flush_tlb_all();
    }
    #[cfg(all(not(target_arch = "aarch64"), not(feature = "ipi")))]
    {
        assert_eq!(
            crate::cpu_num(),
            1,
            "SMP TLB invalidation requires the ax-hal `ipi` feature",
        );
        flush_tlb_all();
    }
}

/// Flushes a batch of individual TLB entries on all available CPUs.
#[cfg(not(target_arch = "aarch64"))]
pub fn flush_tlb_list_all_cpus(vaddrs: &[VirtAddr]) {
    #[cfg(feature = "ipi")]
    {
        if !REMOTE_TLB_SHOOTDOWN_ENABLED.load(Ordering::Acquire) {
            for &vaddr in vaddrs {
                ax_cpu::asm::flush_tlb(Some(vaddr));
            }
            return;
        }
        let _guard = ax_kernel_guard::NoPreempt::new();
        let current_cpu = crate::percpu::this_cpu_id();
        let arg = FlushListArg {
            ptr: vaddrs.as_ptr(),
            len: vaddrs.len(),
        };
        let arg_ptr = &arg as *const FlushListArg as *mut ();

        for cpu_id in 0..crate::cpu_num() {
            if cpu_id == current_cpu {
                continue;
            }
            // SAFETY: the callback is synchronous, so `vaddrs` remains live
            // until the target CPU finishes reading the borrowed slice.
            let result = unsafe {
                crate::irq::run_on_cpu_sync(
                    crate::irq::CpuId(cpu_id),
                    flush_tlb_list_thunk,
                    arg_ptr,
                )
            };
            match result {
                Ok(()) | Err(crate::irq::IrqError::CpuOffline) => {}
                Err(error) => panic!("remote batched TLB shootdown IPI failed: {error:?}"),
            }
        }
    }
    #[cfg(not(feature = "ipi"))]
    assert_eq!(
        crate::cpu_num(),
        1,
        "SMP TLB invalidation requires the ax-hal `ipi` feature",
    );

    for &vaddr in vaddrs {
        ax_cpu::asm::flush_tlb(Some(vaddr));
    }
}

#[cfg(all(not(target_arch = "aarch64"), feature = "ipi"))]
struct FlushRangeArg {
    start: usize,
    size: usize,
}

#[cfg(all(not(target_arch = "aarch64"), feature = "ipi"))]
struct FlushListArg {
    ptr: *const VirtAddr,
    len: usize,
}

#[cfg(all(not(target_arch = "aarch64"), feature = "ipi"))]
/// Runs a range invalidation on a remote CPU.
///
/// # Safety
///
/// `arg` must point to a live [`FlushRangeArg`] until this synchronous callback
/// returns.
unsafe fn flush_tlb_range_thunk(arg: *mut ()) {
    // SAFETY: upheld by the synchronous caller described above.
    let arg = unsafe { &*(arg as *const FlushRangeArg) };
    flush_tlb_range(VirtAddr::from(arg.start), arg.size);
}

#[cfg(all(not(target_arch = "aarch64"), feature = "ipi"))]
/// Runs a complete TLB invalidation on a remote CPU.
///
/// # Safety
///
/// The callback ignores its opaque argument and must be invoked synchronously.
unsafe fn flush_tlb_all_thunk(_arg: *mut ()) {
    flush_tlb_all();
}

#[cfg(all(not(target_arch = "aarch64"), feature = "ipi"))]
/// Runs a list invalidation on a remote CPU.
///
/// # Safety
///
/// `arg` must point to a live [`FlushListArg`] whose slice remains readable
/// until this synchronous callback returns.
unsafe fn flush_tlb_list_thunk(arg: *mut ()) {
    // SAFETY: upheld by the synchronous caller described above.
    let arg = unsafe { &*(arg as *const FlushListArg) };
    // SAFETY: the initiating CPU waits synchronously for this callback while
    // holding preemption disabled, so the borrowed address slice stays live.
    let vaddrs = unsafe { core::slice::from_raw_parts(arg.ptr, arg.len) };
    for &vaddr in vaddrs {
        ax_cpu::asm::flush_tlb(Some(vaddr));
    }
}

/// Flushes the entire instruction cache on the current CPU.
pub fn flush_icache_all() {
    ax_cpu::asm::flush_icache_all();
}

/// Flushes the entire instruction cache on all available CPUs.
pub fn flush_icache_all_cpus() {
    #[cfg(feature = "ipi")]
    {
        let _guard = ax_kernel_guard::NoPreempt::new();
        let current_cpu = crate::percpu::this_cpu_id();

        for cpu_id in 0..crate::cpu_num() {
            if cpu_id == current_cpu {
                continue;
            }
            // SAFETY: the callback has no argument and completes synchronously
            // before this call returns.
            let _ = unsafe {
                crate::irq::run_on_cpu_sync(
                    crate::irq::CpuId(cpu_id),
                    flush_icache_all_thunk,
                    core::ptr::null_mut(),
                )
            };
        }
        flush_icache_all();
    }
    #[cfg(not(feature = "ipi"))]
    {
        flush_icache_all();
    }
}

#[cfg(feature = "ipi")]
/// Runs a complete instruction-cache invalidation on a remote CPU.
///
/// # Safety
///
/// The callback ignores its opaque argument and must be invoked synchronously.
unsafe fn flush_icache_all_thunk(_arg: *mut ()) {
    flush_icache_all();
}

/// Cleans a data-cache range to the point of unification when needed.
pub fn clean_dcache_to_pou(vaddr: VirtAddr, size: usize) {
    #[cfg(target_arch = "aarch64")]
    {
        ax_cpu::asm::clean_dcache_range_to_pou(vaddr, size);
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = (vaddr, size);
    }
}

/// Synchronizes modified kernel text with the local execution pipeline.
pub fn sync_kernel_text(start: VirtAddr, size: usize) {
    flush_tlb_range(start, size);
    flush_icache_all();
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    struct ModelShootdown {
        online: [bool; 3],
        remote_error: Option<TlbShootdownError>,
        remote_cpu: Cell<Option<usize>>,
        local_flushed: Cell<bool>,
    }

    impl TlbShootdown for ModelShootdown {
        fn cpu_count(&self) -> usize {
            self.online.len()
        }

        fn current_cpu(&self) -> usize {
            0
        }

        fn cpu_online(&self, cpu_id: usize) -> bool {
            self.online[cpu_id]
        }

        fn flush_remote(
            &self,
            cpu_id: usize,
            _start: VirtAddr,
            _size: usize,
        ) -> Result<(), TlbShootdownError> {
            self.remote_cpu.set(Some(cpu_id));
            self.remote_error.map_or(Ok(()), Err)
        }

        fn flush_local(&self, _start: VirtAddr, _size: usize) {
            self.local_flushed.set(true);
        }
    }

    #[test]
    fn all_cpu_tlb_shootdown_propagates_remote_failure() {
        let runtime = ModelShootdown {
            online: [true; 3],
            remote_error: Some(TlbShootdownError::Timeout),
            remote_cpu: Cell::new(None),
            local_flushed: Cell::new(false),
        };

        let result = flush_tlb_range_all_cpus_with(&runtime, VirtAddr::from(0x4000), 0x2000);

        assert_eq!(result, Err(TlbShootdownError::Timeout));
        assert_eq!(runtime.remote_cpu.get(), Some(1));
        assert!(!runtime.local_flushed.get());
    }

    #[test]
    fn all_cpu_tlb_shootdown_skips_offline_cpus_then_flushes_local() {
        let runtime = ModelShootdown {
            online: [true, false, true],
            remote_error: None,
            remote_cpu: Cell::new(None),
            local_flushed: Cell::new(false),
        };

        let result = flush_tlb_range_all_cpus_with(&runtime, VirtAddr::from(0x4000), 0x2000);

        assert_eq!(result, Ok(()));
        assert_eq!(runtime.remote_cpu.get(), Some(2));
        assert!(runtime.local_flushed.get());
    }
}
