// Copyright 2025 The Axvisor Team
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

#[cfg(feature = "rt-trace")]
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use aarch64_cpu::registers::*;
use ax_errno::AxResult;
use axvm_types::{GuestPhysAddr, NestedPagingConfig, SysRegAddr, VmArchVcpuOps, VmExit};

use crate::{
    TrapFrame,
    context_frame::GuestSystemRegisters,
    exception::{TrapKind, handle_exception_sync},
    exception_utils::exception_class_value,
};

#[ax_percpu::def_percpu]
static HOST_SP_EL0: u64 = 0;

#[ax_percpu::def_percpu]
static HOST_TPIDR_EL0: u64 = 0;

#[ax_percpu::def_percpu]
static HOST_TPIDRRO_EL0: u64 = 0;

#[cfg(feature = "rt-trace")]
static SHARED_REGISTER_CONTEXT_REPORTED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "rt-trace")]
static GUEST_ENTRY_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "rt-trace")]
static GUEST_IRQ_EXIT_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "rt-trace")]
static GUEST_FIQ_EXIT_COUNT: AtomicU64 = AtomicU64::new(0);

const fn trap_guest_wfi(passthrough_timer: bool) -> bool {
    // A passthrough timer must be allowed to wake a sleeping guest through its
    // physical PPI. Trapping WFI in that mode creates a tight EL1/EL2 loop that
    // can starve the deadline before the timer ever becomes pending.
    !passthrough_timer
}

/// Save host registers that are shared with the guest.
unsafe fn save_host_shared_registers() {
    unsafe {
        #[cfg(feature = "rt-trace")]
        {
            let entry_count = GUEST_ENTRY_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            let host_tpidr_el0 = TPIDR_EL0.get();
            if host_tpidr_el0 == 0 {
                error!(
                    "AArch64 guest entry has no host TLS context: entry_count={entry_count} \
                     sp_el0={:#x} tpidrro_el0={:#x}",
                    SP_EL0.get(),
                    TPIDRRO_EL0.get(),
                );
            }
        }
        HOST_SP_EL0.write_current_raw(SP_EL0.get());
        HOST_TPIDR_EL0.write_current_raw(TPIDR_EL0.get());
        HOST_TPIDRRO_EL0.write_current_raw(TPIDRRO_EL0.get());
    }
}

/// Restore host registers after the guest values have been saved.
unsafe fn restore_host_shared_registers() {
    unsafe {
        SP_EL0.set(HOST_SP_EL0.read_current_raw());
        TPIDR_EL0.set(HOST_TPIDR_EL0.read_current_raw());
        TPIDRRO_EL0.set(HOST_TPIDRRO_EL0.read_current_raw());
    }
}

/// (v)CPU register state that must be saved or restored when entering/exiting a VM or switching
/// between VMs.
#[repr(C)]
#[derive(Clone, Debug, Copy, Default)]
#[allow(dead_code)]
pub struct VmCpuRegisters {
    /// guest trap context
    pub trap_context_regs: TrapFrame,
    /// virtual machine system regs setting
    pub vm_system_regs: GuestSystemRegisters,
}

/// A virtual CPU within a guest
#[repr(C)]
#[derive(Debug)]
pub struct Aarch64VCpu {
    // DO NOT modify `guest_regs` and `host_stack_top` and their order unless you do know what you are doing!
    // DO NOT add anything before or between them unless you do know what you are doing!
    ctx: TrapFrame,
    host_stack_top: u64,
    guest_system_regs: GuestSystemRegisters,
    /// The MPIDR_EL1 value for the vCPU.
    mpidr: u64,
    /// Whether physical timer interrupts are forwarded through a hardware LR.
    passthrough_timer: bool,
}

