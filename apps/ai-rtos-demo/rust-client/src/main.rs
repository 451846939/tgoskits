// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

use std::{
    env,
    ffi::CString,
    fs::File,
    io::{self, Write},
    net::{TcpStream, ToSocketAddrs},
    os::raw::{c_char, c_int, c_short, c_ulong, c_void},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use aicp_rust_protocol::{
    ERROR_OK as AICP_OK, FLAG_ACK_REQUIRED as AICP_FLAG_ACK_REQUIRED, Header as AicpHeader,
    MSG_CONTROL_SET as AICP_MSG_CONTROL_SET, MSG_ERROR as AICP_MSG_ERROR,
    MSG_HELLO as AICP_MSG_HELLO, MSG_STATUS as AICP_MSG_STATUS,
    io::{receive_frame as recv_frame, send_frame},
};

#[derive(Clone, Copy, Debug)]
struct ControlPayload {
    target: f32,
    kp: f32,
    ki: f32,
    kd: f32,
    feed_forward: f32,
    mode: u32,
}

#[derive(Clone, Copy, Debug)]
struct StatusPayload {
    setpoint: f32,
    measured: f32,
    control_output: f32,
    error: f32,
    mode: u32,
    applied_seq: u32,
}

#[derive(Debug)]
struct Config {
    host: String,
    port: u16,
    iterations: u32,
    csv_path: String,
    mode: String,
    period_ms: u64,
    reconnect_ms: u64,
    io_timeout_ms: u64,
    connect_retries: u32,
    guest_init: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8800,
            iterations: 100,
            csv_path: "build/aicp_rust_latency.csv".to_string(),
            mode: "ai".to_string(),
            period_ms: 20,
            reconnect_ms: 200,
            io_timeout_ms: 1000,
            connect_retries: 1,
            guest_init: false,
        }
    }
}

const AF_INET: c_int = 2;
const SOCK_DGRAM: c_int = 2;
const IFF_UP: c_short = 0x1;
const SIOCGIFFLAGS: c_ulong = 0x8913;
const SIOCSIFFLAGS: c_ulong = 0x8914;
const SIOCSIFADDR: c_ulong = 0x8916;
const SIOCSIFNETMASK: c_ulong = 0x891c;
const SIOCADDRT: c_ulong = 0x890b;
const SIOCSARP: c_ulong = 0x8955;
const RTF_UP: u16 = 0x0001;
const ARPHRD_ETHER: u16 = 1;
const ATF_COM: c_int = 0x02;
const ATF_PERM: c_int = 0x04;
const MS_NOSUID: c_ulong = 2;
const MS_NODEV: c_ulong = 4;
const MS_NOEXEC: c_ulong = 8;

