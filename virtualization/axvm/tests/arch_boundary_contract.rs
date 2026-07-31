// Copyright 2026 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[test]
fn vm_core_does_not_handle_arch_local_exits() {
    let vm_rs = include_str!("../src/vm/mod.rs");

    for forbidden in [
        "CurrentArch::handle_vcpu_exit",
        "VcpuRunAction",
        "HostInterrupt",
    ] {
        assert!(
            !vm_rs.contains(forbidden),
            "vm/mod.rs must not contain architecture-local exit handling detail: {forbidden}"
        );
    }
}

#[test]
fn common_vm_code_only_uses_high_level_arch_entrypoints() {
    let vm = include_str!("../src/vm/mod.rs");
    let preparation = include_str!("../src/vm/prepare.rs");
    let common_sources = [
        vm,
        preparation,
        include_str!("../src/vm/prepare/address_space.rs"),
        include_str!("../src/vm/prepare/devices.rs"),
        include_str!("../src/vm/prepare/vcpus.rs"),
    ];

    assert!(vm.contains("CurrentArch::create_vm_resources(config)"));
    assert!(preparation.contains("CurrentArch::init_vm"));

    for source in &common_sources {
        for line in source.lines().filter(|line| line.contains("CurrentArch::")) {
            assert!(
                line.contains("CurrentArch::create_vm_resources")
                    || line.contains("CurrentArch::init_vm")
                    || line.contains("CurrentArch::activate_devices")
                    || line.contains("CurrentArch::deactivate_devices"),
                "common VM code calls a fine-grained architecture hook: {line}"
            );
        }
    }

    for forbidden in [
        "configure_interrupt_fabric",
        "register_arch_devices",
        "append_arch_owned_regions",
        "map_arch_address_space",
        "new_vcpu_create_state",
        "build_vcpu_create_config",
        "build_vcpu_setup_config",
    ] {
        assert!(
            common_sources
                .iter()
                .all(|source| !source.contains(forbidden)),
            "common VM initialization must not call architecture step hook: {forbidden}"
        );
    }
}

#[test]
fn every_architecture_owns_vm_resource_creation_and_initialization() {
    for source in [
        include_str!("../src/arch/aarch64/vm.rs"),
        include_str!("../src/arch/loongarch64/vm.rs"),
        include_str!("../src/arch/riscv64/vm.rs"),
        include_str!("../src/arch/x86_64/vm.rs"),
    ] {
        assert!(source.contains("fn create_vm_resources"));
        assert!(source.contains("fn init_vm"));
    }
}

#[test]
fn custom_vm_init_inputs_cross_the_arch_boundary_unchanged() {
    let preparation = include_str!("../src/vm/prepare.rs");
    assert!(preparation.contains("VmInitRequest::Provided"));
    assert!(preparation.contains("factories: &'a mut DeviceFactoryRegistry"));
    assert!(preparation.contains("VmInitRequest::Provided { factories }"));
    assert!(!preparation.contains("interrupt_fabric"));

    for source in [
        include_str!("../src/arch/aarch64/vm.rs"),
        include_str!("../src/arch/loongarch64/vm.rs"),
        include_str!("../src/arch/riscv64/vm.rs"),
        include_str!("../src/arch/x86_64/vm.rs"),
    ] {
        assert!(source.contains("VmInitRequest::Provided"));
        assert!(source.contains("DeviceFactoryRegistry"));
        assert!(source.contains("init_vm_with(vm, factories,"));
    }
}

#[test]
fn failed_vm_initialization_resets_transient_resources_before_retry() {
    let preparation = include_str!("../src/vm/prepare.rs");
    let initialize = preparation
        .split_once("let prepared = match initialize")
        .expect("VM initialization must handle architecture errors")
        .1;

    assert!(initialize.contains("resources.reset_transient_resources()"));
    assert!(initialize.contains("return Err(err)"));
}

#[test]
fn runtime_vcpu_loop_only_consumes_scheduler_actions() {
    let runtime_vcpus_rs = include_str!("../src/runtime/vcpus.rs");

    for forbidden in [
        "VcpuRunAction::Continue",
        "VcpuRunAction::HostInterrupt",
        "HostInterrupt",
    ] {
        assert!(
            !runtime_vcpus_rs.contains(forbidden),
            "runtime/vcpus.rs must not match architecture-local exit action: {forbidden}"
        );
    }
}

