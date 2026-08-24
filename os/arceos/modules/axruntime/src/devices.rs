pub(crate) fn probe_all_devices() {
    info!("Probe platform devices...");
    if !rdrive::is_initialized() {
        warn!("rdrive is not initialized; skip platform device probe");
        return;
    }
    rdrive::probe_all(false)
        .unwrap_or_else(|err| panic!("failed to probe platform devices: {err:?}"));
}

#[cfg(feature = "display")]
pub(crate) fn init_display() {
    if !rdrive::is_initialized() {
        ax_display::init_display(core::iter::empty::<ax_display::ErasedDisplayDevice>());
        return;
    }
    let devices = ax_driver::display::take_display_devices()
        .unwrap_or_else(|err| panic!("failed to open display devices: {err:?}"))
        .into_iter()
        .map(adapt_display_device);
    ax_display::init_display(devices);
}

#[cfg(feature = "display")]
fn adapt_display_device(
    taken: ax_driver::display::TakenDisplayDevice,
) -> ax_display::ErasedDisplayDevice {
    let name = alloc::string::String::from(taken.device.name());
    let irq = resolve_display_irq(&name, taken.irq)
        .unwrap_or_else(|err| panic!("failed to resolve display IRQ for {name}: {err:?}"));
    let display = ax_display::rdif::RdifDisplayDevice::new_with_irq(taken.device, irq)
        .unwrap_or_else(|err| panic!("failed to adapt display device: {err:?}"));
    ax_display::ErasedDisplayDevice::new(display)
}

#[cfg(all(feature = "display", feature = "irq"))]
fn resolve_display_irq(
    _name: &str,
    irq: Option<ax_driver::BindingIrq>,
) -> Result<Option<irq_framework::IrqId>, irq_framework::IrqError> {
    irq.map(crate::irq::resolve_binding_irq).transpose()
}

#[cfg(all(feature = "display", not(feature = "irq")))]
fn resolve_display_irq(
    name: &str,
    irq: Option<ax_driver::BindingIrq>,
) -> Result<Option<irq_framework::IrqId>, core::convert::Infallible> {
    if irq.is_some() {
        warn!("display device {name} has an IRQ binding but IRQ support is disabled");
    }
    Ok(None)
}

#[cfg(feature = "input")]
pub(crate) fn init_input() {
    if !rdrive::is_initialized() {
        ax_input::init_input(core::iter::empty::<ax_input::ErasedInputDevice>());
        return;
    }
    let devices = ax_driver::input::take_input_devices()
        .unwrap_or_else(|err| panic!("failed to open input devices: {err:?}"))
        .into_iter()
        .map(adapt_input_device);
    ax_input::init_input(devices);
}

#[cfg(feature = "input")]
fn adapt_input_device(taken: ax_driver::input::TakenInputDevice) -> ax_input::ErasedInputDevice {
    let name = alloc::string::String::from(taken.device.name());
    let irq = resolve_input_irq(&name, taken.irq)
        .unwrap_or_else(|err| panic!("failed to resolve input IRQ for {name}: {err:?}"));
    ax_input::ErasedInputDevice::new(ax_input::rdif::RdifInputDevice::new_with_irq(
        taken.device,
        irq,
    ))
}

#[cfg(all(feature = "input", feature = "irq"))]
fn resolve_input_irq(
    _name: &str,
    irq: Option<ax_driver::BindingIrq>,
) -> Result<Option<irq_framework::IrqId>, irq_framework::IrqError> {
    irq.map(crate::irq::resolve_binding_irq).transpose()
}

#[cfg(all(feature = "input", not(feature = "irq")))]
fn resolve_input_irq(
    name: &str,
    irq: Option<ax_driver::BindingIrq>,
) -> Result<Option<irq_framework::IrqId>, core::convert::Infallible> {
    if irq.is_some() {
        warn!("input device {name} has an IRQ binding but IRQ support is disabled");
    }
    Ok(None)
}

#[cfg(feature = "net")]
pub(crate) fn init_net() {
    #[cfg(feature = "irq")]
    ax_net::set_ethernet_irq_registrar(&crate::irq::NET_IRQ_REGISTRAR);
    register_unix_namespace();
    let config = parse_network_config();
    let (nics, wireless) = collect_net_devices();
    ax_net::init_network(nics, config);
    register_wireless_devices(wireless);
}

