// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

use std::{
    env,
    ffi::{CStr, CString, c_char, c_int, c_short, c_ulong, c_void},
    fs, io,
    net::{TcpStream, ToSocketAddrs},
    path::Path,
    ptr, slice, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use aicp_rust_protocol::{
    CONTROL_PAYLOAD_LEN, ERROR_OK, FLAG_ACK_REQUIRED as AICP_FLAG_ACK_REQUIRED,
    STATUS_PAYLOAD_LEN, ControlPayload, Header as AicpHeader, StatusPayload,
    MSG_CONTROL_SET as AICP_MSG_CONTROL_SET, MSG_ERROR as AICP_MSG_ERROR,
    MSG_HELLO as AICP_MSG_HELLO, MSG_STATUS as AICP_MSG_STATUS,
    decode_status_payload, encode_control_payload,
    io::{receive_frame, send_frame},
};

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
struct AicpOrtSession {
    _private: [u8; 0],
}

#[repr(C)]
struct AicpOrtOutput {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn aicp_ort_create(
        model_path: *const c_char,
        threads: c_int,
        error: *mut c_char,
        error_capacity: usize,
    ) -> *mut AicpOrtSession;
    fn aicp_ort_destroy(session: *mut AicpOrtSession);
    fn aicp_ort_run(
        session: *mut AicpOrtSession,
        input: *mut f32,
        input_elements: usize,
        input_shape: *const i64,
        input_rank: usize,
        output: *mut *mut AicpOrtOutput,
        output_data: *mut *const f32,
        output_shape: *mut i64,
        output_shape_capacity: usize,
        output_rank: *mut usize,
        output_elements: *mut usize,
        error: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn aicp_ort_release_output(session: *mut AicpOrtSession, output: *mut AicpOrtOutput);
    fn aicp_image_load_rgb(
        path: *const c_char,
        width: *mut c_int,
        height: *mut c_int,
        error: *mut c_char,
        error_capacity: usize,
    ) -> *mut u8;
    fn aicp_image_free(data: *mut u8);

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

#[derive(Debug, Clone)]
struct Options {
    model: String,
    labels: String,
    image: Option<String>,
    image_list: String,
    host: String,
    port: u16,
    client_ip: String,
    net_prefix: String,
    netmask: String,
    iface: String,
    server_mac: String,
    input_size: usize,
    target_class: i32,
    threads: i32,
    connect_timeout_ms: u64,
    connect_retries: u32,
    connect_retry_delay_ms: u64,
    confidence_threshold: f32,
    nms_threshold: f32,
    dry_run: bool,
    configure_network: bool,
    static_arp: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            model: default_asset_path("/model/yolov8n.onnx", "model/yolov8n.onnx"),
            labels: default_asset_path(
                "/model/coco_80_labels_list.txt",
                "model/coco_80_labels_list.txt",
            ),
            image: None,
            image_list: default_asset_path("/validation/images.txt", "validation/images.txt"),
            host: "10.0.3.2".into(),
            port: 8800,
            client_ip: "10.0.3.3".into(),
            net_prefix: "10.0.3.0".into(),
            netmask: "255.255.255.0".into(),
            iface: "eth0".into(),
            server_mac: "52:54:00:aa:03:02".into(),
            input_size: 640,
            target_class: 32,
            threads: 1,
            connect_timeout_ms: 1000,
            connect_retries: 120,
            connect_retry_delay_ms: 1000,
            confidence_threshold: 0.25,
            nms_threshold: 0.45,
            dry_run: false,
            configure_network: true,
            static_arp: true,
        }
    }
}

#[derive(Debug)]
struct Image {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct Letterbox {
    scale: f32,
    pad_x: f32,
    pad_y: f32,
}

#[derive(Clone, Copy, Debug)]
struct Detection {
    class_id: i32,
    score: f32,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

#[derive(Clone, Copy, Debug)]
struct ControlMapping {
    has_detection: bool,
    class_id: i32,
    confidence: f32,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    control: ControlPayload,
}

impl Default for ControlMapping {
    fn default() -> Self {
        Self {
            has_detection: false,
            class_id: -1,
            confidence: 0.0,
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
            control: ControlPayload {
                target: 0.0,
                kp: 0.42,
                ki: 0.02,
                kd: 0.01,
                feed_forward: 0.0,
                mode: 4,
            },
        }
    }
}

struct OrtSession {
    raw: *mut AicpOrtSession,
}

struct OrtOutput<'a> {
    session: &'a OrtSession,
    raw: *mut AicpOrtOutput,
    data: *const f32,
    elements: usize,
    shape: Vec<i64>,
}

impl OrtSession {
    fn create(model_path: &str, threads: i32) -> io::Result<Self> {
        let model = CString::new(model_path)
            .map_err(|_| invalid_input("model path contains a NUL byte"))?;
        let mut error = [0 as c_char; 512];
        let raw =
            unsafe { aicp_ort_create(model.as_ptr(), threads, error.as_mut_ptr(), error.len()) };
        if raw.is_null() {
            return Err(ffi_error("create ONNX Runtime session", &error));
        }
        Ok(Self { raw })
    }

    fn run<'a>(&'a self, input: &mut [f32], input_shape: &[i64]) -> io::Result<OrtOutput<'a>> {
        let mut raw_output = ptr::null_mut();
        let mut output_data = ptr::null();
        let mut output_shape = [0i64; 8];
        let mut output_rank = 0usize;
        let mut output_elements = 0usize;
        let mut error = [0 as c_char; 512];
        let ret = unsafe {
            aicp_ort_run(
                self.raw,
                input.as_mut_ptr(),
                input.len(),
                input_shape.as_ptr(),
                input_shape.len(),
                &mut raw_output,
                &mut output_data,
                output_shape.as_mut_ptr(),
                output_shape.len(),
                &mut output_rank,
                &mut output_elements,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if ret != 0 || raw_output.is_null() || output_data.is_null() {
            return Err(ffi_error("run ONNX Runtime inference", &error));
        }
        Ok(OrtOutput {
            session: self,
            raw: raw_output,
            data: output_data,
            elements: output_elements,
            shape: output_shape[..output_rank].to_vec(),
        })
    }
}

impl Drop for OrtSession {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { aicp_ort_destroy(self.raw) };
        }
    }
}