#[repr(C)]
#[derive(Clone, Copy)]
struct InAddr {
    s_addr: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddr {
    sa_family: u16,
    sa_data: [c_char; 14],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrIn {
    sin_family: u16,
    sin_port: u16,
    sin_addr: InAddr,
    sin_zero: [u8; 8],
}

#[repr(C)]
struct IfReqAddr {
    ifr_name: [c_char; 16],
    ifr_addr: SockAddrIn,
}

#[repr(C)]
struct IfReqFlags {
    ifr_name: [c_char; 16],
    ifr_flags: c_short,
    padding: [u8; 22],
}

#[repr(C)]
struct RtEntry {
    rt_pad1: c_ulong,
    rt_dst: SockAddr,
    rt_gateway: SockAddr,
    rt_genmask: SockAddr,
    rt_flags: u16,
    rt_pad2: c_short,
    rt_pad3: c_ulong,
    rt_pad4: *mut u8,
    rt_metric: c_short,
    rt_dev: *mut c_char,
    rt_mtu: c_ulong,
    rt_window: c_ulong,
    rt_irtt: u16,
}

#[repr(C)]
struct ArpReq {
    arp_pa: SockAddr,
    arp_ha: SockAddr,
    arp_flags: c_int,
    arp_netmask: SockAddr,
    arp_dev: [c_char; 16],
}

unsafe extern "C" {
    fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn mount(
        source: *const c_char,
        target: *const c_char,
        filesystemtype: *const c_char,
        mountflags: c_ulong,
        data: *const c_void,
    ) -> c_int;
}

fn apply_guest_defaults(cfg: &mut Config) {
    cfg.host = "10.0.3.2".to_string();
    cfg.port = 8800;
    cfg.iterations = 40;
    cfg.csv_path = "/aicp_rust_latency.csv".to_string();
    cfg.mode = "ai".to_string();
    cfg.period_ms = 20;
    cfg.reconnect_ms = 200;
    cfg.io_timeout_ms = 1000;
    cfg.connect_retries = 120;
    cfg.guest_init = true;
}

fn mount_one(source: &str, target: &str, fs_type: &str, flags: c_ulong) -> io::Result<()> {
    let source = CString::new(source)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad mount source"))?;
    let target = CString::new(target)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad mount target"))?;
    let fs_type = CString::new(fs_type)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad mount fs type"))?;
    let ret = unsafe {
        mount(
            source.as_ptr(),
            target.as_ptr(),
            fs_type.as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(16) {
            Ok(())
        } else {
            Err(err)
        }
    }
}

fn ensure_virtual_fs() {
    let _ = std::fs::create_dir_all("/proc");
    let _ = std::fs::create_dir_all("/sys");
    match mount_one("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC) {
        Ok(()) => println!("AICP_RUST_GUEST_MOUNT path=/proc ret=0"),
        Err(err) => println!("AICP_RUST_GUEST_MOUNT path=/proc ret=-1 err={err}"),
    }
    match mount_one("sysfs", "/sys", "sysfs", MS_NOSUID | MS_NODEV | MS_NOEXEC) {
        Ok(()) => println!("AICP_RUST_GUEST_MOUNT path=/sys ret=0"),
        Err(err) => println!("AICP_RUST_GUEST_MOUNT path=/sys ret=-1 err={err}"),
    }
}

fn apply_guest_cmdline(cfg: &mut Config) {
    let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") else {
        println!("AICP_RUST_GUEST_CMDLINE ret=-1 path=/proc/cmdline");
        return;
    };
    println!("AICP_RUST_GUEST_CMDLINE data={}", cmdline.trim());
    for token in cmdline.split_whitespace() {
        if let Some(value) = token.strip_prefix("aicp.iterations=") {
            if let Ok(v) = value.parse() {
                cfg.iterations = v;
            }
        } else if let Some(value) = token.strip_prefix("aicp.mode=") {
            if value == "ai" || value == "fixed" {
                cfg.mode = value.to_string();
            }
        } else if let Some(value) = token.strip_prefix("aicp.period_ms=") {
            if let Ok(v) = value.parse() {
                cfg.period_ms = v;
            }
        } else if let Some(value) = token.strip_prefix("aicp.server=") {
            cfg.host = value.to_string();
        } else if let Some(value) = token.strip_prefix("aicp.port=") {
            if let Ok(v) = value.parse() {
                cfg.port = v;
            }
        } else if let Some(value) = token.strip_prefix("aicp.connect_retries=") {
            if let Ok(v) = value.parse() {
                cfg.connect_retries = v;
            }
        }
    }
}

fn ifname_array(name: &str) -> [c_char; 16] {
    let mut out = [0 as c_char; 16];
    for (idx, byte) in name.as_bytes().iter().copied().take(15).enumerate() {
        out[idx] = byte as c_char;
    }
    out
}

fn sockaddr_from_ipv4(ip: &str) -> io::Result<SockAddrIn> {
    let addr = ip
        .parse::<std::net::Ipv4Addr>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid IPv4 address"))?;
    Ok(SockAddrIn {
        sin_family: AF_INET as u16,
        sin_port: 0,
        sin_addr: InAddr {
            s_addr: addr.octets(),
        },
        sin_zero: [0; 8],
    })
}

fn sockaddr_raw_from_ipv4(ip: &str) -> io::Result<SockAddr> {
    let addr = ip
        .parse::<std::net::Ipv4Addr>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid IPv4 address"))?;
    let mut sa_data = [0 as c_char; 14];
    for (idx, byte) in addr.octets().iter().copied().enumerate() {
        sa_data[idx + 2] = byte as c_char;
    }
    Ok(SockAddr {
        sa_family: AF_INET as u16,
        sa_data,
    })
}

fn parse_mac(mac: &str) -> io::Result<[u8; 6]> {
    let mut out = [0u8; 6];
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid MAC address",
        ));
    }
    for (idx, part) in parts.iter().enumerate() {
        out[idx] = u8::from_str_radix(part, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid MAC address"))?;
    }
    Ok(out)
}

