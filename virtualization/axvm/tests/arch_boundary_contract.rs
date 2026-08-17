use std::{env, fs, path::PathBuf};

fn source_path(relative: &str) -> PathBuf {
    env::var_os("AXVM_SOURCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
        .join(relative)
}

fn read_source(relative: &str) -> String {
    let path = source_path(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn assert_omits(source: &str, path: &str, forbidden: &[&str]) {
    for token in forbidden {
        assert!(
            !source.contains(token),
            "{path} must not own architecture-specific IPI protocol token {token:?}"
        );
    }
}

fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    source
        .split_once(start)
        .unwrap_or_else(|| panic!("missing section start {start:?}"))
        .1
        .split_once(end)
        .unwrap_or_else(|| panic!("missing section end {end:?}"))
        .0
}

#[test]
fn riscv_ipi_protocol_stays_out_of_common_architecture_files() {
    let architecture_ops = read_source("src/architecture/ops.rs");
    assert_omits(
        &architecture_ops,
        "src/architecture/ops.rs",
        &[
            "target_arch",
            "hart_mask",
            "ipi_targets",
            "SendIpi",
            "SendIPI",
        ],
    );

    let arch_dispatch = read_source("src/arch/mod.rs");
    assert_omits(
        &arch_dispatch,
        "src/arch/mod.rs",
        &[
            "hart_mask",
            "ipi_targets",
            "deliver_riscv_ipi",
            "SendIpi",
            "SendIPI",
            "#[cfg(any(target_arch = \"riscv64\", test))]",
        ],
    );

    for arch_module in ["src/arch/aarch64/mod.rs", "src/arch/riscv64/mod.rs"] {
        let source = read_source(arch_module);
        assert_omits(
            &source,
            arch_module,
            &["#[path = \"../../architecture/cpu_up.rs\"]"],
        );
    }
}

#[test]
fn aarch64_wfe_completes_spuriously_without_an_inner_scheduler_yield() {
    let source = read_source("src/arch/aarch64/mod.rs");
    let wait_for_event = section(
        &source,
        "ArmVmExit::WaitForEvent => {",
        "ArmVmExit::CpuDown { state } => {",
    );

    assert!(wait_for_event.contains("waits_for_event: false"));
    assert!(!wait_for_event.contains("std::thread::yield_now()"));
}

#[test]
fn aarch64_realtime_idle_polling_is_explicit_and_scoped() {
    let source = read_source("src/arch/aarch64/mod.rs");
    let architecture_exit = read_source("src/architecture/exit.rs");
    let vcpu_runtime = read_source("src/runtime/vcpus.rs");
    let wait_for_interrupt = section(
        &source,
        "ArmVmExit::WaitForInterrupt => {",
        "ArmVmExit::WaitForEvent => {",
    );
    let wait_for_vcpu_event = section(
        &source,
        "    fn wait_for_vcpu_event(",
        "fn inject_current_el_irq(",
    );
    let cargo = read_source("Cargo.toml");

    let suspend_standby = section(
        &architecture_exit,
        "HyperCallOutcome::CpuSuspendStandby { return_value } => {",
        "HyperCallOutcome::CpuOff =>",
    );

    assert!(cargo.contains("rt-poll-idle = []"));
    assert!(cargo.contains("rt-shared-wait-baseline = []"));
    assert!(architecture_exit.contains("fn idle_waits_for_event() -> bool"));
    assert!(architecture_exit.contains("!cfg!(feature = \"rt-poll-idle\")"));
    assert!(!wait_for_interrupt.contains("arm_timer_wait"));
    assert!(wait_for_interrupt.contains("waits_for_event,"));
    assert_eq!(wait_for_vcpu_event.matches("arm_timer_wait()").count(), 1);
    assert!(suspend_standby.contains("waits_for_event: idle_waits_for_event()"));
    assert!(vcpu_runtime.contains("not(feature = \"rt-poll-idle\")"));
    assert!(vcpu_runtime.contains("crate::host::task::yield_now();"));
    assert!(vcpu_runtime.contains("CurrentArch::wait_for_vcpu_event(&vm, &vcpu, &runtime);"));
}

#[test]
fn aarch64_cpu_on_runs_only_after_the_calling_vcpu_is_unbound() {
    let source = read_source("src/arch/aarch64/mod.rs");
    let bound_cpu_up = section(
        &source,
        "            ArmVmExit::CpuUp {",
        "            ArmVmExit::SystemDown => {",
    );
    let deferred_work = section(
        &source,
        "    fn finish_deferred_run_work(",
        "    fn wait_for_vcpu_event(",
    );

    assert!(bound_cpu_up.contains("Aarch64DeferredRunWork::CpuUp"));
    assert!(!bound_cpu_up.contains("cpu_up::handle::<Self>"));
    assert!(deferred_work.contains("cpu_up::handle::<Self>"));
}

#[test]
fn runtime_waits_and_wakes_the_target_vcpu() {
    let vm = read_source("src/vm/mod.rs");
    let runtime = read_source("src/runtime/vcpus.rs");
    let runtime_api = read_source("src/runtime/mod.rs");

    assert!(!vm.contains("vcpu_wait_queues:"));
    assert!(vm.contains("pub(crate) fn wait_vcpu_until"));
    assert!(vm.contains("pub(crate) fn notify_vcpu"));
    assert!(vm.contains("feature = \"rt-shared-wait-baseline\""));
    let targeted_wait = section(
        &vm,
        "    pub(crate) fn wait_vcpu_until(&self, vcpu_id: usize, condition: impl Fn() -> bool) {",
        "    #[cfg(any(target_arch = \"aarch64\", test))]",
    );
    assert!(targeted_wait.contains("self.wait_queue.wait_until(condition);"));
    assert!(runtime.contains("runtime.notify_vcpu(vcpu_id);"));
    assert!(runtime.contains("send_ipi(cpu_id);"));
    assert!(runtime_api.contains("crate::host::task::send_ipi(cpu_id);"));
    assert!(runtime_api.contains("not(feature = \"rt-poll-idle\")"));
}

#[test]
fn polling_idle_profile_excludes_blocking_vcpu_wait_helpers() {
    let vm = read_source("src/vm/mod.rs");
    let aarch64 = read_source("src/arch/aarch64/mod.rs");
    let runtime = read_source("src/runtime/vcpus.rs");

    assert!(vm.contains("#[cfg(not(feature = \"rt-poll-idle\"))]\n    pub(crate) fn wait_vcpu"));
    assert!(
        vm.contains("#[cfg(not(feature = \"rt-poll-idle\"))]\n    pub(crate) fn wait_vcpu_until")
    );
    assert!(
        aarch64.contains("#[cfg(not(feature = \"rt-poll-idle\"))]\n    fn wait_for_vcpu_event")
    );
    assert!(runtime.contains("vm_vcpus.wait_until(condition);"));
}

#[test]
fn target_vcpu_wake_requests_reschedule_when_the_notifier_runs_locally() {
    let vm = read_source("src/vm/mod.rs");
    let targeted_wake = section(
        &vm,
        "    pub(crate) fn notify_vcpu(&self, vcpu_id: usize) {",
        "    pub(crate) fn notify_all(&self) {",
    );

    assert!(targeted_wake.contains("self.wait_queue.notify_all(true);"));
    assert!(targeted_wake.contains("self.wait_queue.notify_all(false);"));
}

#[test]
fn cpu_on_startup_uses_the_lifecycle_wait_queue() {
    let runtime = read_source("src/runtime/vcpus.rs");
    let startup = section(
        &runtime,
        "let cpu_on_start_ack = runtime.cpu_on_start_ack(vcpu_id);",
        "if let Some(ack) = &cpu_on_start_ack {",
    );

    assert!(startup.contains("runtime.wait_until(start_is_ready);"));
    assert!(!startup.contains("wait_for(&runtime, vcpu_id, start_is_ready);"));
}

#[test]
fn aarch64_assigned_spi_keeps_latest_dev_hardware_backing() {
    let physical = read_source("src/arch/aarch64/gic/physical.rs");
    let runtime = read_source("src/arch/aarch64/vgic/mod.rs");
    let backend = read_source("src/arch/aarch64/gic.rs");

    assert!(physical.contains("self.controller.forward_physical_spi(self.irq)"));
    assert!(!physical.contains("irq::request_irq"));
    assert!(runtime.contains("self.core.bind_assigned_spis()"));
    assert!(backend.contains("physical_spi_target(self.capabilities.host_version(), binding)"));
}

#[test]
fn aarch64_current_el_irq_can_transfer_physical_ownership_to_the_guest() {
    let platform = fs::read_to_string(source_path("../../platforms/axplat-dyn/src/irq.rs"))
        .expect("read axplat-dyn IRQ adapter");
    let somehal = fs::read_to_string(source_path(
        "../../platforms/somehal/src/arch/aarch64/gic/v3.rs",
    ))
    .expect("read somehal GICv3 adapter");
    let physical = read_source("src/arch/aarch64/gic/physical.rs");

    assert!(platform.contains("register_aarch64_virtual_irq_injector"));
    assert!(platform.contains("GuestIrqInjection::HardwareForwarded"));
    assert!(platform.contains("active.forward_to_guest()"));
    assert!(somehal.contains("deactivate_on_drop: bool"));
    assert!(physical.contains("publish_from_current_el"));
}