#[test]
fn aarch64_enables_the_virtual_gic_interface_before_first_guest_entry() {
    let architecture = include_str!("../src/arch/aarch64/mod.rs");
    let cpu_interface = include_str!("../src/arch/aarch64/gic/cpu_interface.rs");

    assert!(
        architecture.contains("vgic_backend_result(binding.load())?")
            && architecture.contains("let save_result = vgic_backend_result(binding.save());"),
        "AArch64 must load and save the VM-local CPU interface around every guest entry"
    );
    assert!(cpu_interface.contains("ICH_HCR_EL2.set(0)"));
    assert!(cpu_interface.contains("ICH_HCR_EL2.set(hardware_v3_hcr_for_load(state.hcr()))"));
}

#[test]
fn aarch64_assigned_spis_use_split_eoi_and_explicit_teardown() {
    let gic = include_str!("../src/arch/aarch64/gic.rs");
    let physical_ingress = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/arch/aarch64/gic/physical.rs"),
    )
    .unwrap_or_default();
    let deferred_kick = include_str!("../src/irq/deferred.rs");
    let vgic = include_str!("../src/arch/aarch64/vgic.rs");
    let controller = include_str!("../../arm_vgic/src/controller/mod.rs");
    let physical = include_str!("../../arm_vgic/src/controller/physical.rs");
    let binding = include_str!("../../arm_vgic/src/controller/binding.rs");

    assert!(
        gic.contains("Acknowledges one host Group1 IRQ and performs only the priority drop")
            && gic.contains("physical::route_acknowledged_host_irq(token)")
            && !gic.contains("HandledAndMask"),
        "the host top half must priority-drop and transfer ownership into canonical VGIC state"
    );
    assert!(
        !gic.contains("Weak<VgicCore>")
            && !gic.contains("ASSIGNED_SPI_ROUTES: SpinNoIrq<BTreeMap")
            && physical_ingress.contains("AtomicPtr<AssignedSpiBinding>")
            && physical_ingress.contains("controller.forward_physical_spi"),
        "hard IRQ routing must publish through a fixed preallocated route into canonical VGIC \
         state"
    );
    assert!(
        !physical_ingress.contains("IrqNotify")
            && !physical_ingress.contains("notify_irq()")
            && !physical_ingress.contains("TaskInner::new")
            && controller.contains("state: SpinNoIrq<ControllerState>")
            && deferred_kick.contains("pending_vcpus: AtomicUsize")
            && deferred_kick.contains("self.notify.notify_irq()"),
        "the physical top half must mutate only IRQ-safe controller state; only the preallocated \
         vCPU kick may be deferred to task context"
    );
    let publish_active = physical_ingress
        .find("*delivery = AssignedSpiDelivery::Active")
        .expect("assigned SPI ingress must publish its active ownership");
    let forward_canonical = physical_ingress
        .find("self.controller.forward_physical_spi(self.irq)")
        .expect("assigned SPI ingress must update canonical VGIC state");
    assert!(
        publish_active < forward_canonical,
        "the route must publish active host-IRQ ownership before canonical forwarding can wake \
         and preempt the current task"
    );
    assert!(
        physical_ingress.contains("delivery: SpinNoIrq<AssignedSpiDelivery>")
            && physical_ingress.contains("AssignedSpiDelivery::Completing")
            && physical_ingress.contains("complete_assigned_spi")
            && gic.contains("physical::complete_assigned_spi(binding.host(), ||"),
        "assigned-SPI completion must keep ingress serialized until host DIR has finished"
    );
    assert!(
        binding.contains("complete_physical_spi(self.vcpu, binding)")
            && physical.contains("deactivate_physical_interrupt(binding.target(), binding)")
            && physical.contains("clear_physical_spi_delivery(spi, binding)")
            && physical.contains("unbind_physical_interrupt(binding)"),
        "guest DIR and stopped-VM teardown must complete host deactivate before releasing \
         ownership"
    );
    assert!(
        vgic.contains("self.core.bind_assigned_spis()")
            && vgic.contains("gic::register_assigned_spi_routes(&self.core)")
            && vgic.contains("self.core.teardown_assigned_spis()"),
        "assigned SPI claims, static routes, and teardown must share one explicit runtime \
         lifecycle"
    );
}

