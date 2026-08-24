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

#[cfg(feature = "display")]
fn resolve_display_irq(
    _name: &str,
    irq: Option<ax_driver::BindingIrq>,
) -> Result<Option<irq_framework::IrqId>, irq_framework::IrqError> {
    irq.map(crate::irq::resolve_binding_irq).transpose()
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

#[cfg(feature = "input")]
fn resolve_input_irq(
    _name: &str,
    irq: Option<ax_driver::BindingIrq>,
) -> Result<Option<irq_framework::IrqId>, irq_framework::IrqError> {
    irq.map(crate::irq::resolve_binding_irq).transpose()
}

#[cfg(feature = "net")]
pub(crate) fn init_net() {
    register_unix_namespace();
    let config = parse_network_config();

    if !rdrive::is_initialized() {
        ax_net::init_network(None, alloc::vec::Vec::new(), config);
        return;
    }

    let devices = collect_net_devices();
    if devices.is_empty() {
        ax_net::init_network(None, alloc::vec::Vec::new(), config);
        return;
    }
    let (runtime, ports) = ax_net::NetworkRuntimeBuilder::new(
        devices,
        &crate::irq::NET_IRQ_REGISTRAR,
        ax_hal::cpu_num(),
    )
    .build()
    .unwrap_or_else(|error| panic!("failed to initialize network queue runtime: {error}"));
    ax_net::init_network(Some(runtime), ports, config);
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

#[cfg(feature = "net")]
fn collect_net_devices() -> alloc::vec::Vec<ax_net::NetworkDeviceInput> {
    let mut devices = alloc::vec::Vec::new();
    for device in rdrive::get_list::<ax_driver::net::PlatformNetDevice>() {
        let taken = ax_driver::net::take_net_device(device)
            .unwrap_or_else(|error| panic!("failed to take network device: {error}"));
        let name = alloc::string::String::from(taken.name);
        let prepared = rd_net::prepare_device(taken.prepared_device, taken.dma)
            .unwrap_or_else(|error| panic!("failed to prepare network device {name}: {error}"));
        let mut irq_sources = alloc::vec::Vec::with_capacity(taken.irq_sources.len());
        for source in taken.irq_sources {
            let source_id = u16::try_from(source.source_id).unwrap_or_else(|_| {
                panic!(
                    "network device {name} IRQ source {} exceeds the source-id width",
                    source.source_id
                )
            });
            let irq = crate::irq::resolve_binding_irq(source.irq).unwrap_or_else(|error| {
                panic!("failed to resolve network device {name} IRQ source {source_id}: {error:?}")
            });
            irq_sources.push(ax_net::ResolvedNetIrqSource {
                source_id: rd_net::NetIrqSourceId::new(source_id),
                irq,
            });
        }
        devices.push(ax_net::NetworkDeviceInput {
            name,
            device: prepared,
            irq_sources,
        });
    }
    devices
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
