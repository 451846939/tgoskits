//! Top-level AxVM orchestration.

extern crate alloc;

#[cfg(not(feature = "fs"))]
use alloc::vec::Vec;
#[cfg(feature = "fs")]
use alloc::{string::String, vec, vec::Vec};

use ax_errno::AxResult;
use axvm::{AxVMRef, AxvmRuntime, VMId};

/// AxVM top-level manager.
///
/// This type belongs to the hypervisor application layer. It owns the policy
/// for loading default VM configs, starting/stopping VMs, and serving shell
/// commands. The lower `axvm` crate only supplies VM/runtime primitives.
pub struct AxvmManager {
    runtime: AxvmRuntime,
}

impl AxvmManager {
    /// Initialize the AxVM runtime services.
    pub fn new() -> AxResult<Self> {
        axvm::register_vm_lifecycle_hooks(on_vm_started, on_vm_stopping);
        Ok(Self {
            runtime: AxvmRuntime::new()?,
        })
    }

    /// Load and initialize the default VM set.
    pub fn init_default_vms(&self) {
        crate::config::init_guest_vms();
        self.runtime.init_vms();
        self.release_host_filesystem_for_guest_passthrough();
    }

    /// Start the default VM set and wait until it exits.
    pub fn start_default_vms(&self) {
        self.runtime.start_default_vms();
    }

    /// Create one VM from a TOML config string.
    pub fn create_vm_from_toml(raw_cfg: &str) -> AxResult<VMId> {
        crate::config::init_guest_vm(raw_cfg)
    }

    /// Start a VM by ID.
    pub fn start_vm(vm_id: VMId) -> AxResult {
        AxvmRuntime::start_vm(vm_id)
    }

    /// Stop a VM by ID.
    pub fn stop_vm(vm_id: VMId) -> AxResult {
        AxvmRuntime::stop_vm(vm_id)
    }

    /// Resume a VM by ID.
    pub fn resume_vm(vm_id: VMId) -> AxResult {
        AxvmRuntime::resume_vm(vm_id)
    }

    /// Reset a VM by ID.
    pub fn reset_vm(vm_id: VMId) -> AxResult {
        AxvmRuntime::reset_vm(vm_id)
    }

    /// Remove a VM by ID.
    pub fn remove_vm(vm_id: VMId) -> Option<AxVMRef> {
        #[cfg(target_arch = "aarch64")]
        unregister_aarch64_passthrough_irq_routes(vm_id);
        #[cfg(target_arch = "loongarch64")]
        unregister_loongarch_passthrough_irq_routes(vm_id);
        AxvmRuntime::remove_vm(vm_id)
    }

    /// Run a closure with a VM by ID.
    pub fn with_vm<T>(vm_id: VMId, f: impl FnOnce(AxVMRef) -> T) -> Option<T> {
        AxvmRuntime::with_vm(vm_id, f)
    }

    /// Return the current VM list snapshot.
    pub fn vm_list() -> Vec<AxVMRef> {
        axvm::get_vm_list()
    }

    /// Return one VM by ID.
    pub fn vm_by_id(vm_id: VMId) -> Option<AxVMRef> {
        axvm::get_vm_by_id(vm_id)
    }