/// Configuration for creating a new `Aarch64VCpu`
#[derive(Clone, Debug, Default)]
pub struct Aarch64VCpuCreateConfig {
    /// The MPIDR_EL1 value for the new vCPU,
    /// which is used to identify the CPU in a multiprocessor system.
    /// Note: mind CPU cluster.
    // FIXME: Handle its interaction with the virtual GIC.
    pub mpidr_el1: u64,
    /// The address of the device tree blob.
    pub dtb_addr: usize,
}

/// Configuration for setting up a new `Aarch64VCpu`
#[derive(Clone, Debug, Default)]
pub struct Aarch64VCpuSetupConfig {
    /// Should the hypervisor passthrough interrupts to the guest?
    pub passthrough_interrupt: bool,
    /// Should the hypervisor passthrough timers to the guest?
    pub passthrough_timer: bool,
}

impl axvm_types::VmArchVcpuOps for Aarch64VCpu {
    type CreateConfig = Aarch64VCpuCreateConfig;

    type SetupConfig = Aarch64VCpuSetupConfig;

    fn new(_vm_id: usize, _vcpu_id: usize, config: Self::CreateConfig) -> AxResult<Self> {
        let mut ctx = TrapFrame::default();
        ctx.set_argument(config.dtb_addr);

        Ok(Self {
            ctx,
            host_stack_top: 0,
            guest_system_regs: GuestSystemRegisters::default(),
            mpidr: config.mpidr_el1,
            passthrough_timer: false,
        })
    }

    fn setup(&mut self, config: Self::SetupConfig) -> AxResult {
        self.passthrough_timer = config.passthrough_timer;
        self.init_hv(config);
        Ok(())
    }

    fn set_entry(&mut self, entry: GuestPhysAddr) -> AxResult {
        debug!("set vcpu entry:{entry:?}");
        self.set_elr(entry.as_usize());
        Ok(())
    }

    fn set_nested_page_table(&mut self, config: NestedPagingConfig) -> AxResult {
        debug!("set vcpu stage-2 root:{:#x}", config.root_paddr);
        self.guest_system_regs.vttbr_el2 = config.root_paddr.as_usize() as u64;
        let pa_bits = if config.mode == 0 {
            pa_bits()
        } else {
            config.mode
        };
        self.guest_system_regs.vtcr_el2 = vtcr_for_config(config.levels, config.gpa_bits, pa_bits);
        Ok(())
    }

    fn run(&mut self) -> AxResult<VmExit> {
        unsafe {
            core::arch::asm!("msr daifset, #2");
        }

        // Run the guest with host IRQs masked across the complete register
        // ownership transition. A lower-EL exit returns while TPIDR_EL0,
        // TPIDRRO_EL0, and SP_EL0 still contain guest values, so no host IRQ or
        // Rust code that may use TLS can run until those values are captured and
        // the host copies are restored.
        let exit_reason = unsafe {
            // Save host registers that are visible to EL1/EL0 before restoring
            // the guest's copies of those registers.
            save_host_shared_registers();
            self.restore_vm_system_regs();
            let exit_reason = self.run_guest();
            self.capture_guest_and_restore_host_shared_registers();
            exit_reason
        };

        let trap_kind = TrapKind::try_from(exit_reason as u8).expect("Invalid TrapKind");
        #[cfg(feature = "rt-trace")]
        {
            let interrupt_exit_count = match trap_kind {
                TrapKind::Irq => Some(GUEST_IRQ_EXIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1),
                TrapKind::Fiq => Some(GUEST_FIQ_EXIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1),
                _ => None,
            };
            if let Some(count) = interrupt_exit_count
                && (count <= 8 || count.is_power_of_two())
            {
                info!(
                    "AArch64 guest interrupt exit count={count} kind={trap_kind:?} mpidr={:#x} \
                     pc={:#x}",
                    self.mpidr,
                    self.ctx.exception_pc()
                );
            }
        }

        // A physical timer IRQ must remain asserted until fetch_irq()/fetch_fiq()
        // acknowledges it and installs the corresponding virtual interrupt. Other
        // exits can quiesce the guest timers before running host-side handlers.
        let interrupt_exit = matches!(trap_kind, TrapKind::Irq | TrapKind::Fiq);
        if !interrupt_exit {
            unsafe { GuestSystemRegisters::quiesce_timers() };
        }
        let result = self.vmexit_handler(trap_kind);
        if interrupt_exit {
            unsafe { GuestSystemRegisters::quiesce_timers() };
        }

        unsafe {
            core::arch::asm!("msr daifclr, #2");
        }

        result
    }

