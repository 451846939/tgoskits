use ax_plat::power::PowerIf;

struct PowerImpl;

#[impl_plat_interface]
impl PowerIf for PowerImpl {
    /// Requests that the platform release the given CPU core.
    ///
    /// Where `cpu_id` is the logical CPU ID (0, 1, ..., N-1, N is the number of
    /// CPU cores on the platform).
    #[cfg(feature = "smp")]
    fn cpu_boot(cpu_id: usize) {
        somehal::power::cpu_on(cpu_id).unwrap();
    }

    /// Shutdown the whole system.
    fn system_off() -> ! {
        somehal::power::shutdown()
    }

    /// Reset the whole system.
    fn system_reset() -> ! {
        somehal::power::reset()
    }

    /// Get the number of CPU cores available on this platform.
    fn cpu_num() -> usize {
        somehal::smp::cpu_count()
    }
}
