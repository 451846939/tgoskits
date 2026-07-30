//! AArch64 VM-local VGIC construction and activation lifecycle.

use alloc::{sync::Arc, vec::Vec};

use arm_vgic::{
    ArmVgicConfig, AssignedSpiConfig, GicAffinity, GicV3Backend, GicV3VcpuBinding, GicV3VcpuWake,
    GicVcpuId, HostGicVersion, PpiId, SpiId, TriggerMode, VgicAccessContext, VgicCore,
    VgicDeviceSet, VgicError, VgicMmioRegion, VgicResult, VgicV3Config,
};
use ax_kspin::SpinNoIrq;
use axdevice::{
    DeviceBuildContext, DeviceBundle, DeviceFactory, DeviceFactoryRegistry, DeviceManagerError,
    DeviceManagerResult, DeviceRegistration, ServiceCardinality, ServiceKey,
    VirtualInterruptControllerKey,
};
use axdevice_base::{HostIrqId, VirtualInterruptController};
use axvm_types::{EmulatedDeviceConfig, EmulatedDeviceType};

use super::gic::{self, AssignedSpiRoutes};
use crate::{AxVmError, AxVmResult, irq::deferred::DeferredVcpuKick};

const REDISTRIBUTOR_STRIDE: u64 = 0x2_0000;
pub(crate) const GUEST_VIRTUAL_TIMER_PPI: u8 = 27;
pub(crate) const GUEST_PHYSICAL_TIMER_PPI: u8 = 30;

/// Typed VM-local service for vCPU attachment and physical-source lifecycle.
pub(crate) struct Aarch64VgicRuntimeKey;

impl ServiceKey for Aarch64VgicRuntimeKey {
    type Service = Aarch64VgicRuntime;

    const NAME: &'static str = "aarch64-vgic-runtime";
    const CARDINALITY: ServiceCardinality = ServiceCardinality::Single;
}

enum RuntimePhase {
    Inactive,
    Activating,
    Active(Arc<AssignedSpiRoutes>),
    Deactivating,
}

/// VM-owned control-plane state that is deliberately separate from IRQ state.
pub(crate) struct Aarch64VgicRuntime {
    core: Arc<VgicCore>,
    kick: Arc<DeferredVcpuKick>,
    phase: SpinNoIrq<RuntimePhase>,
}

impl Aarch64VgicRuntime {
    fn new(vm_id: usize, core: Arc<VgicCore>) -> Arc<Self> {
        Arc::new(Self {
            core,
            kick: DeferredVcpuKick::new(vm_id),
            phase: SpinNoIrq::new(RuntimePhase::Inactive),
        })
    }

    pub(crate) fn core(&self) -> &Arc<VgicCore> {
        &self.core
    }

    pub(crate) fn attach_vcpu(&self, vcpu_id: usize) -> VgicResult<GicV3VcpuBinding> {
        let binding = self.core.attach_vcpu(
            vcpu_id,
            Arc::new(Aarch64VcpuWake {
                kick: self.kick.clone(),
                vcpu_id,
            }),
        )?;
        for ppi in [GUEST_VIRTUAL_TIMER_PPI, GUEST_PHYSICAL_TIMER_PPI] {
            self.core.controller().configure_ppi_input(
                GicVcpuId::new(vcpu_id),
                PpiId::new(ppi)?,
                TriggerMode::Level,
            )?;
        }
        Ok(binding)
    }