    fn bind(&mut self) -> AxResult {
        Ok(())
    }

    fn unbind(&mut self) -> AxResult {
        Ok(())
    }

    fn set_gpr(&mut self, idx: usize, val: usize) {
        self.ctx.set_gpr(idx, val);
    }

    fn inject_interrupt(&mut self, vector: usize) -> AxResult {
        crate::host::hardware_inject_virtual_interrupt(vector as u8);
        Ok(())
    }

    fn set_return_value(&mut self, val: usize) {
        // Return value is stored in x0.
        self.ctx.set_argument(val);
    }
}

// Private function
impl Aarch64VCpu {
    fn init_hv(&mut self, config: Aarch64VCpuSetupConfig) {
        self.ctx.spsr = (SPSR_EL1::M::EL1h
            + SPSR_EL1::I::Masked
            + SPSR_EL1::F::Masked
            + SPSR_EL1::A::Masked
            + SPSR_EL1::D::Masked)
            .value;
        self.init_vm_context(config);
    }

    /// Init guest context. Also set some el2 register value.
    fn init_vm_context(&mut self, config: Aarch64VCpuSetupConfig) {
        // CNTHCTL_EL2.modify(CNTHCTL_EL2::EL1PCEN::SET + CNTHCTL_EL2::EL1PCTEN::SET);
        // Set CNTVOFF_EL2 to the current physical counter so the guest's
        // virtual counter (CNTVCT_EL0 = CNTPCT_EL0 - CNTVOFF_EL2) starts near zero.
        let cntpct: u64;
        unsafe { core::arch::asm!("mrs {0}, CNTPCT_EL0", out(reg) cntpct) };
        self.guest_system_regs.cntvoff_el2 = cntpct;
        self.guest_system_regs.cntkctl_el1 = 0;
        self.guest_system_regs.cnthctl_el2 = if config.passthrough_timer {
            (CNTHCTL_EL2::EL1PCEN::SET + CNTHCTL_EL2::EL1PCTEN::SET).into()
        } else {
            (CNTHCTL_EL2::EL1PCEN::CLEAR + CNTHCTL_EL2::EL1PCTEN::CLEAR).into()
        };

        self.guest_system_regs.sctlr_el1 = 0x30C50830;
        self.guest_system_regs.pmcr_el0 = 0;

        if self.guest_system_regs.vtcr_el2 == 0 {
            let pa_bits = pa_bits();
            let levels = max_gpt_level(pa_bits);
            let gpa_bits = if levels == 3 { 39 } else { 48 };
            self.guest_system_regs.vtcr_el2 = vtcr_for_config(levels, gpa_bits, pa_bits);
        }

        let mut hcr_el2 =
            HCR_EL2::VM::Enable + HCR_EL2::TSC::EnableTrapEl1SmcToEl2 + HCR_EL2::RW::EL1IsAarch64;

        if trap_guest_wfi(config.passthrough_timer) {
            hcr_el2 += HCR_EL2::TWI::SET;
        }

        // Route physical IRQs to EL2 for ownership arbitration. The host IRQ
        // registry consumes host-owned lines first; only unclaimed lines may
        // be forwarded to a passthrough VM by the runtime. This keeps the
        // policy independent of guest type and physical interrupt numbers.
        hcr_el2 += HCR_EL2::IMO::EnableVirtualIRQ;

        // Keep Group 0 FIQs routed to EL2 for platforms whose firmware already
        // uses that interrupt group. Group 0 is not used to encode ownership.
        hcr_el2 += HCR_EL2::FMO::EnableVirtualFIQ;

        self.guest_system_regs.hcr_el2 = hcr_el2.into();

        // Set VMPIDR_EL2, which provides the value of the Virtualization Multiprocessor ID.
        // This is the value returned by Non-secure EL1 reads of MPIDR.
        let mut vmpidr = 1 << 31;
        // Note: mind CPU cluster here.
        vmpidr |= self.mpidr;
        self.guest_system_regs.vmpidr_el2 = vmpidr;
    }