fn set_ifaddr(fd: c_int, ifname: &str, request: c_ulong, ip: &str) -> io::Result<()> {
    let mut ifr = IfReqAddr {
        ifr_name: ifname_array(ifname),
        ifr_addr: sockaddr_from_ipv4(ip)?,
    };
    let ret = unsafe { ioctl(fd, request, &mut ifr) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_if_up(fd: c_int, ifname: &str) -> io::Result<()> {
    let mut ifr = IfReqFlags {
        ifr_name: ifname_array(ifname),
        ifr_flags: 0,
        padding: [0; 22],
    };
    let ret = unsafe { ioctl(fd, SIOCGIFFLAGS, &mut ifr) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    ifr.ifr_flags |= IFF_UP;
    let ret = unsafe { ioctl(fd, SIOCSIFFLAGS, &mut ifr) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn add_connected_route(fd: c_int, ifname: &str) -> io::Result<()> {
    let mut dev = ifname_array(ifname);
    let mut route = RtEntry {
        rt_pad1: 0,
        rt_dst: sockaddr_raw_from_ipv4("10.0.3.0")?,
        rt_gateway: sockaddr_raw_from_ipv4("0.0.0.0")?,
        rt_genmask: sockaddr_raw_from_ipv4("255.255.255.0")?,
        rt_flags: RTF_UP,
        rt_pad2: 0,
        rt_pad3: 0,
        rt_pad4: std::ptr::null_mut(),
        rt_metric: 0,
        rt_dev: dev.as_mut_ptr(),
        rt_mtu: 0,
        rt_window: 0,
        rt_irtt: 0,
    };
    let ret = unsafe { ioctl(fd, SIOCADDRT, &mut route) };
    if ret == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(17) {
            Ok(())
        } else {
            Err(err)
        }
    }
}

fn add_static_arp(fd: c_int, ifname: &str) -> io::Result<()> {
    let mac = parse_mac("52:54:00:aa:03:02")?;
    let mut arp = ArpReq {
        arp_pa: sockaddr_raw_from_ipv4("10.0.3.2")?,
        arp_ha: SockAddr {
            sa_family: ARPHRD_ETHER,
            sa_data: [0 as c_char; 14],
        },
        arp_flags: ATF_COM | ATF_PERM,
        arp_netmask: SockAddr {
            sa_family: 0,
            sa_data: [0 as c_char; 14],
        },
        arp_dev: ifname_array(ifname),
    };
    for (idx, byte) in mac.iter().copied().enumerate() {
        arp.arp_ha.sa_data[idx] = byte as c_char;
    }
    let ret = unsafe { ioctl(fd, SIOCSARP, &mut arp) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn configure_guest_network() -> io::Result<()> {
    let ifname = "eth0";
    let fd = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let result = (|| {
        set_ifaddr(fd, ifname, SIOCSIFADDR, "10.0.3.3")?;
        println!("AICP_RUST_GUEST_NETCFG step=SIOCSIFADDR ret=0 ip=10.0.3.3");
        set_ifaddr(fd, ifname, SIOCSIFNETMASK, "255.255.255.0")?;
        println!("AICP_RUST_GUEST_NETCFG step=SIOCSIFNETMASK ret=0");
        set_if_up(fd, ifname)?;
        println!("AICP_RUST_GUEST_NETCFG step=SIOCSIFFLAGS ret=0");
        match add_connected_route(fd, ifname) {
            Ok(()) => println!("AICP_RUST_GUEST_NETCFG step=SIOCADDRT ret=0"),
            Err(err) => println!("AICP_RUST_GUEST_NETCFG step=SIOCADDRT ret=-1 err={err}"),
        }
        match add_static_arp(fd, ifname) {
            Ok(()) => println!(
                "AICP_RUST_GUEST_NETCFG step=SIOCSARP ret=0 server=10.0.3.2 mac=52:54:00:aa:03:02"
            ),
            Err(err) => println!(
                "AICP_RUST_GUEST_NETCFG step=SIOCSARP ret=-1 err={err} server=10.0.3.2 \
                 mac=52:54:00:aa:03:02"
            ),
        }
        Ok(())
    })();

    unsafe {
        close(fd);
    }
    result
}

fn configure_guest_network_with_retries() -> io::Result<()> {
    let mut last = None;
    for attempt in 1..=40 {
        match configure_guest_network() {
            Ok(()) => return Ok(()),
            Err(err) => {
                println!("AICP_RUST_GUEST_NETCFG attempt={attempt} ret=-1 err={err}");
                last = Some(err);
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("network config failed")))
}

fn monotonic_ns(start: Instant) -> u64 {
    start.elapsed().as_nanos() as u64
}

fn wallclock_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_nanos() as u64
}

fn control_to_bytes(c: ControlPayload) -> [u8; 24] {
    let mut out = [0u8; 24];
    out[0..4].copy_from_slice(&c.target.to_le_bytes());
    out[4..8].copy_from_slice(&c.kp.to_le_bytes());
    out[8..12].copy_from_slice(&c.ki.to_le_bytes());
    out[12..16].copy_from_slice(&c.kd.to_le_bytes());
    out[16..20].copy_from_slice(&c.feed_forward.to_le_bytes());
    out[20..24].copy_from_slice(&c.mode.to_le_bytes());
    out
}

fn status_from_bytes(payload: &[u8]) -> io::Result<StatusPayload> {
    if payload.len() != 24 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad STATUS payload size",
        ));
    }
    Ok(StatusPayload {
        setpoint: f32::from_le_bytes(payload[0..4].try_into().unwrap()),
        measured: f32::from_le_bytes(payload[4..8].try_into().unwrap()),
        control_output: f32::from_le_bytes(payload[8..12].try_into().unwrap()),
        error: f32::from_le_bytes(payload[12..16].try_into().unwrap()),
        mode: u32::from_le_bytes(payload[16..20].try_into().unwrap()),
        applied_seq: u32::from_le_bytes(payload[20..24].try_into().unwrap()),
    })
}

fn connect_tcp(cfg: &Config) -> io::Result<TcpStream> {
    let addr = format!("{}:{}", cfg.host, cfg.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no socket address"))?;
    let timeout = Duration::from_millis(cfg.io_timeout_ms);
    let stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

fn send_hello(stream: &mut TcpStream, seq: &mut u32) -> io::Result<()> {
    let payload = b"{\"role\":\"rust-ai\",\"cap\":\"control,status,heartbeat\"}\0";
    let header = AicpHeader::new(
        AICP_MSG_HELLO,
        0,
        payload.len() as u32,
        *seq,
        wallclock_ns(),
        AICP_OK,
    );
    *seq += 1;
    send_frame(stream, header, payload)
}

fn transact_control(
    stream: &mut TcpStream,
    seq: &mut u32,
    control: ControlPayload,
) -> io::Result<(StatusPayload, u64)> {
    let payload = control_to_bytes(control);
    let start = Instant::now();
    let header = AicpHeader::new(
        AICP_MSG_CONTROL_SET,
        AICP_FLAG_ACK_REQUIRED,
        payload.len() as u32,
        *seq,
        wallclock_ns(),
        AICP_OK,
    );
    *seq += 1;
    send_frame(stream, header, &payload)?;
    let (rx, rx_payload) = recv_frame(stream)?;
    let rtt_ns = monotonic_ns(start);
    if rx.msg_type == AICP_MSG_ERROR {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "AICP ERROR reply",
        ));
    }
    if rx.msg_type != AICP_MSG_STATUS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected AICP STATUS",
        ));
    }
    Ok((status_from_bytes(&rx_payload)?, rtt_ns))
}

