//! Per-vCPU emulated architectural timers connected to VGIC PPI state.

use alloc::sync::Arc;

use arm_vgic::{PpiId, VgicCore};
use axdevice::{
    DeviceBuildContext, DeviceBundle, DeviceFactory, DeviceManagerError, DeviceManagerResult,
    DeviceRegistration,
};
use axvm_types::{EmulatedDeviceConfig, EmulatedDeviceType};

use self::device::VirtualTimerBank;
use crate::{AxVmError, AxVmResult};

mod device;
mod state;

pub(super) use state::{VirtualTimerRelay, counter_frequency};

/// Factory for the machine-owned AArch64 architectural timer bank.
pub(crate) struct Aarch64VtimerFactory {
    vgic: Arc<VgicCore>,
    vcpu_count: usize,
    ppi: PpiId,
    frequency: u64,
}

impl Aarch64VtimerFactory {
    pub(crate) fn new(vgic: Arc<VgicCore>, vcpu_count: usize, ppi: PpiId) -> AxVmResult<Self> {
        let frequency = counter_frequency();
        if frequency == 0 {
            return Err(AxVmError::unsupported(
                "create AArch64 architectural timers",
                "CNTFRQ_EL0 reports zero",
            ));
        }
        Ok(Self {
            vgic,
            vcpu_count,
            ppi,
            frequency,
        })
    }
}

impl DeviceFactory for Aarch64VtimerFactory {
    fn device_type(&self) -> EmulatedDeviceType {
        EmulatedDeviceType::Aarch64Vtimer
    }

    fn build(
        &self,
        config: &EmulatedDeviceConfig,
        _context: &DeviceBuildContext<'_>,
    ) -> DeviceManagerResult<DeviceBundle> {
        if config.emu_type != EmulatedDeviceType::Aarch64Vtimer
            || config.base_gpa != 0
            || config.length != 0
            || config.irq_id != 0
            || !config.cfg_list.is_empty()
        {
            return Err(DeviceManagerError::InvalidConfig {
                operation: "build AArch64 architectural timers",
                detail: "timer resources are fixed by the AArch64 machine profile".into(),
            });
        }
        Ok(DeviceBundle::from_registration(DeviceRegistration::Device(
            Arc::new(VirtualTimerBank::new(
                self.vgic.clone(),
                self.vcpu_count,
                self.ppi,
                self.frequency,
            )),
        )))
    }
}