#[test]
fn aarch64_assigned_spi_trigger_and_target_are_applied_and_restored_at_runtime_binding() {
    let vgic = include_str!("../src/arch/aarch64/vgic.rs");
    let gic = include_str!("../src/arch/aarch64/gic.rs");

    assert!(
        !vgic.contains("host_interrupt_trigger(intid)"),
        "prepare must not reject a firmware trigger because the host GIC still has its old setting"
    );
    assert!(
        gic.contains("trigger: Trigger")
            && gic.contains("gic.set_cfg(intid, expected_trigger)")
            && gic.contains("gic.set_cfg(intid, snapshot.trigger)")
            && gic.contains("target: PhysicalSpiTarget")
            && gic.contains("gic.set_target_cpu(intid, target)")
            && gic.contains("match (version, snapshot.target)")
            && gic.matches("gic.set_target_cpu(intid, target)").count() >= 4,
        "runtime binding must apply the firmware trigger and target vCPU affinity, then restore \
         both host settings on release"
    );
}

#[test]
fn aarch64_virtual_devices_share_controller_owned_level_state() {
    let vgic = include_str!("../src/arch/aarch64/vgic.rs");
    let vm = include_str!("../src/arch/aarch64/vm.rs");
    let core = include_str!("../../arm_vgic/src/core.rs");

    assert!(
        vgic.contains("VgicDeviceSet::new(self.runtime.core.clone()")
            && vgic.contains("VirtualInterruptControllerKey")
            && vm.contains("vgic_runtime.core().clone()"),
        "MMIO frontends, typed services, and VM interrupt routing must share one VgicCore"
    );
    assert!(
        core.contains("set_spi_level")
            && core.contains("configure_spi_input")
            && core.contains("WiredIrqInput::new"),
        "virtual devices must update controller-owned level state through stable wired inputs"
    );
}

#[test]
fn aarch64_software_spis_follow_irouter_while_assigned_spis_keep_fixed_affinity() {
    let state = include_str!("../../arm_vgic/src/controller/state.rs");
    let distributor = include_str!("../../arm_vgic/src/distributor.rs");

    assert!(
        state.contains("let (route, cpu_target_mask)")
            && state.contains("redistributor.affinity() == route"),
        "software SPIs must follow the guest-programmed IROUTER affinity"
    );
    assert!(
        distributor.contains("fixed_routes")
            && distributor.contains("if self.fixed_routes[(spi.raw() - 32) as usize]"),
        "an assigned physical SPI must retain the immutable affinity claimed before VM start"
    );
}

#[test]
fn aarch64_passthrough_routes_reject_intids_outside_the_virtual_gic() {
    let config = include_str!("../../arm_vgic/src/arm_config.rs");

    assert!(
        config.contains("AssignedSpiConfig")
            && config.contains("spi_count")
            && config.contains("assigned SPI"),
        "physical IRQ routes must be bounded by the implemented guest distributor capacity"
    );
}

#[test]
fn aarch64_preserves_host_irq_identity_and_virtualizes_the_guest_timer() {
    let architecture = include_str!("../src/arch/aarch64/mod.rs");
    let gic = include_str!("../src/arch/aarch64/gic.rs");
    let vgic = include_str!("../src/arch/aarch64/vgic.rs");
    let timer_state = include_str!("../src/arch/aarch64/vtimer/state.rs");

    assert!(
        gic.contains("identity forwarding requires guest INTID")
            && gic.contains("binding.host().raw()"),
        "assigned devices must preserve host GIC INTID identity"
    );
    assert!(
        vgic.contains("timer_profile.virtual_intid")
            && vgic.contains("timer_profile.nonsecure_physical_intid")
            && timer_state.contains("host_virtual_timer_intid")
            && timer_state.contains("set_ppi_level")
            && !timer_state.contains("register_platform_irq_injector"),
        "architectural timer PPIs must come from the machine profile and remain level lines"
    );
    assert!(
        architecture.contains("let timer_result = self.synchronize_timer();")
            && architecture.contains("let save_result = vgic_backend_result(binding.save());")
            && timer_state.contains("ArmTimerSnapshot")
            && timer_state.contains("ArmTimerKind::Virtual")
            && timer_state.contains("ArmTimerKind::Physical"),
        "vCPU-owned CNTV/CNTP levels must enter canonical VGIC state before VGIC save"
    );
}