fn clamp(value: f32, low: f32, high: f32) -> f32 {
    value.max(low).min(high)
}

fn target_trajectory(step: u32) -> f32 {
    let base = 0.52 + 0.28 * ((step as f32) * 0.055).sin();
    let pulse = if step % 80 >= 40 { 0.10 } else { -0.04 };
    clamp(base + pulse, 0.05, 0.95)
}

fn nn_infer_adaptation(sensor: f32, trend: f32, load: f32) -> f32 {
    let w1 = [
        [0.70, -0.22, 0.15],
        [-0.35, 0.91, 0.08],
        [0.24, 0.10, -0.52],
        [0.44, 0.28, 0.36],
    ];
    let b1 = [0.05, -0.02, 0.01, 0.03];
    let w2 = [0.48, -0.31, 0.22, 0.35];
    let x = [sensor, trend, load];
    let mut y = 0.45;
    for i in 0..4 {
        let mut h = b1[i];
        for j in 0..3 {
            h += w1[i][j] * x[j];
        }
        y += w2[i] * h.tanh();
    }
    clamp(y, 0.05, 0.95)
}

fn control_for_step(step: u32, ai_mode: bool) -> ControlPayload {
    let sensor = ((step as f32) * 0.07).sin();
    let trend = ((step as f32) * 0.03).cos();
    let load = (step % 17) as f32 / 16.0;
    let adaptation = nn_infer_adaptation(sensor, trend, load);
    ControlPayload {
        target: target_trajectory(step),
        kp: if ai_mode {
            0.58 + 0.22 * adaptation
        } else {
            0.45
        },
        ki: if ai_mode { 0.06 + 0.04 * load } else { 0.03 },
        kd: if ai_mode { 0.02 } else { 0.0 },
        feed_forward: if ai_mode {
            0.08 * load + 0.04 * trend
        } else {
            0.0
        },
        mode: if ai_mode { 5 } else { 0 },
    }
}