    /// Set exception return pc
    fn set_elr(&mut self, elr: usize) {
        self.ctx.set_exception_pc(elr);
    }

    /// Get general purpose register
    #[allow(unused)]
    fn get_gpr(&self, idx: usize) {
        self.ctx.gpr(idx);
    }
}

/// Private functions related to vcpu runtime control flow.
impl Aarch64VCpu {
    /// Save host context and run guest.
    ///
    /// When a VM-Exit happens when guest's vCpu is running,
    /// the control flow will be redirected to this function through `return_run_guest`.
    #[unsafe(naked)]
    unsafe extern "C" fn run_guest(&mut self) -> usize {
        // Fixes: https://github.com/arceos-hypervisor/arm_vcpu/issues/22
        //
        // The original issue seems to be caused by an unexpected compiler optimization that takes
        // the dummy return value `0` of `run_guest` as the actual return value. By replacing the
        // original `run_guest` with the current naked one, we eliminate the dummy code path of the
        // original version, and ensure that the compiler does not perform any unexpected return
        // value optimization.
        core::arch::naked_asm!(
            // Save host context.
            save_regs_to_stack!(),
            // Save current host stack top to `self.host_stack_top`.
            //
            // 'extern "C"' here specifies the aapcs64 calling convention, according to which
            // the first and only parameter, the pointer of self, should be in x0:
            "mov x9, sp",
            "add x0, x0, {host_stack_top_offset}",
            "str x9, [x0]",
            // Go to `context_vm_entry`.
            "b context_vm_entry",
            // Panic if the control flow comes back here, which should never happen.
            "b {run_guest_panic}",
            host_stack_top_offset = const core::mem::size_of::<TrapFrame>(),
            run_guest_panic = sym Self::run_guest_panic,
        );
    }

    /// This function is called when the control flow comes back to `run_guest`. To provide a error
    /// message for debugging purposes.
    ///
    /// This function may fail as the stack may have been corrupted when this function is called.
    /// But we won't handle it here for now.
    unsafe fn run_guest_panic() -> ! {
        panic!("run_guest_panic");
    }

    /// Restores guest system control registers.
    unsafe fn restore_vm_system_regs(&mut self) {
        unsafe {
            // load system regs
            core::arch::asm!(
                "
                mov x3, xzr           // Trap nothing from EL1 to El2.
                msr cptr_el2, x3"
            );
            self.guest_system_regs.restore();
            core::arch::asm!(
                "
                ic  iallu
                tlbi	alle2
                tlbi	alle1         // Flush tlb
                dsb	nsh
                isb"
            );
        }
    }