    #[cfg(all(
        feature = "fs",
        any(target_arch = "x86_64", target_arch = "loongarch64")
    ))]
    fn release_host_filesystem_for_guest_passthrough(&self) {
        if !crate::config::host_filesystem_release_required() {
            return;
        }

        axvm::shutdown_host_filesystems().expect(
            "Failed to release host filesystem before guest passthrough devices take ownership",
        );
        #[cfg(target_arch = "x86_64")]
        crate::config::prepare_x86_host_fs_passthrough_devices();
        info!("Host filesystem cleanly unmounted before guest passthrough devices start");
    }

    #[cfg(not(all(
        feature = "fs",
        any(target_arch = "x86_64", target_arch = "loongarch64")
    )))]
    fn release_host_filesystem_for_guest_passthrough(&self) {}

    /// Read VM config files from an Axvisor-owned directory.
    #[cfg(feature = "fs")]
    pub fn filesystem_vm_configs(config_dir: &str) -> Vec<String> {
        let mut configs = Vec::new();

        debug!("Read VM config files from filesystem.");

        let entries = match ax_std::fs::read_dir(config_dir) {
            Ok(entries) => {
                info!("Find dir: {}", config_dir);
                entries
            }
            Err(_) => {
                info!("NOT find dir: {} in filesystem", config_dir);
                return configs;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    warn!("Failed to read config directory entry: {e:?}");
                    continue;
                }
            };
            let path = entry.path();
            let path_str = path.as_str();
            debug!("Considering file: {}", path_str);
            if !path_str.ends_with(".toml") {
                continue;
            }

            let file_size = match Self::file_size(path_str) {
                Ok(file_size) => file_size,
                Err(e) => {
                    error!("Failed to get config file {} metadata: {:?}", path_str, e);
                    continue;
                }
            };
            info!("File {} size: {}", path_str, file_size);

            if file_size == 0 {
                warn!("File {} is empty", path_str);
                continue;
            }

            let buffer = match Self::read_file_exact(path_str, file_size) {
                Ok(buffer) => buffer,
                Err(e) => {
                    error!("Failed to read file {}: {:?}", path_str, e);
                    continue;
                }
            };

            match String::from_utf8(buffer) {
                Ok(content) => configs.push(content),
                Err(e) => error!("Config file {} is not valid UTF-8: {:?}", path_str, e),
            }
        }

        configs
    }

    #[cfg(feature = "fs")]
    fn open_file(file_name: &str) -> AxResult<ax_std::fs::File> {
        ax_std::fs::File::open(file_name).map_err(|err| {
            ax_errno::ax_err_type!(
                NotFound,
                alloc::format!(
                    "Failed to open {}, err {:?}, please check your disk.img",
                    file_name,
                    err
                )
            )
        })
    }

    #[cfg(feature = "fs")]
    pub fn file_size(file_name: &str) -> AxResult<usize> {
        Self::open_file(file_name)?
            .metadata()
            .map_err(|err| {
                ax_errno::ax_err_type!(
                    Io,
                    alloc::format!(
                        "Failed to get metadate of file {}, err {:?}",
                        file_name,
                        err
                    )
                )
            })
            .map(|metadata| metadata.size() as usize)
    }

    #[cfg(feature = "fs")]
    pub fn read_file_exact(file_name: &str, read_size: usize) -> AxResult<Vec<u8>> {
        use ax_std::io::Read;

        let mut file = Self::open_file(file_name)?;
        let mut buffer = vec![0u8; read_size];
        file.read_exact(&mut buffer).map_err(|err| {
            ax_errno::ax_err_type!(
                Io,
                alloc::format!("Failed in reading from file {}, err {:?}", file_name, err)
            )
        })?;
        Ok(buffer)
    }

    #[cfg(feature = "fs")]
    pub fn read_file(file_name: &str) -> AxResult<Vec<u8>> {
        let size = Self::file_size(file_name)?;
        Self::read_file_exact(file_name, size)
    }
}

fn on_vm_started(_vm_id: VMId) {
    // Passthrough IRQ routes are registered masked. The guest driver enables
    // each line through its virtual interrupt controller only after installing
    // the corresponding handler, avoiding an interrupt-before-handler race.
}

fn on_vm_stopping(vm_id: VMId) {
    #[cfg(target_arch = "aarch64")]
    mask_aarch64_passthrough_irq_routes(vm_id);
}