#[cfg(all(feature = "net", feature = "fs"))]
fn register_unix_namespace() {
    ax_net::unix::register_unix_namespace(crate::unix_ns::AxFsUnixNamespace);
}

#[cfg(all(feature = "net", not(feature = "fs")))]
fn register_unix_namespace() {
    // Path-based Unix sockets require filesystem namespace support.
}

#[cfg(feature = "net")]
fn parse_network_config() -> ax_net::NetworkConfig {
    match option_env!("AX_IP") {
        None => ax_net::NetworkConfig::default(),
        Some(ip) => static_network_config(
            ip,
            option_env!("AX_GW"),
            option_env!("AX_PREFIX_LEN").unwrap_or("24"),
        )
        .unwrap_or_else(|error| panic!("invalid static network build configuration: {error}")),
    }
}

/// Builds the opt-in static configuration for the first Ethernet interface.
///
/// A build that sets `AX_IP` intentionally bypasses DHCP. This prevents a
/// later DHCP lease from replacing the address selected by a guest topology.
#[cfg(feature = "net")]
fn static_network_config(
    ip: &str,
    gateway: Option<&str>,
    prefix_len: &str,
) -> Result<ax_net::NetworkConfig, &'static str> {
    let ip = ip.parse().map_err(|_| "AX_IP is not an IPv4 address")?;
    let gateway = gateway
        .map(str::parse)
        .transpose()
        .map_err(|_| "AX_GW is not an IPv4 address")?
        .unwrap_or(core::net::Ipv4Addr::UNSPECIFIED);
    let prefix_len = prefix_len
        .parse()
        .map_err(|_| "AX_PREFIX_LEN is not an unsigned integer")?;
    if prefix_len > 32 {
        return Err("AX_PREFIX_LEN is greater than 32");
    }

    Ok(ax_net::NetworkConfig {
        interfaces: alloc::vec![ax_net::InterfaceConfig {
            name: "eth0".into(),
            match_by: ax_net::InterfaceMatcher::ByOrder(0),
            static_ip: Some(ax_net::StaticIpConfig {
                ip,
                prefix_len,
                gateway,
            }),
            dhcp: false,
            metric: 100,
            dns_servers: alloc::vec![],
        }],
        default_dns_servers: alloc::vec![],
    })
}

#[cfg(all(test, feature = "net"))]
mod network_config_tests {
    use super::*;

    #[test]
    fn static_build_config_disables_dhcp_for_eth0() {
        let config = static_network_config("10.0.3.2", Some("10.0.3.1"), "24").unwrap();
        let interface = &config.interfaces[0];
        assert_eq!(interface.name, "eth0");
        assert!(matches!(
            interface.match_by,
            ax_net::InterfaceMatcher::ByOrder(0)
        ));
        assert!(!interface.dhcp);
        let address = interface.static_ip.as_ref().unwrap();
        assert_eq!(address.ip, core::net::Ipv4Addr::new(10, 0, 3, 2));
        assert_eq!(address.gateway, core::net::Ipv4Addr::new(10, 0, 3, 1));
        assert_eq!(address.prefix_len, 24);
    }

    #[test]
    fn static_build_config_rejects_invalid_prefix() {
        assert!(static_network_config("10.0.3.2", None, "33").is_err());
    }
}

/// A wireless device that registers *after* `init_network`: its already-wrapped
/// driver plus the link policy (static IP + optional DHCP-server lease) the
/// board reported for it.
#[cfg(feature = "net")]
type WirelessDevice = (
    alloc::boxed::Box<dyn ax_net::EthernetDriver>,
    ax_net::NetConfig,
);

