use alloc::string::String;

#[cfg(any(
    target_arch = "loongarch64",
    target_arch = "riscv64",
    target_arch = "x86_64",
))]
use ax_hal::irq::CPU_LOCAL_IRQ_DOMAIN;
#[cfg(any(
    target_arch = "aarch64",
    target_arch = "loongarch64",
    target_arch = "riscv64",
    target_arch = "x86_64",
))]
use ax_hal::irq::HwIrq;
use ax_hal::irq::{IrqContext, IrqError, IrqHandle, IrqId, IrqReturn, IrqSource};

/// Resolves an explicitly legacy numeric IRQ without truncating it.
pub fn resolve_legacy_irq(irq: usize) -> Result<IrqId, IrqError> {
    ax_hal::irq::try_legacy_irq(irq)
}

/// Resolves a discovered device IRQ binding through the platform IRQ domain.
pub fn resolve_binding_irq(irq: ax_driver::BindingIrq) -> Result<IrqId, IrqError> {
    if let Some(id) = irq.irq_id() {
        return Ok(id);
    }

    match irq {
        ax_driver::BindingIrq::Id(id) => Ok(id),
        ax_driver::BindingIrq::Source(source) => resolve_binding_irq_source(source),
    }
}

fn resolve_binding_irq_source(source: ax_driver::BindingIrqSource) -> Result<IrqId, IrqError> {
    match source {
        ax_driver::BindingIrqSource::AcpiGsi(gsi) => {
            ax_hal::irq::resolve_irq_source(IrqSource::AcpiGsi(gsi))
        }
        ax_driver::BindingIrqSource::AcpiGsiRoute(route) => {
            ax_hal::irq::resolve_irq_source(IrqSource::AcpiGsiRoute(route))
        }
        ax_driver::BindingIrqSource::FdtInterrupt(spec) => resolve_fdt_irq_spec(spec),
    }
}

#[cfg(target_arch = "aarch64")]
fn resolve_fdt_irq_spec(spec: ax_driver::FdtIrqSpec) -> Result<IrqId, IrqError> {
    if let Ok(intc) = rdrive::get::<rdif_intc::Intc>(spec.controller) {
        let mut intc = intc.lock().map_err(|_| IrqError::Controller)?;
        let translation = intc.translate_fdt(&spec.cells)?;
        intc.configure(&translation)?;
        return Ok(translation.id);
    }

    // The AArch64 machine profile owns the guest GIC, so it may initialize the
    // platform IRQ domain without publishing a duplicate rdrive controller.
    // PCI INTx bindings still carry the standard three-cell GIC encoding.
    let (hwirq, trigger) = decode_aarch64_gic_fdt(&spec.cells).inspect_err(|error| {
        warn!(
            "failed to decode machine-owned AArch64 GIC FDT binding controller={:?} cells={:?}: \
             {error:?}",
            spec.controller, spec.cells
        );
    })?;
    let id = ax_hal::irq::resolve_irq_source(IrqSource::ControllerLine {
        domain: ax_hal::irq::AARCH64_GIC_DOMAIN,
        hwirq,
    })
    .inspect_err(|error| {
        warn!("failed to resolve machine-owned AArch64 GIC line {hwirq:?}: {error:?}");
    })?;
    ax_hal::irq::set_trigger(id, trigger).inspect_err(|error| {
        warn!("failed to configure machine-owned AArch64 GIC line {id:?}: {error:?}");
    })?;
    Ok(id)
}

#[cfg(target_arch = "aarch64")]
fn decode_aarch64_gic_fdt(cells: &[u32]) -> Result<(HwIrq, ax_hal::irq::IrqTrigger), IrqError> {
    let [irq_type, irq_num, flags] = cells else {
        return Err(IrqError::InvalidIrq);
    };
    let hwirq = match irq_type {
        0 => irq_num.checked_add(32),
        1 => irq_num.checked_add(16),
        2 => irq_num.checked_add(4096),
        3 => irq_num.checked_add(1056),
        4 => Some(*irq_num),
        _ => None,
    }
    .ok_or(IrqError::InvalidIrq)?;
    let trigger = match flags & 0xf {
        1..=3 => ax_hal::irq::IrqTrigger::Edge,
        4 | 8 => ax_hal::irq::IrqTrigger::Level,
        _ => return Err(IrqError::InvalidIrq),
    };
    Ok((HwIrq(hwirq), trigger))
}

#[cfg(any(target_arch = "loongarch64", target_arch = "riscv64"))]
fn resolve_fdt_irq_spec(spec: ax_driver::FdtIrqSpec) -> Result<IrqId, IrqError> {
    let mut intc = rdrive::get::<rdif_intc::Intc>(spec.controller)
        .map_err(|_| IrqError::Unsupported)?
        .lock()
        .map_err(|_| IrqError::Controller)?;
    let translation = intc.translate_fdt(&spec.cells)?;
    intc.configure(&translation)?;
    Ok(translation.id)
}