    /// Captures guest-owned shared registers and restores their host-owned
    /// counterparts before host IRQs or TLS-using Rust code can run.
    unsafe fn capture_guest_and_restore_host_shared_registers(&mut self) {
        unsafe {
            let guest_sp_el0 = SP_EL0.get();
            let guest_tpidr_el0 = TPIDR_EL0.get();
            let guest_tpidrro_el0 = TPIDRRO_EL0.get();

            // Restore the host-owned physical registers before collecting the
            // remaining guest state. If a later system-register access faults,
            // the host exception and panic paths must already have valid host
            // stack and TLS register values.
            restore_host_shared_registers();
            self.guest_system_regs.store();
            self.guest_system_regs.set_shared_el0_registers(
                guest_sp_el0,
                guest_tpidr_el0,
                guest_tpidrro_el0,
            );
            self.ctx.sp_el0 = guest_sp_el0;

            #[cfg(feature = "rt-trace")]
            if !SHARED_REGISTER_CONTEXT_REPORTED.swap(true, Ordering::Relaxed) {
                warn!(
                    "AArch64 shared-register context: host_saved_tpidr_el0={:#x} \
                     guest_tpidr_el0={:#x} host_restored_tpidr_el0={:#x} \
                     host_saved_tpidrro_el0={:#x} guest_tpidrro_el0={:#x} \
                     host_restored_tpidrro_el0={:#x}",
                    HOST_TPIDR_EL0.read_current_raw(),
                    guest_tpidr_el0,
                    TPIDR_EL0.get(),
                    HOST_TPIDRRO_EL0.read_current_raw(),
                    guest_tpidrro_el0,
                    TPIDRRO_EL0.get(),
                );
            }
        }
    }

    /// Handle VM-Exits.
    ///
    /// Parameters:
    /// - `exit_reason`: The reason why the VM-Exit happened in [`TrapKind`].
    ///
    /// Returns:
    /// - [`VmExit`]: a wrappered VM-Exit reason needed to be handled by the hypervisor.
    ///
    /// This function may panic for unhandled exceptions.
    fn vmexit_handler(&mut self, exit_reason: TrapKind) -> AxResult<VmExit> {
        trace!(
            "Aarch64VCpu vmexit_handler() esr:{:#x} ctx:{:#x?}",
            exception_class_value(),
            self.ctx
        );

        let result = match exit_reason {
            TrapKind::Synchronous => handle_exception_sync(&mut self.ctx),
            TrapKind::Irq => Ok(VmExit::ExternalInterrupt {
                vector: crate::host::fetch_irq() as u64,
            }),
            TrapKind::Fiq => Ok(VmExit::ExternalInterrupt {
                vector: crate::host::fetch_fiq() as u64,
            }),
            _ => panic!("Unhandled exception {:?}", exit_reason),
        };

        match result {
            Ok(VmExit::SysRegRead { addr, reg }) => {
                if let Some(exit_reason) =
                    self.builtin_sysreg_access_handler(addr, false, 0, reg)?
                {
                    return Ok(exit_reason);
                }

                result
            }
            Ok(VmExit::SysRegWrite { addr, value }) => {
                if let Some(exit_reason) =
                    self.builtin_sysreg_access_handler(addr, true, value, 0)?
                {
                    return Ok(exit_reason);
                }

                result
            }
            r => r,
        }
    }

    /// Handle system register access that can and should be handled by the VCpu itself.
    ///
    /// Return `Ok(None)` if the system register access is not handled by the VCpu itself,
    fn builtin_sysreg_access_handler(
        &mut self,
        addr: SysRegAddr,
        write: bool,
        value: u64,
        reg: usize,
    ) -> AxResult<Option<VmExit>> {
        const SYSREG_ICC_SGI1R_EL1: SysRegAddr = SysRegAddr::new(0x3A_3016); // ICC_SGI1R_EL1

        match (addr, write) {
            (SYSREG_ICC_SGI1R_EL1, true) => {
                debug!("arm_vcpu ICC_SGI1R_EL1 write: {value:#x}");

                // TODO: support RangeSelector

                let intid = (value >> 24) & 0b1111;
                let irm = ((value >> 40) & 0b1) != 0;

                // IRM == 1 => send to all except self
                if irm {
                    debug!("arm_vcpu ICC_SGI1R_EL1 write: irm == 1, send to all except self");

                    return Ok(Some(VmExit::SendIPI {
                        target_cpu: 0,
                        target_cpu_aux: 0,
                        send_to_all: true,
                        send_to_self: false,
                        vector: intid,
                    }));
                }

                let aff3 = (value >> 48) & 0xff;
                let aff2 = (value >> 32) & 0xff;
                let aff1 = (value >> 16) & 0xff;
                let target_list = value & 0xffff;

                debug!(
                    "arm_vcpu ICC_SGI1R_EL1 write: aff3:{aff3:#x} aff2:{aff2:#x} aff1:{aff1:#x} \
                     intid:{intid:#x} target_list:{target_list:#x}"
                );

                Ok(Some(VmExit::SendIPI {
                    target_cpu: (aff3 << 24) | (aff2 << 16) | (aff1 << 8),
                    target_cpu_aux: target_list,
                    send_to_all: false,
                    send_to_self: false,
                    vector: intid,
                }))
            }
            (SYSREG_ICC_SGI1R_EL1, false) => {
                // ICC_SGI1R_EL1 is WO, we take it as RAZ.
                self.set_gpr(reg, 0);
                Ok(Some(VmExit::Nothing))
            }
            _ => {
                // If the system register access is not handled by the VCpu itself,
                // we return None to let the hypervisor handle it.
                Ok(None)
            }
        }
    }
}