    /// Claims host sources and publishes their fixed hard-IRQ routes.
    pub(crate) fn activate(&self) -> AxVmResult {
        {
            let mut phase = self.phase.lock();
            match &*phase {
                RuntimePhase::Inactive => *phase = RuntimePhase::Activating,
                RuntimePhase::Active(_) => return Ok(()),
                RuntimePhase::Activating | RuntimePhase::Deactivating => {
                    return Err(AxVmError::resource_conflict(
                        "AArch64 VGIC lifecycle",
                        "another lifecycle transition is in progress",
                    ));
                }
            }
        }

        self.kick.start();
        if let Err(error) = self.core.bind_assigned_spis() {
            self.kick.stop();
            *self.phase.lock() = RuntimePhase::Inactive;
            return Err(AxVmError::interrupt("bind assigned physical SPIs", error));
        }

        let routes = match gic::register_assigned_spi_routes(&self.core) {
            Ok(routes) => routes,
            Err(error) => {
                if let Err(rollback_error) = self.core.unbind_assigned_spis() {
                    warn!(
                        "failed to roll back AArch64 assigned SPIs after route error: \
                         {rollback_error}"
                    );
                }
                self.kick.stop();
                *self.phase.lock() = RuntimePhase::Inactive;
                return Err(AxVmError::interrupt(
                    "register assigned physical SPI routes",
                    error,
                ));
            }
        };
        *self.phase.lock() = RuntimePhase::Active(routes);
        Ok(())
    }

    /// Removes routes only after every physical delivery is quiescent.
    pub(crate) fn deactivate(&self) -> AxVmResult {
        let routes = {
            let mut phase = self.phase.lock();
            match core::mem::replace(&mut *phase, RuntimePhase::Deactivating) {
                RuntimePhase::Inactive => {
                    *phase = RuntimePhase::Inactive;
                    return Ok(());
                }
                RuntimePhase::Active(routes) => routes,
                RuntimePhase::Activating | RuntimePhase::Deactivating => {
                    *phase = RuntimePhase::Deactivating;
                    return Err(AxVmError::resource_conflict(
                        "AArch64 VGIC lifecycle",
                        "another lifecycle transition is in progress",
                    ));
                }
            }
        };

        routes.quiesce();
        if let Err(error) = self.core.teardown_assigned_spis() {
            routes.resume();
            *self.phase.lock() = RuntimePhase::Active(routes);
            return Err(AxVmError::interrupt(
                "tear down assigned physical SPIs",
                error,
            ));
        }

        // Dropping the route handles removes the static hard-IRQ lookup before
        // the task-context kick worker is stopped.
        drop(routes);
        self.kick.stop();
        *self.phase.lock() = RuntimePhase::Inactive;
        Ok(())
    }
}

impl Drop for Aarch64VgicRuntime {
    fn drop(&mut self) {
        if let Err(error) = self.deactivate() {
            warn!("failed to deactivate AArch64 VGIC runtime while dropping it: {error:?}");
        }
    }
}

struct Aarch64VcpuWake {
    kick: Arc<DeferredVcpuKick>,
    vcpu_id: usize,
}

impl GicV3VcpuWake for Aarch64VcpuWake {
    fn wake(&self) -> VgicResult {
        self.kick
            .publish_from_irq(self.vcpu_id)
            .map_err(|error| VgicError::Backend {
                operation: "publish deferred AArch64 vCPU kick",
                detail: alloc::format!("{error}"),
            })
    }
}

#[derive(Clone)]
struct DeviceFingerprint {
    name: alloc::string::String,
    base_gpa: usize,
    length: usize,
    irq_id: usize,
    device_type: EmulatedDeviceType,
    cfg_list: Vec<usize>,
}

impl DeviceFingerprint {
    fn from_config(config: &EmulatedDeviceConfig) -> Self {
        Self {
            name: config.name.clone(),
            base_gpa: config.base_gpa,
            length: config.length,
            irq_id: config.irq_id,
            device_type: config.emu_type,
            cfg_list: config.cfg_list.clone(),
        }
    }

    fn validate(
        &self,
        config: &EmulatedDeviceConfig,
        operation: &'static str,
    ) -> DeviceManagerResult {
        if self.name != config.name
            || self.base_gpa != config.base_gpa
            || self.length != config.length
            || self.irq_id != config.irq_id
            || self.device_type != config.emu_type
            || self.cfg_list != config.cfg_list
        {
            return Err(DeviceManagerError::InvalidConfig {
                operation,
                detail: alloc::format!(
                    "device '{}' does not match the immutable machine plan",
                    config.name
                ),
            });
        }
        Ok(())
    }
}

