#[cfg(feature = "rockchip-dwmmc")]
use alloc::format;

#[cfg(any(feature = "rockchip-dwmmc", test))]
use fdt_edit::Node;
use log::info;
#[cfg(feature = "rockchip-dwmmc")]
use rdrive::probe::fdt::ClockRef;
use rdrive::{probe::OnProbeError, register::FdtInfo};

#[cfg(feature = "rockchip-dwmmc")]
use crate::soc::scmi;

#[cfg(any(feature = "rockchip-dwmmc", test))]
fn is_scmi_clock_protocol(node: &Node) -> bool {
    node.name().starts_with("protocol@14")
        && node.get_property("reg").and_then(|prop| prop.get_u32()) == Some(0x14)
}

/// Enables every non-placeholder clock referenced by the RK3588 DWCMSHC node.
pub(crate) fn enable_node_clocks(info: &FdtInfo<'_>, label: &str) -> Result<(), OnProbeError> {
    for clock in info.clocks()? {
        if clock.select() == Some(0) {
            continue;
        }

        #[cfg(feature = "rockchip-dwmmc")]
        if let Some(scmi_clock) = ScmiClockOps::from_ref(info, &clock) {
            scmi_clock.enable().ok_or_else(|| {
                OnProbeError::other(format!(
                    "[{}] failed to enable SCMI {label} clock {:?} ({:#x})",
                    info.node.name(),
                    clock.name,
                    scmi_clock.clock_id
                ))
            })?;
            info!(
                "[{}] enabled {label} SCMI clock {:?} ({:#x})",
                info.node.name(),
                clock.name,
                scmi_clock.clock_id
            );
            continue;
        }

        let line = info.clock_line(&clock)?;
        line.enable()?;
        info!(
            "[{}] enabled {label} clock {:?} ({:#x})",
            info.node.name(),
            clock.name,
            line.id().raw()
        );
    }
    Ok(())
}

#[cfg(feature = "rockchip-dwmmc")]
pub(crate) struct ScmiClockOps {
    phandle: fdt_edit::Phandle,
    clock_id: u32,
}

#[cfg(feature = "rockchip-dwmmc")]
impl ScmiClockOps {
    pub(crate) fn from_ref(info: &FdtInfo<'_>, clock: &ClockRef) -> Option<Self> {
        let provider = info.get_by_phandle(clock.phandle)?;
        if !is_scmi_clock_protocol(provider.as_node()) {
            return None;
        }
        Some(Self {
            phandle: clock.phandle,
            clock_id: *clock.specifier.first()?,
        })
    }

    fn enable(&self) -> Option<()> {
        scmi::enable_clock(self.phandle, self.clock_id)
    }

    pub(crate) fn set_rate(&self, rate: u64) -> Option<()> {
        scmi::set_clock_rate(self.phandle, self.clock_id, rate)
    }

    pub(crate) fn rate(&self) -> Option<u64> {
        scmi::clock_rate(self.phandle, self.clock_id)
    }
}

#[cfg(test)]
mod tests {
    use fdt_edit::{Node, Property};

    use super::*;

    #[test]
    fn identifies_scmi_clock_protocol_provider() {
        let mut scmi_clock = Node::new("protocol@14");
        scmi_clock.add_property(Property::new("reg", 0x14_u32.to_be_bytes().to_vec()));

        assert!(is_scmi_clock_protocol(&scmi_clock));
    }

    #[test]
    fn does_not_treat_cru_as_scmi_clock_protocol() {
        let mut cru = Node::new("clock-controller@fd7c0000");
        cru.add_property(Property::new("reg", 0xfd7c0000_u32.to_be_bytes().to_vec()));

        assert!(!is_scmi_clock_protocol(&cru));
    }
}
