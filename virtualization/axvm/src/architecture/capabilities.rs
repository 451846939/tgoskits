//! Small capability boundaries implemented by the selected guest architecture.

use crate::AxVmResult;

/// Selects the smallest capability across a default and every target CPU.
pub(crate) fn minimum_cpu_capability(
    default: usize,
    cpu_capabilities: impl IntoIterator<Item = usize>,
) -> usize {
    cpu_capabilities
        .into_iter()
        .fold(default, |minimum, capability| minimum.min(capability))
}

/// Selects the smallest capability supported by every CPU on which a VM's
/// vCPUs may run.
pub(crate) fn minimum_target_cpu_capability(
    default: usize,
    vcpu_mappings: &[(usize, Option<usize>, usize)],
    mut capability_for_cpu: impl FnMut(usize) -> usize,
) -> usize {
    minimum_cpu_capability(
        default,
        crate::architecture::ops::target_phys_cpu_ids(vcpu_mappings)
            .into_iter()
            .map(&mut capability_for_cpu),
    )
}

/// Architecture selection for fixed guest machine resources.
pub(crate) trait MachinePlatform {
    const MACHINE_ARCHITECTURE: crate::machine::MachineArchitecture;
}

/// Guest firmware preparation performed before common VM memory loading.
pub(crate) trait GuestBootPlatform {
    fn init_guest_boot_resources() {}

    fn prepare_guest_boot(
        _vm_config: &mut crate::config::AxVMConfig,
        _vm_create_config: &mut axvmconfig::GuestConfig,
        _provider: &dyn crate::boot::BootImageProvider,
    ) -> AxVmResult<Option<crate::boot::fdt::GuestDtbImage>> {
        Ok(None)
    }
}

/// Architecture-specific guest image planning layered over common byte loading.
pub(crate) trait BootImagePlatform {
    fn default_boot_firmware_load_gpa(
        _config: &axvmconfig::GuestConfig,
    ) -> Option<axvm_types::GuestPhysAddr> {
        None
    }

    fn load_images_from_memory(
        loader: &mut crate::boot::images::ImageLoaderCore<'_>,
        images: crate::boot::StaticVmImage,
    ) -> AxVmResult {
        loader.load_standard_images_from_memory(images, Self::load_guest_dtb)
    }

    #[cfg(any(feature = "fs", feature = "host-fs"))]
    fn load_images_from_filesystem(
        loader: &mut crate::boot::images::ImageLoaderCore<'_>,
    ) -> AxVmResult {
        loader.load_standard_images_from_filesystem(Self::load_guest_dtb)
    }

    fn load_guest_dtb(
        _loader: &crate::boot::images::ImageLoaderCore<'_>,
        _dtb: &crate::boot::fdt::GuestDtbImage,
    ) -> AxVmResult {
        Ok(())
    }

    fn is_x86_linux_image_config(
        _config: &axvmconfig::GuestConfig,
        _provider: &dyn crate::boot::BootImageProvider,
    ) -> bool {
        false
    }
}

/// Architecture-specific host timer policy used by the ArceOS adapter.
pub(crate) trait HostTimePlatform {
    fn set_oneshot_timer(deadline_ns: u64) {
        ax_std::os::arceos::modules::ax_hal::time::set_oneshot_timer(deadline_ns);
    }

    fn register_timer_callback() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_cpu_capability_uses_the_weakest_heterogeneous_cpu() {
        assert_eq!(minimum_cpu_capability(48, [48, 44]), 44);
    }

    #[test]
    fn minimum_cpu_capability_keeps_the_default_without_targets() {
        assert_eq!(minimum_cpu_capability(39, []), 39);
    }

    #[test]
    fn target_cpu_capability_includes_every_cpu_in_vcpu_affinity_masks() {
        let mappings = [
            (0, Some((1 << 0) | (1 << 2)), 0),
            (1, None, 1),
            (2, Some((1 << 2) | (1 << 3)), 0),
        ];
        let capabilities = [48, 44, 42, 39];

        assert_eq!(
            minimum_target_cpu_capability(52, &mappings, |cpu_id| capabilities[cpu_id]),
            39
        );
    }
}