#[cfg(target_arch = "aarch64")]
pub(crate) fn register_aarch64_passthrough_irq_routes(vm_id: VMId) {
    const GIC_SPI_BASE: usize = 32;

    let Some(vm) = axvm::get_vm_by_id(vm_id) else {
        warn!("cannot register AArch64 IRQ routes for missing VM[{vm_id}]");
        return;
    };
    if vm.interrupt_mode() != axvm::config::VMInterruptMode::Passthrough {
        return;
    }

    let vcpu_mappings = vm.get_vcpu_affinities_pcpu_ids();
    if vcpu_mappings.is_empty() {
        warn!("VM[{vm_id}] has no vCPU available for AArch64 passthrough IRQ routing");
        return;
    }

    for (vcpu_id, affinity_mask, _) in &vcpu_mappings {
        let Some(target_pcpu) = affinity_mask.and_then(single_cpu_from_mask) else {
            warn!(
                "VM[{vm_id}] VCpu[{vcpu_id}] has no exclusive single-pCPU affinity; private IRQ \
                 fallback routing is disabled for this vCPU"
            );
            continue;
        };
        match axvm::register_aarch64_guest_private_irq_target(target_pcpu, vm_id, *vcpu_id) {
            Ok(true) => info!(
                "Registered AArch64 pCPU {target_pcpu} private IRQ target for VM[{vm_id}] \
                 VCpu[{vcpu_id}]"
            ),
            Ok(false) => {}
            Err(err) => warn!(
                "failed to register AArch64 pCPU {target_pcpu} private IRQ target for VM[{vm_id}] \
                 VCpu[{vcpu_id}]: {err:?}"
            ),
        }
    }

    let spis = vm.with_config(|cfg| cfg.pass_through_spis().to_vec());
    if spis.is_empty() {
        let has_passthrough = vm.with_config(|cfg| !cfg.pass_through_devices().is_empty());
        if has_passthrough {
            warn!("VM[{vm_id}] has passthrough devices but no AArch64 SPI route was parsed");
        }
        return;
    }

    let (vcpu_id, affinity_mask, _) = vcpu_mappings[0];
    let target_pcpu = affinity_mask.and_then(single_cpu_from_mask);

    info!(
        "Registering {} AArch64 passthrough IRQ route(s) for VM[{vm_id}] VCpu[{vcpu_id}], \
         target_pcpu={target_pcpu:?}",
        spis.len()
    );
    for spi in spis {
        let physical_irq = spi.spi as usize + GIC_SPI_BASE;
        match axvm::register_aarch64_guest_irq_route(
            physical_irq,
            vm_id,
            vcpu_id,
            physical_irq,
            target_pcpu,
            spi.trigger,
        ) {
            Ok(_) => {
                let resolved_irq = match ax_hal::irq::resolve_percpu_irq(ax_hal::irq::HwIrq(
                    physical_irq as u32,
                )) {
                    Ok(irq) => irq,
                    Err(err) => {
                        warn!(
                            "failed to resolve AArch64 physical IRQ {physical_irq} for VM[{vm_id}]: \
                             {err:?}"
                        );
                        continue;
                    }
                };

                if let Some(cpu_id) = target_pcpu
                    && let Err(err) = ax_hal::irq::set_affinity(
                        resolved_irq,
                        ax_hal::irq::IrqAffinity::Fixed(ax_hal::irq::CpuId(cpu_id)),
                    )
                {
                    warn!(
                        "failed to set AArch64 physical IRQ {physical_irq} affinity to pCPU \
                             {cpu_id}: {err:?}"
                    );
                }

                if let Err(err) = ax_hal::irq::set_enable(resolved_irq, false) {
                    warn!(
                        "failed to keep AArch64 physical IRQ {physical_irq} masked while \
                         preparing VM[{vm_id}]: \
                         {err:?}"
                    );
                } else {
                    info!(
                        "Prepared masked AArch64 passthrough physical IRQ {physical_irq} for \
                         VM[{vm_id}] VCpu[{vcpu_id}]"
                    );
                }
            }
            Err(err) => warn!(
                "skipping conflicting AArch64 physical IRQ {physical_irq} route for VM[{vm_id}] \
                 VCpu[{vcpu_id}]: {err:?}"
            ),
        }
    }
}