pub(crate) fn pa_bits() -> usize {
    match ID_AA64MMFR0_EL1.read_as_enum(ID_AA64MMFR0_EL1::PARange) {
        Some(ID_AA64MMFR0_EL1::PARange::Value::Bits_32) => 32,
        Some(ID_AA64MMFR0_EL1::PARange::Value::Bits_36) => 36,
        Some(ID_AA64MMFR0_EL1::PARange::Value::Bits_40) => 40,
        Some(ID_AA64MMFR0_EL1::PARange::Value::Bits_42) => 42,
        Some(ID_AA64MMFR0_EL1::PARange::Value::Bits_44) => 44,
        Some(ID_AA64MMFR0_EL1::PARange::Value::Bits_48) => 48,
        Some(ID_AA64MMFR0_EL1::PARange::Value::Bits_52) => 52,
        _ => 32,
    }
}

#[allow(dead_code)]
pub(crate) fn current_gpt_level() -> usize {
    let t0sz = VTCR_EL2.read(VTCR_EL2::T0SZ) as usize;
    match t0sz {
        16..=25 => 4,
        26..=35 => 3,
        _ => 2,
    }
}

pub(crate) fn max_gpt_level(pa_bits: usize) -> usize {
    match pa_bits {
        44.. => 4,
        _ => 3,
    }
}

fn vtcr_for_config(levels: usize, gpa_bits: usize, pa_bits: usize) -> u64 {
    let mut val = match levels {
        4 => VTCR_EL2::SL0::Granule4KBLevel0 + VTCR_EL2::T0SZ.val((64 - gpa_bits) as u64),
        _ => VTCR_EL2::SL0::Granule4KBLevel1 + VTCR_EL2::T0SZ.val((64 - gpa_bits) as u64),
    };

    match pa_bits {
        52..=64 => val += VTCR_EL2::PS::PA_52B_4PB,
        48..=51 => val += VTCR_EL2::PS::PA_48B_256TB,
        44..=47 => val += VTCR_EL2::PS::PA_44B_16TB,
        42..=43 => val += VTCR_EL2::PS::PA_42B_4TB,
        40..=41 => val += VTCR_EL2::PS::PA_40B_1TB,
        36..=39 => val += VTCR_EL2::PS::PA_36B_64GB,
        _ => val += VTCR_EL2::PS::PA_32B_4GB,
    }

    val += VTCR_EL2::TG0::Granule4KB
        + VTCR_EL2::SH0::Inner
        + VTCR_EL2::ORGN0::NormalWBRAWA
        + VTCR_EL2::IRGN0::NormalWBRAWA;

    val.value
}

#[cfg(test)]
mod tests {
    use super::trap_guest_wfi;

    #[test]
    fn passthrough_timer_uses_guest_wfi_for_hardware_wakeup() {
        assert!(!trap_guest_wfi(true));
        assert!(trap_guest_wfi(false));
    }
}