struct Aarch64VgicFactory {
    vm_id: usize,
    expected: DeviceFingerprint,
    runtime: Arc<Aarch64VgicRuntime>,
}

impl DeviceFactory for Aarch64VgicFactory {
    fn device_type(&self) -> EmulatedDeviceType {
        EmulatedDeviceType::InterruptController
    }

    fn build(
        &self,
        config: &EmulatedDeviceConfig,
        _context: &DeviceBuildContext<'_>,
    ) -> DeviceManagerResult<DeviceBundle> {
        self.expected
            .validate(config, "build AArch64 virtual GIC")?;
        let access_context: Arc<dyn VgicAccessContext> =
            Arc::new(AxvmVgicAccessContext { vm_id: self.vm_id });
        let devices = VgicDeviceSet::new(self.runtime.core.clone(), access_context)
            .map_err(|error| vgic_device_error("build AArch64 virtual GIC frontends", error))?;
        let mut bundle = DeviceBundle::new();
        for device in devices.into_devices() {
            bundle.push(DeviceRegistration::Device(device));
        }
        let controller: Arc<dyn VirtualInterruptController> = self.runtime.core.clone();
        bundle
            .with_service::<Aarch64VgicRuntimeKey>(self.runtime.clone())?
            .with_service::<VirtualInterruptControllerKey>(controller)
    }
}

struct Aarch64RedistributorMarkerFactory {
    expected: DeviceFingerprint,
}

impl DeviceFactory for Aarch64RedistributorMarkerFactory {
    fn device_type(&self) -> EmulatedDeviceType {
        EmulatedDeviceType::GPPTRedistributor
    }

    fn build(
        &self,
        config: &EmulatedDeviceConfig,
        _context: &DeviceBuildContext<'_>,
    ) -> DeviceManagerResult<DeviceBundle> {
        self.expected
            .validate(config, "validate AArch64 virtual GIC redistributors")?;
        // The Distributor contribution atomically registers every frontend
        // from one VgicCore. This marker consumes the machine descriptor so no
        // second Redistributor construction path exists.
        Ok(DeviceBundle::new())
    }
}

struct AxvmVgicAccessContext {
    vm_id: usize,
}

impl VgicAccessContext for AxvmVgicAccessContext {
    fn current_vcpu(&self) -> Option<usize> {
        (crate::current_vm_id() == Some(self.vm_id))
            .then(crate::current_vcpu_id)
            .flatten()
    }
}