impl OrtOutput<'_> {
    fn data(&self) -> &[f32] {
        unsafe { slice::from_raw_parts(self.data, self.elements) }
    }
}

impl Drop for OrtOutput<'_> {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { aicp_ort_release_output(self.session.raw, self.raw) };
        }
    }
}

fn ffi_error(context: &str, buffer: &[c_char]) -> io::Error {
    let detail = unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    io::Error::other(format!("{context}: {detail}"))
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn default_asset_path(guest_path: &str, packaged_path: &str) -> String {
    if Path::new(guest_path).exists() {
        guest_path.into()
    } else {
        packaged_path.into()
    }
}

fn usage(program: &str) {
    println!("Usage: {program} [OPTIONS]");
    println!("  --model PATH                 YOLOv8 ONNX model [/model/yolov8n.onnx or model/yolov8n.onnx]");
    println!("  --labels PATH                COCO labels [/model/coco_80_labels_list.txt or model/coco_80_labels_list.txt]");
    println!("  --image PATH                 process one JPEG image");
    println!("  --image-list PATH            process paths from list [/validation/images.txt or validation/images.txt]");
    println!("  --aicp-host IPV4             RTOS AICP server [10.0.3.2]");
    println!("  --aicp-port PORT             RTOS AICP port [8800]");
    println!("  --client-ip IPV4             Linux guest address [10.0.3.3]");
    println!("  --net-prefix IPV4            connected route prefix [10.0.3.0]");
    println!("  --netmask IPV4               Linux guest netmask [255.255.255.0]");
    println!("  --iface NAME                 Linux guest interface [eth0]");
    println!("  --server-mac MAC             RTOS static ARP MAC [52:54:00:aa:03:02]");
    println!("  --input-size N               square model input [640]");
    println!("  --target-class ID            COCO class, -1 selects best object [32]");
    println!("  --conf FLOAT                 confidence threshold [0.25]");
    println!("  --nms FLOAT                  NMS IoU threshold [0.45]");
    println!("  --threads N                  ONNX Runtime CPU threads [1]");
    println!("  --connect-timeout-ms MS      TCP timeout [1000]");
    println!("  --connect-retries N          startup/reconnect attempts [120]");
    println!("  --connect-retry-delay-ms MS  retry interval [1000]");
    println!("  --dry-run                    infer without AICP network traffic");
    println!("  --no-net-config              skip Linux network ioctl setup");
    println!("  --no-static-arp              skip static ARP setup");
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value for {name}"))
}

fn parse_number<T: std::str::FromStr>(text: String, name: &str) -> Result<T, String> {
    text.parse().map_err(|_| format!("invalid {name}: {text}"))
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = env::args();
    let program = args
        .next()
        .unwrap_or_else(|| "aicp-yolov8-rust-onnx".into());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                usage(&program);
                std::process::exit(0);
            }
            "--model" => options.model = next_arg(&mut args, &arg)?,
            "--labels" => options.labels = next_arg(&mut args, &arg)?,
            "--image" => options.image = Some(next_arg(&mut args, &arg)?),
            "--image-list" => options.image_list = next_arg(&mut args, &arg)?,
            "--aicp-host" => options.host = next_arg(&mut args, &arg)?,
            "--aicp-port" => {
                options.port = parse_number(next_arg(&mut args, &arg)?, &arg)?;
            }
            "--client-ip" => options.client_ip = next_arg(&mut args, &arg)?,
            "--net-prefix" => options.net_prefix = next_arg(&mut args, &arg)?,
            "--netmask" => options.netmask = next_arg(&mut args, &arg)?,
            "--iface" => options.iface = next_arg(&mut args, &arg)?,
            "--server-mac" => options.server_mac = next_arg(&mut args, &arg)?,
            "--input-size" => {
                options.input_size = parse_number(next_arg(&mut args, &arg)?, &arg)?;
            }
            "--target-class" => {
                options.target_class = parse_number(next_arg(&mut args, &arg)?, &arg)?;
            }
            "--threads" => {
                options.threads = parse_number(next_arg(&mut args, &arg)?, &arg)?;
            }
            "--connect-timeout-ms" => {
                options.connect_timeout_ms = parse_number(next_arg(&mut args, &arg)?, &arg)?;
            }
            "--connect-retries" => {
                options.connect_retries = parse_number(next_arg(&mut args, &arg)?, &arg)?;
            }
            "--connect-retry-delay-ms" => {
                options.connect_retry_delay_ms = parse_number(next_arg(&mut args, &arg)?, &arg)?;
            }
            "--conf" => {
                options.confidence_threshold = parse_number(next_arg(&mut args, &arg)?, &arg)?;
            }
            "--nms" => {
                options.nms_threshold = parse_number(next_arg(&mut args, &arg)?, &arg)?;
            }
            "--dry-run" => options.dry_run = true,
            "--no-net-config" => options.configure_network = false,
            "--no-static-arp" => options.static_arp = false,
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    if options.input_size < 32 || options.threads < 1 || options.connect_retries == 0 {
        return Err("input size, thread count, and retry count must be positive".into());
    }
    if !(0.0..=1.0).contains(&options.confidence_threshold)
        || !(0.0..=1.0).contains(&options.nms_threshold)
    {
        return Err("confidence and NMS thresholds must be within [0, 1]".into());
    }
    Ok(options)
}