/// Wraps one probed net device, splitting it into either a plain NIC (for the
/// `init_network` device list) or a wireless device (registered separately with
/// its link policy).
///
/// A wireless device is just a `PlatformNetDevice` whose underlying `Interface`
/// exposes a [`rd_net::WifiControl`] (via `Net::wifi_control`). We read its link
/// policy and wire its out-of-band RX callback here, then wrap the same
/// `rd_net::Net` data plane every NIC uses. Keeping the Wi-Fi specifics on the
/// device (not in the stack) is what lets the protocol stack stay link-agnostic.
#[cfg(feature = "net")]
fn adapt_net_device(
    net: rd_net::Net,
    name: &'static str,
    irq: Option<ax_driver::BindingIrq>,
    nics: &mut alloc::vec::Vec<alloc::boxed::Box<dyn ax_net::EthernetDriver>>,
    wireless: &mut alloc::vec::Vec<WirelessDevice>,
) {
    // If this device has a wireless control plane, wire its out-of-band RX wake
    // and read the link policy the board attached to it.
    let policy = if let Some(ctrl) = net.wifi_control() {
        // SDIO Wi-Fi RX is out-of-band (not the ethernet IRQ framework); the
        // chip's RX-data callback wakes the stack's dedicated poll task.
        ctrl.set_rx_wake(ax_net::wake_net_task_irq);
        ctrl.link_policy()
    } else {
        None
    };

    // Capture a standalone control-plane handle *before* the `Net` is consumed
    // into the data-plane driver, so runtime mode switching can reach this
    // device's `WifiControl` by name (see `ax_net::reconfigure_wifi`).
    if let Some(handle) = net.wifi_control_handle() {
        ax_net::register_wifi_control(name, handle);
    }

    let irq = resolve_net_irq(name, irq);
    let driver = ax_net::RdNetDriver::new(name, net, irq)
        .unwrap_or_else(|err| panic!("failed to adapt net device {name}: {err:?}"));
    let driver = alloc::boxed::Box::new(driver) as alloc::boxed::Box<dyn ax_net::EthernetDriver>;

    match policy {
        Some(p) => wireless.push((
            driver,
            ax_net::NetConfig {
                name: name.into(),
                ip: p.ip,
                prefix_len: p.prefix_len,
                dhcp_server_client_ip: p.dhcp_server_client_ip,
                dedicated_poll: true,
            },
        )),
        None => nics.push(driver),
    }
}

#[cfg(all(feature = "net", feature = "irq"))]
fn resolve_net_irq(name: &str, irq: Option<ax_driver::BindingIrq>) -> Option<irq_framework::IrqId> {
    let irq = irq?;
    match crate::irq::resolve_binding_irq(irq) {
        Ok(id) => Some(id),
        Err(err) => {
            warn!("failed to resolve net IRQ for {name}: {err:?}");
            None
        }
    }
}

#[cfg(all(feature = "net", not(feature = "irq")))]
fn resolve_net_irq(
    _name: &str,
    _irq: Option<ax_driver::BindingIrq>,
) -> Option<irq_framework::IrqId> {
    None
}

/// Registers wireless devices that carry a link policy with the
/// already-initialized network stack (static IP + optional DHCP server +
/// dedicated out-of-band RX poll). Plain NICs are handled by `init_network`.
#[cfg(feature = "net")]
fn register_wireless_devices(wireless: alloc::vec::Vec<WirelessDevice>) {
    for (driver, config) in wireless {
        ax_net::register_device_with_config(driver, config);
    }
}

#[cfg(feature = "vsock")]
pub(crate) fn init_vsock() {
    if !rdrive::is_initialized() {
        ax_net::init_vsock(alloc::vec::Vec::new());
        return;
    }
    let devices = ax_driver::vsock::take_vsock_devices()
        .unwrap_or_else(|err| panic!("failed to open vsock devices: {err:?}"));
    ax_net::init_vsock(devices);
}

#[cfg(feature = "net")]
fn collect_net_devices() -> (
    alloc::vec::Vec<alloc::boxed::Box<dyn ax_net::EthernetDriver>>,
    alloc::vec::Vec<WirelessDevice>,
) {
    let mut nics = alloc::vec::Vec::new();
    let mut wireless = alloc::vec::Vec::new();
    if !rdrive::is_initialized() {
        return (nics, wireless);
    }
    for dev in rdrive::get_list::<ax_driver::net::PlatformNetDevice>() {
        let (net, name, irq) = ax_driver::net::take_rd_net_device(dev)
            .unwrap_or_else(|err| panic!("failed to open net device: {err:?}"));
        adapt_net_device(net, name, irq, &mut nics, &mut wireless);
    }
    (nics, wireless)
}