/// Creates the canonical controller and registers its only construction path.
pub(crate) fn register_device_factories(
    vm: &crate::vm::AxVM,
    registry: &mut DeviceFactoryRegistry,
) -> AxVmResult<Arc<Aarch64VgicRuntime>> {
    let (configs, placements, passthrough_irqs) = vm.with_config(|config| {
        (
            config.emu_devices().clone(),
            config.phys_cpu_ls.get_vcpu_affinities_pcpu_ids(),
            config.pass_through_irqs().to_vec(),
        )
    });
    let distributor = unique_config(
        &configs,
        EmulatedDeviceType::InterruptController,
        "AArch64 virtual GIC Distributor",
    )?;
    let redistributors = unique_config(
        &configs,
        EmulatedDeviceType::GPPTRedistributor,
        "AArch64 virtual GIC Redistributors",
    )?;
    let [configured_vcpu_count] = redistributors.cfg_list.as_slice() else {
        return Err(AxVmError::invalid_config(
            "AArch64 redistributor descriptor requires one vCPU count",
        ));
    };
    if *configured_vcpu_count != placements.len() {
        return Err(AxVmError::invalid_config(alloc::format!(
            "AArch64 redistributor descriptor names {} vCPUs, but placement has {}",
            configured_vcpu_count,
            placements.len()
        )));
    }

    let backend =
        gic::backend().map_err(|error| AxVmError::interrupt("create host GIC backend", error))?;
    let capabilities = backend.capabilities();
    if capabilities.host_version() != HostGicVersion::V3 {
        return Err(AxVmError::unsupported(
            "create AArch64 virtual GIC",
            "the machine profile requires a GICv3 host CPU interface",
        ));
    }
    let affinities = placements
        .iter()
        .map(|(_, _, physical_id)| GicAffinity::from_mpidr(*physical_id as u64))
        .collect();
    let assigned_spis = assigned_spis(&passthrough_irqs)?;
    let vgic_config = VgicV3Config::new(
        axdevice_base::InterruptControllerId::new(0),
        VgicMmioRegion::new(distributor.base_gpa as u64, distributor.length as u64)
            .map_err(|error| AxVmError::interrupt("validate GIC Distributor range", error))?,
        alloc::vec![
            VgicMmioRegion::new(redistributors.base_gpa as u64, redistributors.length as u64,)
                .map_err(|error| AxVmError::interrupt("validate GIC Redistributor range", error))?,
        ],
        REDISTRIBUTOR_STRIDE,
        affinities,
    )
    .and_then(|config| config.with_spi_count(gic::host_spi_count()?))
    .and_then(|config| config.with_list_register_count(capabilities.list_register_count()))
    .and_then(|config| config.with_priority_bits(capabilities.priority_bits()))
    .and_then(|config| config.with_assigned_spis(assigned_spis))
    .map_err(|error| AxVmError::interrupt("construct AArch64 virtual GIC", error))?;
    let core = Arc::new(
        VgicCore::new(ArmVgicConfig::V3(vgic_config), backend)
            .map_err(|error| AxVmError::interrupt("create AArch64 virtual GIC", error))?,
    );
    let runtime = Aarch64VgicRuntime::new(vm.id(), core);

    registry.register(Arc::new(Aarch64VgicFactory {
        vm_id: vm.id(),
        expected: DeviceFingerprint::from_config(distributor),
        runtime: runtime.clone(),
    }))?;
    registry.register(Arc::new(Aarch64RedistributorMarkerFactory {
        expected: DeviceFingerprint::from_config(redistributors),
    }))?;
    Ok(runtime)
}

fn assigned_spis(
    configured: &[crate::config::PassthroughInterrupt],
) -> AxVmResult<Vec<AssignedSpiConfig>> {
    configured
        .iter()
        .map(|route| {
            let intid = route.source.checked_add(32).ok_or_else(|| {
                AxVmError::invalid_config("AArch64 passthrough SPI number overflows")
            })?;
            AssignedSpiConfig::new(
                SpiId::new(intid)
                    .map_err(|error| AxVmError::interrupt("validate assigned SPI", error))?,
                HostIrqId::new(intid as usize),
                0,
                route.trigger,
            )
            .map_err(|error| AxVmError::interrupt("plan assigned physical SPI", error))
        })
        .collect()
}

fn unique_config<'a>(
    configs: &'a [EmulatedDeviceConfig],
    device_type: EmulatedDeviceType,
    resource: &'static str,
) -> AxVmResult<&'a EmulatedDeviceConfig> {
    let mut matches = configs
        .iter()
        .filter(|config| config.emu_type == device_type);
    let config = matches
        .next()
        .ok_or_else(|| AxVmError::resource_unavailable("machine device", resource))?;
    if matches.next().is_some() {
        return Err(AxVmError::resource_conflict(
            "machine device",
            alloc::format!("more than one {resource} descriptor is configured"),
        ));
    }
    Ok(config)
}

fn vgic_device_error(operation: &'static str, error: VgicError) -> DeviceManagerError {
    DeviceManagerError::InvalidConfig {
        operation,
        detail: alloc::format!("{error}"),
    }
}