#[test]
fn aarch64_host_timer_ppi_completion_follows_guest_retirement() {
    let gic = include_str!("../src/arch/aarch64/gic.rs");
    let timer_state = include_str!("../src/arch/aarch64/vtimer/state.rs");

    assert!(
        gic.contains("fn retire_emulated_interrupt(") && gic.contains(".retire_host_activation()"),
        "the VGIC backend must complete a host timer PPI only after canonical guest retirement"
    );
    assert!(
        !timer_state.contains("if !virtual_level")
            && timer_state.contains("fn retire_host_activation(&self)"),
        "lowering the timer line before guest EOI/DIR must not deactivate its host PPI"
    );
}

#[test]
fn aarch64_traps_guest_wfi_without_losing_an_already_pending_virtual_irq() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let arm_vcpu_root = manifest_dir.join("../arm_vcpu/src");
    let vcpu = std::fs::read_to_string(arm_vcpu_root.join("vcpu.rs"))
        .expect("AArch64 vCPU implementation must be readable");
    let exception = std::fs::read_to_string(arm_vcpu_root.join("exception.rs"))
        .expect("AArch64 exception implementation must be readable");
    let exit_types = std::fs::read_to_string(arm_vcpu_root.join("types.rs"))
        .expect("AArch64 exit types must be readable");
    let architecture = include_str!("../src/arch/aarch64/mod.rs");

    assert!(
        vcpu.contains("HCR_EL2::TWI::SET"),
        "guest WFI must trap to EL2 so a pending virtual IRQ cannot strand the host CPU"
    );
    assert!(
        exception.contains("ESR_EL2::EC::Value::TrappedWFIorWFE")
            && exception.contains("ArmVmExit::WaitForInterrupt"),
        "the AArch64 vCPU core must advance and report a trapped WFI"
    );
    assert!(exit_types.contains("WaitForInterrupt"));
    assert!(
        architecture.contains("vcpu.get_arch_vcpu().arm_timer_wait()?")
            && architecture.contains("waits_for_event: true")
            && architecture.contains("has_pending_interrupt()"),
        "AxVM must arm CNTV wakeup and check canonical VGIC pending state before sleeping"
    );
    assert!(
        architecture.contains("runtime.wait_until") && architecture.contains("if !vm.running()"),
        "WFI waiting must be race-free and remain interruptible by VM lifecycle transitions"
    );
}

#[test]
fn aarch64_guest_timer_state_is_owned_by_each_vcpu() {
    let timer = include_str!("../../arm_vcpu/src/timer.rs");
    let vm = include_str!("../src/arch/aarch64/vm.rs");
    let state = include_str!("../src/arch/aarch64/vtimer/state.rs");

    assert!(timer.contains("pub struct ArmVcpuTimer"));
    assert!(timer.contains("physical_timer: ArmTimerContext"));
    assert!(state.contains("VmTimerHandle"));
    assert!(state.contains("cancel_timer_handle"));
    assert!(!vm.contains("Aarch64VtimerFactory"));
}

#[test]
fn riscv_defers_physical_completion_without_dynamic_irq_callbacks() {
    let architecture = include_str!("../src/arch/riscv64/mod.rs");
    let irq = include_str!("../src/arch/riscv64/irq/mod.rs");
    let vm = include_str!("../src/arch/riscv64/vm.rs");
    let vplic = include_str!("../../riscv_vplic/src/devops_impl.rs");
    let platform = include_str!("../../../platforms/axplat-dyn/src/irq.rs");

    assert!(
        !vm.contains("reject_unsupported_physical_irqs")
            && irq.contains("publish_physical_claim_from_irq")
            && irq.contains("write_register_with_completion"),
        "RISC-V physical PLIC claims must enter the VM through the deferred controller bridge"
    );
    assert!(
        irq.contains(".set_irq_line_level")
            && irq.contains(".set_pending")
            && irq.contains("publish_from_irq")
            && vplic.contains("context_has_deliverable_irq"),
        "vPLIC owns pending/active/enable state while deferred wake carries only a vCPU bit"
    );
    assert!(
        architecture.contains("sync_vplic_vseip(vm, vcpu)")
            && !architecture.contains("register_virtual_irq_injector")
            && !platform.contains("register_virtual_irq_injector")
            && !platform.contains("VIRTUAL_IRQ_INJECTOR"),
        "VSEIP must be derived from the VM-local controller through a fixed typed ingress"
    );
}