fn mount_one(source: &str, target: &str, fs_type: &str, flags: c_ulong) -> io::Result<()> {
    let source = CString::new(source).map_err(|_| invalid_input("bad mount source"))?;
    let target = CString::new(target).map_err(|_| invalid_input("bad mount target"))?;
    let fs_type = CString::new(fs_type).map_err(|_| invalid_input("bad mount fs type"))?;
    let ret = unsafe {
        mount(
            source.as_ptr(),
            target.as_ptr(),
            fs_type.as_ptr(),
            flags,
            ptr::null(),
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(16) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

fn ensure_virtual_filesystems() {
    let _ = fs::create_dir_all("/proc");
    let _ = fs::create_dir_all("/sys");
    for (source, target, fs_type) in [("proc", "/proc", "proc"), ("sysfs", "/sys", "sysfs")] {
        match mount_one(source, target, fs_type, MS_NOSUID | MS_NODEV | MS_NOEXEC) {
            Ok(()) => println!("AICP_YOLO_RUST_MOUNT path={target} ret=0"),
            Err(error) => println!("AICP_YOLO_RUST_MOUNT path={target} ret=-1 error={error}"),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InAddr {
    bytes: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddr {
    family: u16,
    data: [c_char; 14],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrIn {
    family: u16,
    port: u16,
    addr: InAddr,
    zero: [u8; 8],
}

#[repr(C)]
struct IfReqAddr {
    name: [c_char; 16],
    addr: SockAddrIn,
}

#[repr(C)]
struct IfReqFlags {
    name: [c_char; 16],
    flags: c_short,
    padding: [u8; 22],
}

#[repr(C)]
struct RtEntry {
    pad1: c_ulong,
    dst: SockAddr,
    gateway: SockAddr,
    genmask: SockAddr,
    flags: u16,
    pad2: c_short,
    pad3: c_ulong,
    pad4: *mut u8,
    metric: c_short,
    dev: *mut c_char,
    mtu: c_ulong,
    window: c_ulong,
    irtt: u16,
}

#[repr(C)]
struct ArpReq {
    protocol_addr: SockAddr,
    hardware_addr: SockAddr,
    flags: c_int,
    netmask: SockAddr,
    dev: [c_char; 16],
}

fn ifname_array(name: &str) -> [c_char; 16] {
    let mut out = [0 as c_char; 16];
    for (index, byte) in name.bytes().take(15).enumerate() {
        out[index] = byte as c_char;
    }
    out
}

fn ipv4_octets(address: &str) -> io::Result<[u8; 4]> {
    address
        .parse::<std::net::Ipv4Addr>()
        .map(|address| address.octets())
        .map_err(|_| invalid_input("invalid IPv4 address"))
}

fn sockaddr_in(address: &str) -> io::Result<SockAddrIn> {
    Ok(SockAddrIn {
        family: AF_INET as u16,
        port: 0,
        addr: InAddr {
            bytes: ipv4_octets(address)?,
        },
        zero: [0; 8],
    })
}

fn sockaddr_raw(address: &str) -> io::Result<SockAddr> {
    let mut data = [0 as c_char; 14];
    for (index, byte) in ipv4_octets(address)?.iter().copied().enumerate() {
        data[index + 2] = byte as c_char;
    }
    Ok(SockAddr {
        family: AF_INET as u16,
        data,
    })
}

fn parse_mac(address: &str) -> io::Result<[u8; 6]> {
    let parts: Vec<_> = address.split(':').collect();
    if parts.len() != 6 {
        return Err(invalid_input("invalid MAC address"));
    }
    let mut out = [0u8; 6];
    for (index, part) in parts.iter().enumerate() {
        out[index] =
            u8::from_str_radix(part, 16).map_err(|_| invalid_input("invalid MAC address"))?;
    }
    Ok(out)
}

fn set_interface_address(
    fd: c_int,
    iface: &str,
    request: c_ulong,
    address: &str,
) -> io::Result<()> {
    let mut value = IfReqAddr {
        name: ifname_array(iface),
        addr: sockaddr_in(address)?,
    };
    if unsafe { ioctl(fd, request, &mut value) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_interface_up(fd: c_int, iface: &str) -> io::Result<()> {
    let mut value = IfReqFlags {
        name: ifname_array(iface),
        flags: 0,
        padding: [0; 22],
    };
    if unsafe { ioctl(fd, SIOCGIFFLAGS, &mut value) } != 0 {
        return Err(io::Error::last_os_error());
    }
    value.flags |= IFF_UP;
    if unsafe { ioctl(fd, SIOCSIFFLAGS, &mut value) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn add_connected_route(fd: c_int, options: &Options) -> io::Result<()> {
    let mut dev = ifname_array(&options.iface);
    let mut route = RtEntry {
        pad1: 0,
        dst: sockaddr_raw(&options.net_prefix)?,
        gateway: sockaddr_raw("0.0.0.0")?,
        genmask: sockaddr_raw(&options.netmask)?,
        flags: RTF_UP,
        pad2: 0,
        pad3: 0,
        pad4: ptr::null_mut(),
        metric: 0,
        dev: dev.as_mut_ptr(),
        mtu: 0,
        window: 0,
        irtt: 0,
    };
    if unsafe { ioctl(fd, SIOCADDRT, &mut route) } == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(17) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

fn add_static_arp(fd: c_int, options: &Options) -> io::Result<()> {
    let mac = parse_mac(&options.server_mac)?;
    let mut request = ArpReq {
        protocol_addr: sockaddr_raw(&options.host)?,
        hardware_addr: SockAddr {
            family: ARPHRD_ETHER,
            data: [0; 14],
        },
        flags: ATF_COM | ATF_PERM,
        netmask: SockAddr {
            family: 0,
            data: [0; 14],
        },
        dev: ifname_array(&options.iface),
    };
    for (index, byte) in mac.iter().copied().enumerate() {
        request.hardware_addr.data[index] = byte as c_char;
    }
    if unsafe { ioctl(fd, SIOCSARP, &mut request) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn configure_network_once(options: &Options) -> io::Result<()> {
    let fd = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        set_interface_address(fd, &options.iface, SIOCSIFADDR, &options.client_ip)?;
        println!(
            "AICP_YOLO_RUST_NETCFG step=SIOCSIFADDR ret=0 iface={} address={}",
            options.iface, options.client_ip
        );
        set_interface_address(fd, &options.iface, SIOCSIFNETMASK, &options.netmask)?;
        println!(
            "AICP_YOLO_RUST_NETCFG step=SIOCSIFNETMASK ret=0 netmask={}",
            options.netmask
        );
        set_interface_up(fd, &options.iface)?;
        println!("AICP_YOLO_RUST_NETCFG step=SIOCSIFFLAGS ret=0");
        add_connected_route(fd, options)?;
        println!(
            "AICP_YOLO_RUST_NETCFG step=SIOCADDRT ret=0 prefix={}",
            options.net_prefix
        );
        if options.static_arp {
            add_static_arp(fd, options)?;
            println!(
                "AICP_YOLO_RUST_NETCFG step=SIOCSARP ret=0 server={} mac={}",
                options.host, options.server_mac
            );
        }
        Ok(())
    })();
    unsafe { close(fd) };
    result
}

fn configure_network(options: &Options) -> io::Result<()> {
    if !options.configure_network {
        println!("AICP_YOLO_RUST_NETCFG skipped=1");
        return Ok(());
    }
    let mut last_error = None;
    for attempt in 1..=40 {
        match configure_network_once(options) {
            Ok(()) => return Ok(()),
            Err(error) => {
                println!("AICP_YOLO_RUST_NETCFG attempt={attempt} ret=-1 error={error}");
                last_error = Some(error);
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("network setup failed")))
}

fn load_image(path: &str) -> io::Result<Image> {
    let path_c = CString::new(path).map_err(|_| invalid_input("image path contains a NUL byte"))?;
    let mut width = 0;
    let mut height = 0;
    let mut error = [0 as c_char; 512];
    let data = unsafe {
        aicp_image_load_rgb(
            path_c.as_ptr(),
            &mut width,
            &mut height,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    if data.is_null() || width <= 0 || height <= 0 {
        return Err(ffi_error("decode JPEG", &error));
    }
    let len = width as usize * height as usize * 3;
    let rgb = unsafe { slice::from_raw_parts(data, len) }.to_vec();
    unsafe { aicp_image_free(data) };
    Ok(Image {
        width: width as usize,
        height: height as usize,
        rgb,
    })
}

fn resize_bilinear_rgb(image: &Image, width: usize, height: usize) -> Vec<u8> {
    let mut output = vec![114u8; width * height * 3];
    let scale_x = image.width as f32 / width as f32;
    let scale_y = image.height as f32 / height as f32;
    for y in 0..height {
        let fy = (y as f32 + 0.5) * scale_y - 0.5;
        let y0 = fy.floor().max(0.0) as usize;
        let y1 = (y0 + 1).min(image.height - 1);
        let weight_y = fy - y0 as f32;
        for x in 0..width {
            let fx = (x as f32 + 0.5) * scale_x - 0.5;
            let x0 = fx.floor().max(0.0) as usize;
            let x1 = (x0 + 1).min(image.width - 1);
            let weight_x = fx - x0 as f32;
            for channel in 0..3 {
                let pixel = |px: usize, py: usize| -> f32 {
                    image.rgb[(py * image.width + px) * 3 + channel] as f32
                };
                let top = pixel(x0, y0) + (pixel(x1, y0) - pixel(x0, y0)) * weight_x;
                let bottom = pixel(x0, y1) + (pixel(x1, y1) - pixel(x0, y1)) * weight_x;
                output[(y * width + x) * 3 + channel] =
                    (top + (bottom - top) * weight_y).clamp(0.0, 255.0) as u8;
            }
        }
    }
    output
}

fn preprocess(image: &Image, input_size: usize) -> (Vec<f32>, Letterbox) {
    let scale =
        (input_size as f32 / image.width as f32).min(input_size as f32 / image.height as f32);
    let resized_width = ((image.width as f32 * scale).round() as usize).clamp(1, input_size);
    let resized_height = ((image.height as f32 * scale).round() as usize).clamp(1, input_size);
    let pad_x = (input_size - resized_width) / 2;
    let pad_y = (input_size - resized_height) / 2;
    let resized = resize_bilinear_rgb(image, resized_width, resized_height);
    let mut chw = vec![114.0 / 255.0; 3 * input_size * input_size];
    for y in 0..resized_height {
        for x in 0..resized_width {
            for channel in 0..3 {
                let source = resized[(y * resized_width + x) * 3 + channel];
                let destination =
                    channel * input_size * input_size + (y + pad_y) * input_size + x + pad_x;
                chw[destination] = source as f32 / 255.0;
            }
        }
    }
    (
        chw,
        Letterbox {
            scale,
            pad_x: pad_x as f32,
            pad_y: pad_y as f32,
        },
    )
}

fn intersection_over_union(a: Detection, b: Detection) -> f32 {
    let left = a.left.max(b.left);
    let top = a.top.max(b.top);
    let right = a.right.min(b.right);
    let bottom = a.bottom.min(b.bottom);
    let intersection = (right - left).max(0.0) * (bottom - top).max(0.0);
    let area_a = (a.right - a.left).max(0.0) * (a.bottom - a.top).max(0.0);
    let area_b = (b.right - b.left).max(0.0) * (b.bottom - b.top).max(0.0);
    intersection / (area_a + area_b - intersection).max(1e-6)
}

fn non_maximum_suppression(mut detections: Vec<Detection>, threshold: f32) -> Vec<Detection> {
    detections.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept = Vec::new();
    for detection in detections {
        if kept.iter().all(|other: &Detection| {
            other.class_id != detection.class_id
                || intersection_over_union(*other, detection) <= threshold
        }) {
            kept.push(detection);
        }
    }
    kept
}

fn decode_yolov8(
    data: &[f32],
    shape: &[i64],
    letterbox: Letterbox,
    image: &Image,
    options: &Options,
) -> io::Result<Vec<Detection>> {
    if shape.len() != 3 || shape[0] != 1 || shape[1] <= 0 || shape[2] <= 0 {
        return Err(io::Error::other(format!(
            "unsupported YOLO output shape {shape:?}"
        )));
    }
    let channels_first = shape[1] < shape[2];
    let channels = if channels_first { shape[1] } else { shape[2] } as usize;
    let count = if channels_first { shape[2] } else { shape[1] } as usize;
    if channels < 84 || count == 0 || channels.saturating_mul(count) > data.len() {
        return Err(io::Error::other(format!(
            "invalid YOLO output shape {shape:?}"
        )));
    }
    let has_objectness = channels >= 85;
    let class_offset = if has_objectness { 5 } else { 4 };
    let class_count = channels - class_offset;
    let value_at = |row: usize, channel: usize| -> f32 {
        if channels_first {
            data[channel * count + row]
        } else {
            data[row * channels + channel]
        }
    };

    let mut detections = Vec::new();
    for row in 0..count {
        let objectness = if has_objectness {
            value_at(row, 4)
        } else {
            1.0
        };
        let mut best_class = -1;
        let mut best_score = f32::NEG_INFINITY;
        for class_id in 0..class_count {
            let score = objectness * value_at(row, class_offset + class_id);
            if score > best_score {
                best_class = class_id as i32;
                best_score = score;
            }
        }
        if best_score < options.confidence_threshold
            || (options.target_class >= 0 && best_class != options.target_class)
        {
            continue;
        }
        let center_x = value_at(row, 0);
        let center_y = value_at(row, 1);
        let width = value_at(row, 2);
        let height = value_at(row, 3);
        if !center_x.is_finite()
            || !center_y.is_finite()
            || !width.is_finite()
            || !height.is_finite()
            || width <= 0.0
            || height <= 0.0
        {
            continue;
        }
        detections.push(Detection {
            class_id: best_class,
            score: best_score,
            left: ((center_x - width * 0.5 - letterbox.pad_x) / letterbox.scale)
                .clamp(0.0, image.width as f32),
            top: ((center_y - height * 0.5 - letterbox.pad_y) / letterbox.scale)
                .clamp(0.0, image.height as f32),
            right: ((center_x + width * 0.5 - letterbox.pad_x) / letterbox.scale)
                .clamp(0.0, image.width as f32),
            bottom: ((center_y + height * 0.5 - letterbox.pad_y) / letterbox.scale)
                .clamp(0.0, image.height as f32),
        });
    }
    Ok(non_maximum_suppression(detections, options.nms_threshold))
}

fn map_detection_to_control(detections: &[Detection], image: &Image) -> ControlMapping {
    let Some(best) = detections.first().copied() else {
        return ControlMapping::default();
    };
    let width = (best.right - best.left).max(1.0);
    let height = (best.bottom - best.top).max(1.0);
    let center_x = best.left + width * 0.5;
    let center_y = best.top + height * 0.5;
    let x_error = center_x / image.width as f32 - 0.5;
    let y_error = 0.5 - center_y / image.height as f32;
    let area_ratio = (width * height / (image.width * image.height) as f32).clamp(0.0, 1.0);
    let confidence = best.score.clamp(0.0, 1.0);
    ControlMapping {
        has_detection: true,
        class_id: best.class_id,
        confidence,
        left: best.left,
        top: best.top,
        right: best.right,
        bottom: best.bottom,
        control: ControlPayload {
            target: (x_error * 2.0).clamp(-1.0, 1.0),
            kp: 0.45 + 0.35 * confidence,
            ki: 0.02 + 0.10 * area_ratio,
            kd: 0.02 + 0.10 * x_error.abs(),
            feed_forward: (y_error * 0.35).clamp(-0.25, 0.25),
            mode: 4,
        },
    }
}

fn make_header(msg_type: u8, flags: u16, payload_len: u32, seq: u32) -> AicpHeader {
    AicpHeader::new(
        msg_type,
        flags,
        payload_len,
        seq,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
        ERROR_OK,
    )
}

fn control_bytes(control: ControlPayload) -> [u8; CONTROL_PAYLOAD_LEN] {
    encode_control_payload(control)
}

fn status_from_bytes(payload: &[u8]) -> io::Result<StatusPayload> {
    let wire: &[u8; STATUS_PAYLOAD_LEN] = payload
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad STATUS payload"))?;
    Ok(decode_status_payload(wire))
}

struct AicpClient<'a> {
    options: &'a Options,
    stream: Option<TcpStream>,
    sequence: u32,
}

impl<'a> AicpClient<'a> {
    fn new(options: &'a Options) -> Self {
        Self {
            options,
            stream: None,
            sequence: 1,
        }
    }

    fn connect(&mut self) -> io::Result<()> {
        self.stream = None;
        let address = format!("{}:{}", self.options.host, self.options.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| invalid_input("AICP host did not resolve"))?;
        let timeout = Duration::from_millis(self.options.connect_timeout_ms);
        let mut stream = TcpStream::connect_timeout(&address, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let hello = b"{\"role\":\"yolov8-rust-onnx-cpu\",\"model\":\"yolov8n.onnx\",\"cap\":\"control,status\"}\0";
        let header = make_header(AICP_MSG_HELLO, 0, hello.len() as u32, self.sequence);
        self.sequence = self.sequence.wrapping_add(1);
        send_frame(&mut stream, header, hello)?;
        self.stream = Some(stream);
        Ok(())
    }

    fn ensure_connected(&mut self) -> io::Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let mut last_error = None;
        for attempt in 1..=self.options.connect_retries {
            match self.connect() {
                Ok(()) => {
                    println!("AICP_YOLO_RUST_CONNECTED attempt={attempt}");
                    return Ok(());
                }
                Err(error) => {
                    println!("AICP_YOLO_RUST_CONNECT_RETRY attempt={attempt} error={error}");
                    last_error = Some(error);
                    if attempt < self.options.connect_retries {
                        thread::sleep(Duration::from_millis(self.options.connect_retry_delay_ms));
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| io::Error::other("AICP connect failed")))
    }

    fn transact_once(&mut self, control: ControlPayload) -> io::Result<(StatusPayload, u64)> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "AICP disconnected"))?;
        let payload = control_bytes(control);
        let request_seq = self.sequence;
        let header = make_header(
            AICP_MSG_CONTROL_SET,
            AICP_FLAG_ACK_REQUIRED,
            payload.len() as u32,
            request_seq,
        );
        self.sequence = self.sequence.wrapping_add(1);
        let start = Instant::now();
        send_frame(stream, header, &payload)?;
        let (response, response_payload) = receive_frame(stream)?;
        let rtt_ns = start.elapsed().as_nanos() as u64;
        if response.msg_type == AICP_MSG_ERROR {
            return Err(io::Error::other(format!(
                "RTOS AICP ERROR code={}",
                response.error_code
            )));
        }
        if response.msg_type != AICP_MSG_STATUS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected STATUS",
            ));
        }
        if response.seq != request_seq {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "AICP response sequence mismatch: request={request_seq} response={}",
                    response.seq
                ),
            ));
        }
        let status = status_from_bytes(&response_payload)?;
        if status.applied_seq != request_seq {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "RTOS applied sequence mismatch: request={request_seq} applied={}",
                    status.applied_seq
                ),
            ));
        }
        Ok((status, rtt_ns))
    }

    fn transact(&mut self, control: ControlPayload) -> io::Result<(StatusPayload, u64)> {
        self.ensure_connected()?;
        match self.transact_once(control) {
            Ok(result) => Ok(result),
            Err(first_error) => {
                println!("AICP_YOLO_RUST_RECONNECT reason={first_error}");
                self.stream = None;
                self.ensure_connected()?;
                self.transact_once(control)
            }
        }
    }
}

fn load_lines(path: &str) -> io::Result<Vec<String>> {
    Ok(fs::read_to_string(path)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned)
        .collect())
}

fn image_paths(options: &Options) -> io::Result<Vec<String>> {
    if let Some(image) = &options.image {
        return Ok(vec![image.clone()]);
    }
    load_lines(&options.image_list)
}

fn resolve_guest_path(list_path: &str, image_path: &str) -> String {
    if Path::new(image_path).is_absolute() {
        return image_path.into();
    }
    let rooted = Path::new("/").join(image_path);
    if rooted.exists() {
        return rooted.to_string_lossy().into_owned();
    }
    if Path::new(image_path).exists() {
        return image_path.into();
    }
    Path::new(list_path)
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .join(image_path)
        .to_string_lossy()
        .into_owned()
}

fn idle_if_pid1() -> ! {
    println!("AICP_YOLO_RUST_IDLE pid=1 reason=linux_init_must_not_exit");
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

fn run(options: &Options) -> io::Result<bool> {
    let paths = image_paths(options)?;
    let labels = load_lines(&options.labels).unwrap_or_default();
    println!(
        "AICP_YOLO_RUST_BEGIN model={} images={} host={} port={} dry_run={} target_class={} \
         backend=onnxruntime-cpu language=rust",
        options.model,
        paths.len(),
        options.host,
        options.port,
        u8::from(options.dry_run),
        options.target_class
    );

    if !options.dry_run {
        configure_network(options)?;
    }
    let session = OrtSession::create(&options.model, options.threads)?;

    let mut ok = 0u32;
    let mut failed = 0u32;
    let mut inference_sum = 0u64;
    let mut inference_max = 0u64;
    let mut load_sum = 0u64;
    let mut preprocess_sum = 0u64;
    let mut postprocess_sum = 0u64;
    let mut rtt_sum = 0u64;
    let mut rtt_max = 0u64;
    let mut e2e_sum = 0u64;
    let mut e2e_max = 0u64;

    for listed_path in paths {
        let path = resolve_guest_path(&options.image_list, &listed_path);
        let result = (|| -> io::Result<()> {
            let e2e_start = Instant::now();
            let load_start = Instant::now();
            let image = load_image(&path)?;
            let load_ns = load_start.elapsed().as_nanos() as u64;
            let preprocess_start = Instant::now();
            let (mut input, letterbox) = preprocess(&image, options.input_size);
            let preprocess_ns = preprocess_start.elapsed().as_nanos() as u64;
            let input_shape = [1, 3, options.input_size as i64, options.input_size as i64];
            let inference_start = Instant::now();
            let output = session.run(&mut input, &input_shape)?;
            let inference_ns = inference_start.elapsed().as_nanos() as u64;
            let postprocess_start = Instant::now();
            inference_sum = inference_sum.saturating_add(inference_ns);
            inference_max = inference_max.max(inference_ns);
            let detections =
                decode_yolov8(output.data(), &output.shape, letterbox, &image, options)?;
            let mapping = map_detection_to_control(&detections, &image);
            let postprocess_ns = postprocess_start.elapsed().as_nanos() as u64;
            let label = labels
                .get(mapping.class_id.max(0) as usize)
                .map(String::as_str)
                .unwrap_or("");
            println!(
                "AICP_YOLO_RUST_RESULT image={} detections={} selected={} cls={} label={} \
                 score={:.3} box={:.1},{:.1},{:.1},{:.1} target={:.4} kp={:.4} ki={:.4} kd={:.4} \
                 feed_forward={:.4} mode={} infer_ns={}",
                path,
                detections.len(),
                u8::from(mapping.has_detection),
                mapping.class_id,
                label,
                mapping.confidence,
                mapping.left,
                mapping.top,
                mapping.right,
                mapping.bottom,
                mapping.control.target,
                mapping.control.kp,
                mapping.control.ki,
                mapping.control.kd,
                mapping.control.feed_forward,
                mapping.control.mode,
                inference_ns
            );
            let mut aicp_rtt_ns = 0u64;
            if !options.dry_run {
                // CPU inference can exceed the RTOS peer's idle receive
                // timeout. Create the short AICP session only after the
                // model result is available, so HELLO and CONTROL are
                // adjacent on the wire.
                let mut client = AicpClient::new(options);
                let (status, rtt_ns) = client.transact(mapping.control)?;
                aicp_rtt_ns = rtt_ns;
                rtt_sum = rtt_sum.saturating_add(rtt_ns);
                rtt_max = rtt_max.max(rtt_ns);
                println!(
                    "AICP_YOLO_RUST_CONTROL image={} applied_seq={} setpoint={:.4} measured={:.4} \
                     error={:.4} output={:.4} mode={} rtt_ns={}",
                    path,
                    status.applied_seq,
                    status.setpoint,
                    status.measured,
                    status.error,
                    status.control_output,
                    status.mode,
                    rtt_ns
                );
            }
            let e2e_ns = e2e_start.elapsed().as_nanos() as u64;
            load_sum = load_sum.saturating_add(load_ns);
            preprocess_sum = preprocess_sum.saturating_add(preprocess_ns);
            postprocess_sum = postprocess_sum.saturating_add(postprocess_ns);
            e2e_sum = e2e_sum.saturating_add(e2e_ns);
            e2e_max = e2e_max.max(e2e_ns);
            println!(
                "AICP_YOLO_RUST_STAGE image={} load_ns={} preprocess_ns={} infer_ns={} \
                 postprocess_ns={} aicp_rtt_ns={} e2e_ns={}",
                path, load_ns, preprocess_ns, inference_ns, postprocess_ns, aicp_rtt_ns, e2e_ns
            );
            Ok(())
        })();
        match result {
            Ok(()) => ok += 1,
            Err(error) => {
                failed += 1;
                println!("AICP_YOLO_RUST_FAIL image={path} error={error}");
            }
        }
    }

    let denominator = u64::from(ok.max(1));
    println!(
        "AICP_YOLO_RUST_DONE ok={} failed={} avg_load_ns={} avg_preprocess_ns={} avg_infer_ns={} \
         max_infer_ns={} avg_postprocess_ns={} avg_rtt_ns={} max_rtt_ns={} avg_e2e_ns={} \
         max_e2e_ns={}",
        ok,
        failed,
        load_sum / denominator,
        preprocess_sum / denominator,
        inference_sum / denominator,
        inference_max,
        postprocess_sum / denominator,
        rtt_sum / denominator,
        rtt_max,
        e2e_sum / denominator,
        e2e_max
    );
    Ok(failed == 0 && ok > 0)
}

fn main() {
    let options = match parse_args() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("AICP_YOLO_RUST_FATAL stage=args error={error}");
            std::process::exit(2);
        }
    };
    let is_pid1 = std::process::id() == 1;
    if is_pid1 && !options.dry_run {
        ensure_virtual_filesystems();
    }
    let success = match run(&options) {
        Ok(success) => success,
        Err(error) => {
            println!("AICP_YOLO_RUST_FATAL error={error}");
            false
        }
    };
    if is_pid1 && !options.dry_run {
        idle_if_pid1();
    }
    if !success {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nms_is_class_aware() {
        let a = Detection {
            class_id: 1,
            score: 0.9,
            left: 0.0,
            top: 0.0,
            right: 10.0,
            bottom: 10.0,
        };
        let mut b = a;
        b.score = 0.8;
        let mut c = b;
        c.class_id = 2;
        assert_eq!(non_maximum_suppression(vec![a, b, c], 0.5).len(), 2);
    }

    #[test]
    fn preprocess_keeps_fixed_shape_for_extreme_aspect_ratio() {
        let image = Image {
            width: 1000,
            height: 1,
            rgb: vec![127; 1000 * 3],
        };
        let (tensor, letterbox) = preprocess(&image, 640);
        assert_eq!(tensor.len(), 3 * 640 * 640);
        assert_eq!(letterbox.pad_x, 0.0);
        assert!(letterbox.pad_y >= 0.0);
    }

    #[test]
    fn decode_rejects_non_positive_output_dimensions() {
        let image = Image {
            width: 640,
            height: 640,
            rgb: Vec::new(),
        };
        let result = decode_yolov8(
            &[],
            &[1, -84, 8400],
            Letterbox {
                scale: 1.0,
                pad_x: 0.0,
                pad_y: 0.0,
            },
            &image,
            &Options::default(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn aicp_payloads_use_network_byte_order() {
        let control = ControlPayload {
            target: 1.0,
            kp: -2.0,
            ki: 0.5,
            kd: 3.25,
            feed_forward: -0.25,
            mode: 4,
        };
        assert_eq!(
            control_bytes(control),
            [
                0x3f, 0x80, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x3f, 0x00, 0x00, 0x00,
                0x40, 0x50, 0x00, 0x00, 0xbe, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04,
            ]
        );

        let status = status_from_bytes(&[
            0x3f, 0x80, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x3f, 0x00, 0x00, 0x00, 0x40,
            0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x02,
        ])
        .unwrap();
        assert_eq!(status.setpoint, 1.0);
        assert_eq!(status.measured, -2.0);
        assert_eq!(status.control_output, 0.5);
        assert_eq!(status.error, 3.25);
        assert_eq!(status.mode, 4);
        assert_eq!(status.applied_seq, 2);
    }

    #[test]
    fn packaged_assets_are_used_when_guest_rootfs_assets_are_unavailable() {
        assert_eq!(
            default_asset_path("/aicp-yolov8-test-missing/model.onnx", "model/model.onnx"),
            "model/model.onnx"
        );
    }
}