fn parse_args() -> Result<Config, String> {
    let raw_args: Vec<String> = env::args().collect();
    let program = raw_args
        .first()
        .cloned()
        .unwrap_or_else(|| "aicp_rust_client".to_string());
    let mut args = vec![program.clone()];
    let mut explicit_guest =
        cfg!(feature = "guest-init") || env::var("AICP_RUST_GUEST_INIT").as_deref() == Ok("1");

    for arg in raw_args.into_iter().skip(1) {
        if arg == "--guest-init" {
            explicit_guest = true;
        } else {
            args.push(arg);
        }
    }

    if args.len() > 6 || args.get(1).map(String::as_str) == Some("-h") {
        return Err(format!(
            "usage: {} [--guest-init] [host] [port] [iterations] [csv] [ai|fixed]\ndefault host \
             mode: 127.0.0.1 8800 100 build/aicp_rust_latency.csv ai\ndefault guest-init mode: \
             10.0.3.2 8800 40 /aicp_rust_latency.csv ai",
            program
        ));
    }
    let mut cfg = Config::default();
    if explicit_guest {
        apply_guest_defaults(&mut cfg);
    }
    if let Some(v) = args.get(1) {
        cfg.host = v.clone();
    }
    if let Some(v) = args.get(2) {
        cfg.port = v.parse().map_err(|_| format!("invalid port: {v}"))?;
    }
    if let Some(v) = args.get(3) {
        cfg.iterations = v.parse().map_err(|_| format!("invalid iterations: {v}"))?;
    }
    if let Some(v) = args.get(4) {
        cfg.csv_path = v.clone();
    }
    if let Some(v) = args.get(5) {
        cfg.mode = v.clone();
    }
    if cfg.mode != "ai" && cfg.mode != "fixed" {
        return Err(format!("invalid mode: {}", cfg.mode));
    }
    Ok(cfg)
}