#[test]
fn production_sources_keep_architecture_cfg_inside_arch_module() {
    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let violations = find_target_arch_cfg_outside_arch(&source_root, &source_root);

    assert!(
        violations.is_empty(),
        "target_arch must stay inside src/arch; found: {}",
        violations.join(", ")
    );
}

#[test]
fn arch_root_contains_only_architecture_directories_and_dispatch_page() {
    let arch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/arch");
    let mut unexpected_entries = std::fs::read_dir(&arch_root)
        .expect("AxVM architecture directory must be readable")
        .map(|entry| entry.expect("AxVM architecture entry must be readable"))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            !matches!(
                name.as_str(),
                "aarch64" | "loongarch64" | "riscv64" | "x86_64" | "mod.rs"
            )
        })
        .collect::<Vec<_>>();
    unexpected_entries.sort();

    assert!(
        unexpected_entries.is_empty(),
        "arch root must contain only architecture directories and the dispatch page; found: {}",
        unexpected_entries.join(", ")
    );
}

#[test]
fn arch_dispatch_page_does_not_own_common_implementations() {
    let dispatch = include_str!("../src/arch/mod.rs");

    for forbidden in [
        "#[path",
        "trait ArchOps",
        "struct MmioReadExit",
        "fn handle_mmio_read",
        "fn default_vcpu_affinities",
    ] {
        assert!(
            !dispatch.contains(forbidden),
            "arch/mod.rs must only select and export the current architecture: {forbidden}"
        );
    }
}

#[test]
fn riscv_routes_host_software_interrupts_through_the_host_irq_path() {
    let vcpu = include_str!("../../riscv_vcpu/src/vcpu.rs");
    let software_interrupt = vcpu
        .split_once("Trap::Interrupt(Interrupt::SupervisorSoft)")
        .expect("the vCPU must recognize host supervisor software interrupts")
        .1
        .split_once("Trap::")
        .expect("the software-interrupt arm must be bounded")
        .0;

    assert!(
        software_interrupt.contains("RiscvVmExit::ExternalInterrupt")
            && software_interrupt.contains("S_SOFT"),
        "host IPIs must leave the vCPU through the normal host IRQ path"
    );
}

#[test]
fn common_domains_live_outside_architecture_directories() {
    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    for relative_path in [
        "boot/fdt/mod.rs",
        "boot/images/mod.rs",
        "host/arceos.rs",
        "npt.rs",
    ] {
        assert!(
            source_root.join(relative_path).is_file(),
            "common AxVM domain must use its canonical source path: {relative_path}"
        );
    }
}

#[test]
fn vm_domain_uses_a_canonical_directory_module() {
    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    assert!(
        source_root.join("vm/mod.rs").is_file(),
        "the VM domain with child modules must use vm/mod.rs as its directory page"
    );
    assert!(
        !source_root.join("vm.rs").exists(),
        "vm.rs must not coexist with the vm child-module directory"
    );
}

#[test]
fn common_modules_do_not_include_architecture_sources() {
    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    find_source_files(&source_root, &mut |path, source| {
        if !path.starts_with(source_root.join("arch"))
            && source.contains("#[path")
            && source.contains("arch/")
        {
            violations.push(source_relative_path(&source_root, path));
        }
    });

    assert!(
        violations.is_empty(),
        "common modules must not include implementations from src/arch: {}",
        violations.join(", ")
    );
}

#[test]
fn architecture_directories_only_select_their_own_target() {
    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let arch_root = source_root.join("arch");
    let architectures = ["aarch64", "loongarch64", "riscv64", "x86_64"];
    let mut violations = Vec::new();

    for architecture in architectures {
        find_source_files(&arch_root.join(architecture), &mut |path, source| {
            for other_architecture in architectures {
                if other_architecture != architecture
                    && source.contains(&format!("target_arch = \"{other_architecture}\""))
                {
                    violations.push(format!(
                        "{} selects {other_architecture}",
                        source_relative_path(&source_root, path)
                    ));
                }
            }
        });
    }

    assert!(
        violations.is_empty(),
        "an architecture directory must not select another target: {}",
        violations.join(", ")
    );
}

#[test]
fn axvisor_vm_creation_uses_unified_guest_boot_facade() {
    let axvisor_config =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../os/axvisor/src/config.rs");
    let source = std::fs::read_to_string(&axvisor_config)
        .expect("Axvisor VM creation source must be readable");

    for legacy_call in [
        "handle_fdt_operations",
        "ImageLoader::new",
        "x86_linux_direct_boot_config",
        "DEFAULT_X86_BIOS_LOAD_GPA",
    ] {
        assert!(
            !source.contains(legacy_call),
            "Axvisor VM creation must use the unified AxVM boot facade: {legacy_call}"
        );
    }
}