#[cfg(not(any(
    target_arch = "aarch64",
    target_arch = "loongarch64",
    target_arch = "riscv64"
)))]
fn resolve_fdt_irq_spec(_spec: ax_driver::FdtIrqSpec) -> Result<IrqId, IrqError> {
    Err(IrqError::Unsupported)
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;

    #[test]
    fn decodes_gic_spi_and_trigger_from_fdt_cells() {
        assert_eq!(
            decode_aarch64_gic_fdt(&[0, 3, 4]),
            Ok((HwIrq(35), ax_hal::irq::IrqTrigger::Level))
        );
        assert_eq!(
            decode_aarch64_gic_fdt(&[0, 46, 1]),
            Ok((HwIrq(78), ax_hal::irq::IrqTrigger::Edge))
        );
    }

    #[test]
    fn rejects_non_gic_or_incomplete_fdt_cells() {
        assert_eq!(decode_aarch64_gic_fdt(&[3, 4]), Err(IrqError::InvalidIrq));
        assert_eq!(
            decode_aarch64_gic_fdt(&[7, 0, 4]),
            Err(IrqError::InvalidIrq)
        );
    }
}

/// Resolves a per-CPU trap IRQ through the platform IRQ domain.
#[cfg(target_arch = "aarch64")]
pub fn resolve_percpu_irq(irq: usize) -> IrqId {
    let hwirq = HwIrq(u32::try_from(irq).expect("AArch64 per-CPU IRQ exceeds GIC INTID width"));
    ax_hal::irq::resolve_percpu_irq(hwirq).expect("AArch64 per-CPU IRQ domain is not registered")
}

/// Resolves a per-CPU trap IRQ through the platform IRQ domain.
#[cfg(any(target_arch = "loongarch64", target_arch = "x86_64"))]
pub fn resolve_percpu_irq(irq: usize) -> IrqId {
    IrqId::new(CPU_LOCAL_IRQ_DOMAIN, HwIrq(irq as u32))
}

/// Resolves a per-CPU trap IRQ through the platform IRQ domain.
#[cfg(not(any(
    target_arch = "aarch64",
    target_arch = "loongarch64",
    target_arch = "x86_64"
)))]
pub fn resolve_percpu_irq(irq: usize) -> IrqId {
    #[cfg(target_arch = "riscv64")]
    {
        const RISCV_INTERRUPT_BIT: usize = 1usize << (usize::BITS as usize - 1);

        if irq & RISCV_INTERRUPT_BIT != 0 {
            return IrqId::new(
                CPU_LOCAL_IRQ_DOMAIN,
                HwIrq((irq & !RISCV_INTERRUPT_BIT) as u32),
            );
        }
    }

    resolve_legacy_irq(irq).expect("legacy per-CPU IRQ exceeds platform IRQ id width")
}

pub struct Registration {
    name: String,
    handle: Option<IrqHandle>,
}

impl Registration {
    pub fn register_shared(
        name: impl Into<String>,
        irq: IrqId,
        handler: impl FnMut(IrqContext) -> IrqReturn + Send + 'static,
    ) -> Result<Self, IrqError> {
        let name = name.into();
        match ax_hal::irq::request_shared_irq(irq, handler) {
            Ok(handle) => {
                info!("registered {name} irq {:?}", handle.irq());
                Ok(Self {
                    name,
                    handle: Some(handle),
                })
            }
            Err(err) => {
                warn!("failed to register {name} irq handler for irq {irq:?}: {err:?}");
                Err(err)
            }
        }
    }

    pub fn handle(&self) -> Option<IrqHandle> {
        self.handle
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        if let Err(err) = ax_hal::irq::free_irq(handle) {
            warn!("failed to free {} irq handler: {err:?}", self.name);
        }
    }
}

#[cfg(feature = "net")]
pub(crate) struct RuntimeNetIrqRegistrar;

#[cfg(feature = "net")]
pub(crate) static NET_IRQ_REGISTRAR: RuntimeNetIrqRegistrar = RuntimeNetIrqRegistrar;

#[cfg(feature = "net")]
impl ax_net::EthernetIrqRegistration for Registration {}

#[cfg(feature = "net")]
fn map_net_irq_error(err: IrqError) -> ax_net::EthernetIrqRegistrationError {
    match err {
        IrqError::InvalidIrq | IrqError::InvalidCpu => {
            ax_net::EthernetIrqRegistrationError::InvalidIrq
        }
        IrqError::Busy | IrqError::InIrqContext => ax_net::EthernetIrqRegistrationError::Busy,
        IrqError::Unsupported | IrqError::CpuOffline => {
            ax_net::EthernetIrqRegistrationError::Unsupported
        }
        IrqError::NoMemory | IrqError::NotFound | IrqError::Timeout | IrqError::Controller => {
            ax_net::EthernetIrqRegistrationError::Other
        }
    }
}

#[cfg(feature = "net")]
impl ax_net::EthernetIrqRegistrar for RuntimeNetIrqRegistrar {
    fn register_shared(
        &self,
        name: &str,
        irq: IrqId,
        action: ax_net::EthernetIrqAction,
    ) -> Result<
        alloc::boxed::Box<dyn ax_net::EthernetIrqRegistration>,
        ax_net::EthernetIrqRegistrationError,
    > {
        let mut action = action;
        let handler = move |_ctx| match action.run() {
            ax_net::EthernetIrqOutcome::Handled => ax_hal::irq::IrqReturn::Handled,
            ax_net::EthernetIrqOutcome::Wake => ax_hal::irq::IrqReturn::Wake,
        };
        Registration::register_shared(name, irq, handler)
            .map(|registration| {
                alloc::boxed::Box::new(registration)
                    as alloc::boxed::Box<dyn ax_net::EthernetIrqRegistration>
            })
            .map_err(map_net_irq_error)
    }
}