fn idle_as_init() -> ! {
    println!("AICP_RUST_GUEST_IDLE pid=1 reason=linux_init_must_not_exit");
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

fn main() -> io::Result<()> {
    let mut cfg = match parse_args() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    if cfg.guest_init {
        println!("AICP_RUST_GUEST_INIT begin=1");
        ensure_virtual_fs();
        apply_guest_cmdline(&mut cfg);
        if let Err(err) = configure_guest_network_with_retries() {
            println!(
                "AICP_RUST_DONE ok=0 failed=1 csv={} err={err}",
                cfg.csv_path
            );
            idle_as_init();
        }
    }
    let ai_mode = cfg.mode == "ai";
    let mut csv = File::create(&cfg.csv_path)?;
    writeln!(csv, "seq,rtt_ns,target,measured,error,control_output")?;

    println!(
        "AICP_RUST_BEGIN host={} port={} iterations={} mode={}",
        cfg.host, cfg.port, cfg.iterations, cfg.mode
    );

    let mut stream: Option<TcpStream> = None;
    let mut seq = 1u32;
    let mut ok = 0u32;
    let mut failed = 0u32;

    for step in 0..cfg.iterations {
        if stream.is_none() {
            let retries = cfg.connect_retries.max(1);
            let mut last_err = None;
            for attempt in 1..=retries {
                match connect_tcp(&cfg).and_then(|mut s| {
                    send_hello(&mut s, &mut seq)?;
                    Ok(s)
                }) {
                    Ok(s) => {
                        println!("AICP_RUST_CONNECTED seq_next={seq} attempt={attempt}");
                        stream = Some(s);
                        break;
                    }
                    Err(err) => {
                        eprintln!(
                            "AICP_RUST_RECONNECT step={step} attempt={attempt}/{retries} err={err}"
                        );
                        last_err = Some(err);
                        thread::sleep(Duration::from_millis(cfg.reconnect_ms));
                    }
                }
            }
            if stream.is_none() {
                failed += 1;
                if let Some(err) = last_err {
                    eprintln!("AICP_RUST_CONNECT_FAIL step={step} err={err}");
                }
                continue;
            }
        }

        let control = control_for_step(step, ai_mode);
        let result = transact_control(stream.as_mut().unwrap(), &mut seq, control);
        match result {
            Ok((status, rtt_ns)) => {
                ok += 1;
                println!(
                    "AICP_RUST_STATUS seq={} target={:.3} measured={:.3} error={:.3} output={:.3} \
                     mode={} rtt_ns={}",
                    status.applied_seq,
                    control.target,
                    status.measured,
                    status.error,
                    status.control_output,
                    status.mode,
                    rtt_ns
                );
                writeln!(
                    csv,
                    "{},{},{:.6},{:.6},{:.6},{:.6}",
                    status.applied_seq,
                    rtt_ns,
                    status.setpoint,
                    status.measured,
                    status.error,
                    status.control_output
                )?;
            }
            Err(err) => {
                failed += 1;
                eprintln!("AICP_RUST_FAIL step={step} err={err}");
                stream = None;
                thread::sleep(Duration::from_millis(cfg.reconnect_ms));
                continue;
            }
        }
        thread::sleep(Duration::from_millis(cfg.period_ms));
    }

    println!(
        "AICP_RUST_DONE ok={ok} failed={failed} csv={}",
        cfg.csv_path
    );
    if failed == 0 {
        if cfg.guest_init {
            idle_as_init();
        }
        Ok(())
    } else if cfg.guest_init {
        idle_as_init();
    } else {
        std::process::exit(1);
    }
}