#[test]
fn host_time_trait_only_exposes_common_clock_capabilities() {
    let host_traits = include_str!("../src/host/traits.rs");

    for architecture_specific_detail in ["CancelToken", "fn register_timer"] {
        assert!(
            !host_traits.contains(architecture_specific_detail),
            "HostTime must not expose architecture-specific timer details: \
             {architecture_specific_detail}"
        );
    }
}

#[test]
fn aarch64_host_time_registers_axvm_timer_callback() {
    let aarch64 = include_str!("../src/arch/aarch64/capabilities.rs");
    let common = include_str!("../src/architecture/capabilities.rs");

    assert!(
        aarch64.contains("impl HostTimePlatform for Aarch64Arch {}"),
        "AArch64 must use the common deferred host-timer path"
    );
    assert!(
        common.contains("fn register_timer_callback(notify: Arc<IrqNotify>)")
            && common.contains("ax_task::register_timer_irq_callback")
            && common.contains("notify.notify_irq()"),
        "hard IRQ must publish only to IrqNotify; the pinned worker drains the timer wheel"
    );
}

#[test]
fn vcpu_setup_context_keeps_named_capabilities() {
    let types = include_str!("../src/architecture/types.rs");
    let ops = include_str!("../src/architecture/ops.rs");
    let preparation = include_str!("../src/vm/prepare/vcpus.rs");

    assert!(
        [&types, &ops, &preparation]
            .into_iter()
            .all(|source| !source.contains("VcpuSetupContext")),
        "vCPU setup must pass named configuration and memory sources without a union context"
    );
}

#[test]
fn vm_init_capability_traits_are_not_reintroduced() {
    let capabilities = include_str!("../src/architecture/capabilities.rs");
    let ops = include_str!("../src/architecture/ops.rs");

    for forbidden in [
        "trait DevicePlatform",
        "trait AddressSpacePlatform",
        "VcpuCreateContext",
        "fn build_vcpu_create_config",
        "fn build_vcpu_setup_config",
    ] {
        assert!(
            !capabilities.contains(forbidden) && !ops.contains(forbidden),
            "VM initialization detail must stay behind CurrentArch::init_vm: {forbidden}"
        );
    }
}

#[test]
fn eager_vm_lifecycle_has_no_uninit_state() {
    let status = include_str!("../src/lifecycle/status.rs");
    let machine = include_str!("../src/lifecycle/machine.rs");
    let vm = include_str!("../src/vm/mod.rs");

    assert!(!status.contains("Uninit"));
    assert!(!machine.contains("Machine::Uninit"));
    assert!(vm.contains("machine: Mutex::new(Machine::Ready(resources))"));
}

fn find_target_arch_cfg_outside_arch(
    source_root: &std::path::Path,
    directory: &std::path::Path,
) -> Vec<String> {
    let mut violations = Vec::new();
    for entry in std::fs::read_dir(directory).expect("AxVM source directory must be readable") {
        let entry = entry.expect("AxVM source directory entry must be readable");
        let path = entry.path();
        if path.is_dir() {
            if path != source_root.join("arch") {
                violations.extend(find_target_arch_cfg_outside_arch(source_root, &path));
            }
            continue;
        }

        if path.extension().is_some_and(|extension| extension == "rs")
            && std::fs::read_to_string(&path)
                .expect("AxVM source file must be readable")
                .contains("target_arch")
        {
            violations.push(
                path.strip_prefix(source_root)
                    .expect("source path must be below src")
                    .display()
                    .to_string(),
            );
        }
    }
    violations
}

fn find_source_files(directory: &std::path::Path, visit: &mut impl FnMut(&std::path::Path, &str)) {
    for entry in std::fs::read_dir(directory).expect("AxVM source directory must be readable") {
        let entry = entry.expect("AxVM source directory entry must be readable");
        let path = entry.path();
        if path.is_dir() {
            find_source_files(&path, visit);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = std::fs::read_to_string(&path).expect("AxVM source file must be readable");
            visit(&path, &source);
        }
    }
}

fn source_relative_path(source_root: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(source_root)
        .expect("source path must be below src")
        .display()
        .to_string()
}