#[cfg(target_arch = "aarch64")]
fn mask_aarch64_passthrough_irq_routes(vm_id: VMId) {
    const GIC_SPI_BASE: usize = 32;

    let Some(vm) = axvm::get_vm_by_id(vm_id) else {
        warn!("cannot update AArch64 IRQ routes for missing VM[{vm_id}]");
        return;
    };
    if vm.interrupt_mode() != axvm::config::VMInterruptMode::Passthrough {
        return;
    }

    let spis = vm.with_config(|cfg| cfg.pass_through_spis().to_vec());
    for spi in spis {
        let physical_irq = spi.spi as usize + GIC_SPI_BASE;
        let result = ax_hal::irq::resolve_percpu_irq(ax_hal::irq::HwIrq(physical_irq as u32))
            .and_then(|irq| ax_hal::irq::set_enable(irq, false));
        if let Err(err) = result {
            warn!(
                "failed to mask AArch64 physical IRQ {physical_irq} for \
                 VM[{vm_id}]: {err:?}"
            );
        }
    }

    info!("AArch64 passthrough IRQ routes for VM[{vm_id}] are masked");
}

#[cfg(target_arch = "aarch64")]
fn single_cpu_from_mask(mask: usize) -> Option<usize> {
    (mask.count_ones() == 1).then_some(mask.trailing_zeros() as usize)
}

#[cfg(target_arch = "aarch64")]
fn unregister_aarch64_passthrough_irq_routes(vm_id: VMId) {
    const GIC_SPI_BASE: usize = 32;

    if let Some(vm) = axvm::get_vm_by_id(vm_id) {
        let spis = vm.with_config(|cfg| cfg.pass_through_spis().to_vec());
        for spi in spis {
            let physical_irq = spi.spi as usize + GIC_SPI_BASE;
            let disable_result =
                ax_hal::irq::resolve_percpu_irq(ax_hal::irq::HwIrq(physical_irq as u32))
                    .and_then(|irq| ax_hal::irq::set_enable(irq, false));
            if let Err(err) = disable_result {
                warn!(
                    "failed to disable AArch64 physical IRQ {physical_irq} while removing VM[{vm_id}]: \
                     {err:?}"
                );
            }
        }
    }

    let removed = axvm::unregister_aarch64_guest_irq_routes(vm_id);
    if removed != 0 {
        info!("Removed {removed} AArch64 passthrough IRQ route(s) for VM[{vm_id}]");
    }
    let removed_private_targets = axvm::unregister_aarch64_guest_private_irq_targets(vm_id);
    if removed_private_targets != 0 {
        info!(
            "Removed {removed_private_targets} AArch64 private IRQ pCPU target(s) for VM[{vm_id}]"
        );
    }
}

#[cfg(target_arch = "loongarch64")]
pub(crate) fn register_loongarch_passthrough_irq_routes(vm_id: VMId) {
    let routes = axvm::boot::guest_platform::loongarch64::get_guest_irq_routes(vm_id);
    if routes.is_empty() {
        if let Some(vm) = axvm::get_vm_by_id(vm_id) {
            let passthrough = vm.with_config(|cfg| !cfg.pass_through_devices().is_empty());
            if passthrough {
                warn!(
                    "VM[{vm_id}] has passthrough devices but no LoongArch guest IRQ route parsed"
                );
            }
        }
        return;
    }

    let vcpu_id = 0usize;
    info!(
        "Registering {} LoongArch passthrough IRQ route(s) for VM[{vm_id}]",
        routes.len()
    );
    for route in routes {
        axvm::register_loongarch_guest_irq_route(
            route.physical_irq,
            vm_id,
            vcpu_id,
            route.guest_vector,
        );
    }
}

#[cfg(target_arch = "loongarch64")]
fn unregister_loongarch_passthrough_irq_routes(vm_id: VMId) {
    axvm::unregister_loongarch_guest_irq_routes(vm_id);
}
