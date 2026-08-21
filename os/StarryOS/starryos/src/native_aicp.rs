use core::time::Duration;

use ax_log::ax_println;
use ax_std::{
    io::prelude::*,
    net::{SocketAddr, SocketAddrV4, TcpStream, UdpSocket},
    thread,
    time::Instant,
};

const AICP_MAGIC: u16 = 0xA1C0;
const AICP_VERSION: u8 = 1;
const AICP_HEADER_LEN: usize = 32;
const AICP_MAX_PAYLOAD: usize = 4096;
const AICP_MSG_HELLO: u8 = 0x01;
const AICP_MSG_CONTROL_SET: u8 = 0x02;
const AICP_MSG_STATUS: u8 = 0x03;
const AICP_MSG_ERROR: u8 = 0x04;
const AICP_OK: u16 = 0;
const DEFAULT_ITERATIONS: usize = 40;
const DEFAULT_RETRIES: usize = 8;
const RECV_TIMEOUT: Duration = Duration::from_millis(800);
const RETRY_DELAY: Duration = Duration::from_millis(40);
const CONTROL_PERIOD: Duration = Duration::from_millis(20);
const START_DELAY: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy)]
enum Transport {
    Tcp,
    Udp,
}

impl Transport {
    fn from_build_env() -> Self {
        if option_env!("AICP_STARRY_TRANSPORT").unwrap_or("udp") == "tcp" {
            Self::Tcp
        } else {
            Self::Udp
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Header {
    magic: u16,
    version: u8,
    msg_type: u8,
    flags: u16,
    header_len: u16,
    payload_len: u32,
    seq: u32,
    timestamp_ns: u64,
    error_code: u16,
    crc16: u16,
    reserved: u32,
}

#[derive(Clone, Copy, Default)]
struct ControlPayload {
    target: f32,
    kp: f32,
    ki: f32,
    kd: f32,
    feed_forward: f32,
    mode: u32,
}

#[derive(Clone, Copy, Default)]
struct StatusPayload {
    setpoint: f32,
    measured: f32,
    control_output: f32,
    error: f32,
    mode: u32,
    applied_seq: u32,
}

struct NativeConfig {
    iterations: usize,
    retries: usize,
    mode_ai: bool,
    server: SocketAddr,
    transport: Transport,
}

pub fn maybe_run() -> bool {
    if option_env!("AICP_STARRY_NATIVE").unwrap_or("0") != "1" {
        return false;
    }

    let config = NativeConfig {
        iterations: parse_usize(option_env!("AICP_STARRY_ITERATIONS"), DEFAULT_ITERATIONS),
        retries: parse_usize(option_env!("AICP_STARRY_UDP_RETRIES"), DEFAULT_RETRIES),
        mode_ai: option_env!("AICP_STARRY_MODE").unwrap_or("ai") != "fixed",
        server: SocketAddr::V4(SocketAddrV4::new(
            parse_ipv4(option_env!("AICP_STARRY_SERVER").unwrap_or("10.0.3.2")),
            parse_u16(option_env!("AICP_STARRY_SERVER_PORT"), 8800),
        )),
        transport: Transport::from_build_env(),
    };

    ax_println!(
        "AICP_STARRY_NATIVE_SPAWN target={} transport={} iterations={} mode={} retries={} \
         delay_ms=1500",
        config.server,
        config.transport.name(),
        config.iterations,
        if config.mode_ai { "ai" } else { "fixed" },
        config.retries
    );

    wait_for(START_DELAY);
    run_delayed(config);
    true
}

fn run_delayed(config: NativeConfig) {
    ax_println!(
        "AICP_STARRY_NATIVE_START target={} transport={} iterations={} mode={} retries={}",
        config.server,
        config.transport.name(),
        config.iterations,
        if config.mode_ai { "ai" } else { "fixed" },
        config.retries
    );

    log_peer_neighbor_discovery();

    match run_client(&config) {
        Ok(summary) => ax_println!(
            "AICP_STARRY_DONE ok={} failed=0 avg_rtt_ns={} max_rtt_ns={}",
            summary.ok,
            summary.avg_rtt_ns,
            summary.max_rtt_ns
        ),
        Err(err) => {
            ax_println!("AICP_STARRY_NATIVE_ERROR {err}");
            ax_println!("AICP_STARRY_DONE ok=0 failed=1 avg_rtt_ns=0 max_rtt_ns=0");
        }
    }
}

struct Summary {
    ok: usize,
    avg_rtt_ns: u128,
    max_rtt_ns: u128,
}

fn wait_for(duration: Duration) {
    let start = Instant::now();
    while Instant::now().duration_since(start) < duration {
        thread::yield_now();
    }
}

fn run_client(config: &NativeConfig) -> Result<Summary, &'static str> {
    match config.transport {
        Transport::Tcp => run_tcp_client(config),
        Transport::Udp => run_udp_client(config),
    }
}

fn run_udp_client(config: &NativeConfig) -> Result<Summary, &'static str> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|_| "bind")?;
    socket.set_nonblocking(true).map_err(|_| "nonblocking")?;
    ax_println!("AICP_STARRY_NATIVE_BOUND local={:?}", socket.local_addr());

    let hello_payload = b"{\"guest\":\"StarryOS\",\"transport\":\"udp\",\"role\":\"ai-control\"}";
    let hello = make_header(AICP_MSG_HELLO, hello_payload.len(), 1, AICP_OK);
    let (_hdr, status, hello_rtt) = transact(
        &socket,
        config.server,
        hello,
        hello_payload,
        1,
        config.retries,
    )?;
    ax_println!(
        "AICP_STARRY_NATIVE_HELLO rtt_ns={} measured={:.4} error={:.4}",
        hello_rtt,
        status.measured,
        status.error
    );

    let mut ok = 0usize;
    let mut total_rtt_ns = 0u128;
    let mut max_rtt_ns = 0u128;
    let mut last_status = status;
    for i in 0..config.iterations {
        let seq = (i + 2) as u32;
        let control = make_control(seq, last_status, config.mode_ai);
        let payload = control_to_payload(control);
        let hdr = make_header(AICP_MSG_CONTROL_SET, payload.len(), seq, AICP_OK);
        let (_rx_hdr, status, rtt_ns) =
            transact(&socket, config.server, hdr, &payload, seq, config.retries)?;
        ax_println!(
            "AICP_STARRY_NATIVE_STATUS seq={} rtt_ns={} setpoint={:.4} measured={:.4} \
             output={:.4} error={:.4} mode={} applied_seq={}",
            seq,
            rtt_ns,
            status.setpoint,
            status.measured,
            status.control_output,
            status.error,
            status.mode,
            status.applied_seq
        );
        ok += 1;
        total_rtt_ns += rtt_ns;
        max_rtt_ns = max_rtt_ns.max(rtt_ns);
        last_status = status;
        wait_for(CONTROL_PERIOD);
    }

    Ok(Summary {
        ok,
        avg_rtt_ns: if ok == 0 {
            0
        } else {
            total_rtt_ns / ok as u128
        },
        max_rtt_ns,
    })
}

fn run_tcp_client(config: &NativeConfig) -> Result<Summary, &'static str> {
    let mut stream = connect_tcp(config)?;
    let hello_payload = b"{\"guest\":\"StarryOS\",\"transport\":\"tcp\",\"role\":\"ai-control\"}";
    let hello = make_header(AICP_MSG_HELLO, hello_payload.len(), 1, AICP_OK);
    tcp_send_frame(&mut stream, hello, hello_payload)?;
    ax_println!("AICP_STARRY_NATIVE_HELLO transport=tcp seq=1");

    let mut ok = 0usize;
    let mut total_rtt_ns = 0u128;
    let mut max_rtt_ns = 0u128;
    let mut last_status = StatusPayload::default();
    for i in 0..config.iterations {
        let seq = (i + 2) as u32;
        let control = make_control(seq, last_status, config.mode_ai);
        let payload = control_to_payload(control);
        let hdr = make_header(AICP_MSG_CONTROL_SET, payload.len(), seq, AICP_OK);
        let start = Instant::now();

        let status = match tcp_transact(&mut stream, hdr, &payload, seq) {
            Ok(status) => status,
            Err(_) => {
                ax_println!("AICP_STARRY_NATIVE_TCP_RECONNECT seq={seq}");
                stream = connect_tcp(config)?;
                tcp_send_frame(&mut stream, hello, hello_payload)?;
                tcp_transact(&mut stream, hdr, &payload, seq)?
            }
        };
        let rtt_ns = Instant::now().duration_since(start).as_nanos();
        ax_println!(
            "AICP_STARRY_NATIVE_STATUS seq={} rtt_ns={} setpoint={:.4} measured={:.4} \
             output={:.4} error={:.4} mode={} applied_seq={}",
            seq,
            rtt_ns,
            status.setpoint,
            status.measured,
            status.control_output,
            status.error,
            status.mode,
            status.applied_seq
        );
        ok += 1;
        total_rtt_ns += rtt_ns;
        max_rtt_ns = max_rtt_ns.max(rtt_ns);
        last_status = status;
        wait_for(CONTROL_PERIOD);
    }

    let _ = stream.shutdown();
    Ok(Summary {
        ok,
        avg_rtt_ns: if ok == 0 {
            0
        } else {
            total_rtt_ns / ok as u128
        },
        max_rtt_ns,
    })
}

fn connect_tcp(config: &NativeConfig) -> Result<TcpStream, &'static str> {
    for attempt in 0..config.retries {
        match TcpStream::connect(config.server) {
            Ok(stream) => {
                ax_println!(
                    "AICP_STARRY_NATIVE_TCP_CONNECTED target={} attempt={} local={:?}",
                    config.server,
                    attempt + 1,
                    stream.local_addr()
                );
                return Ok(stream);
            }
            Err(_) => {
                ax_println!(
                    "AICP_STARRY_NATIVE_TCP_CONNECT_RETRY target={} attempt={}",
                    config.server,
                    attempt + 1
                );
                wait_for(RETRY_DELAY);
            }
        }
    }
    Err("tcp-connect")
}

fn tcp_transact(
    stream: &mut TcpStream,
    hdr: Header,
    payload: &[u8],
    expected_seq: u32,
) -> Result<StatusPayload, &'static str> {
    tcp_send_frame(stream, hdr, payload)?;
    let (rx_hdr, rx_payload) = tcp_recv_frame(stream)?;
    if rx_hdr.seq != expected_seq {
        return Err("tcp-sequence");
    }
    if rx_hdr.msg_type == AICP_MSG_ERROR || rx_hdr.error_code != AICP_OK {
        return Err("tcp-peer-error");
    }
    if rx_hdr.msg_type != AICP_MSG_STATUS {
        return Err("tcp-message-type");
    }
    status_from_payload(&rx_payload[..rx_hdr.payload_len as usize]).ok_or("tcp-status-payload")
}

fn tcp_send_frame(stream: &mut TcpStream, hdr: Header, payload: &[u8]) -> Result<(), &'static str> {
    let (frame, frame_len) = encode_frame(hdr, payload)?;
    stream
        .write_all(&frame[..frame_len])
        .map_err(|_| "tcp-send")
}

fn tcp_recv_frame(
    stream: &mut TcpStream,
) -> Result<(Header, [u8; AICP_MAX_PAYLOAD]), &'static str> {
    let mut header_wire = [0u8; AICP_HEADER_LEN];
    stream
        .read_exact(&mut header_wire)
        .map_err(|_| "tcp-recv-header")?;
    let hdr = header_from_wire(&header_wire);
    if hdr.magic != AICP_MAGIC
        || hdr.version != AICP_VERSION
        || hdr.header_len as usize != AICP_HEADER_LEN
    {
        return Err("tcp-header");
    }
    if hdr.payload_len as usize > AICP_MAX_PAYLOAD {
        return Err("tcp-payload-too-large");
    }

    let mut payload = [0u8; AICP_MAX_PAYLOAD];
    let payload_len = hdr.payload_len as usize;
    stream
        .read_exact(&mut payload[..payload_len])
        .map_err(|_| "tcp-recv-payload")?;
    if frame_crc(hdr, &payload[..payload_len]) != hdr.crc16 {
        return Err("tcp-crc");
    }
    Ok((hdr, payload))
}

fn transact(
    socket: &UdpSocket,
    server: SocketAddr,
    hdr: Header,
    payload: &[u8],
    expected_seq: u32,
    retries: usize,
) -> Result<(Header, StatusPayload, u128), &'static str> {
    let (frame, frame_len) = encode_frame(hdr, payload)?;
    let mut rx = [0u8; AICP_HEADER_LEN + AICP_MAX_PAYLOAD];
    for attempt in 0..retries {
        let start = Instant::now();
        let sent = socket
            .send_to(&frame[..frame_len], server)
            .map_err(|_| "send")?;
        ax_println!(
            "AICP_STARRY_NATIVE_TX seq={} type={} attempt={} bytes={}",
            expected_seq,
            hdr.msg_type,
            attempt + 1,
            sent
        );
        while Instant::now().duration_since(start) < RECV_TIMEOUT {
            match socket.recv_from(&mut rx) {
                Ok((len, peer)) => {
                    if peer != server {
                        ax_println!("AICP_STARRY_NATIVE_DROP unexpected_peer={peer}");
                        continue;
                    }
                    let (rx_hdr, status) = parse_status_datagram(&rx[..len])?;
                    if rx_hdr.seq != expected_seq {
                        ax_println!(
                            "AICP_STARRY_NATIVE_DROP unexpected_seq={} expected={}",
                            rx_hdr.seq,
                            expected_seq
                        );
                        continue;
                    }
                    if rx_hdr.error_code != AICP_OK {
                        return Err("peer_error");
                    }
                    return Ok((
                        rx_hdr,
                        status,
                        Instant::now().duration_since(start).as_nanos(),
                    ));
                }
                Err(_) => wait_for(RETRY_DELAY),
            }
        }
        ax_println!(
            "AICP_STARRY_NATIVE_TIMEOUT seq={} attempt={}",
            expected_seq,
            attempt + 1
        );
    }
    Err("timeout")
}

fn make_control(seq: u32, status: StatusPayload, mode_ai: bool) -> ControlPayload {
    let phase = (seq % 40) as f32 / 40.0;
    let target = if phase < 0.5 { 0.82 } else { 0.28 };
    let error = target - status.measured;

    if mode_ai {
        let hidden0 = relu(0.85 * error + 0.25 * status.control_output + 0.12);
        let hidden1 = relu(-0.40 * error + 0.18 * status.measured + 0.05);
        let gain = (0.55 + 0.30 * hidden0 - 0.08 * hidden1).clamp(0.35, 0.95);
        ControlPayload {
            target,
            kp: gain,
            ki: 0.08 + 0.03 * hidden0,
            kd: 0.015 + 0.01 * hidden1,
            feed_forward: (0.05 * target + 0.03 * error).clamp(-0.1, 0.1),
            mode: 1,
        }
    } else {
        ControlPayload {
            target,
            kp: 0.45,
            ki: 0.04,
            kd: 0.01,
            feed_forward: 0.0,
            mode: 0,
        }
    }
}

fn relu(value: f32) -> f32 {
    if value > 0.0 { value } else { 0.0 }
}

fn make_header(msg_type: u8, payload_len: usize, seq: u32, error_code: u16) -> Header {
    Header {
        magic: AICP_MAGIC,
        version: AICP_VERSION,
        msg_type,
        flags: 0,
        header_len: AICP_HEADER_LEN as u16,
        payload_len: payload_len as u32,
        seq,
        timestamp_ns: Instant::now().elapsed().as_nanos() as u64,
        error_code,
        crc16: 0,
        reserved: 0,
    }
}

fn encode_frame(
    mut hdr: Header,
    payload: &[u8],
) -> Result<([u8; AICP_HEADER_LEN + 64], usize), &'static str> {
    if payload.len() > 64 {
        return Err("payload_too_large");
    }
    hdr.magic = AICP_MAGIC;
    hdr.version = AICP_VERSION;
    hdr.header_len = AICP_HEADER_LEN as u16;
    hdr.payload_len = payload.len() as u32;
    hdr.crc16 = frame_crc(hdr, payload);
    let mut out = [0u8; AICP_HEADER_LEN + 64];
    header_to_wire(hdr, &mut out[..AICP_HEADER_LEN]);
    out[AICP_HEADER_LEN..AICP_HEADER_LEN + payload.len()].copy_from_slice(payload);
    Ok((out, AICP_HEADER_LEN + payload.len()))
}

fn parse_status_datagram(buf: &[u8]) -> Result<(Header, StatusPayload), &'static str> {
    if buf.len() < AICP_HEADER_LEN {
        return Err("short_header");
    }
    let hdr = header_from_wire(&buf[..AICP_HEADER_LEN]);
    if hdr.magic != AICP_MAGIC || hdr.version != AICP_VERSION {
        return Err("bad_header");
    }
    if hdr.msg_type == AICP_MSG_ERROR {
        return Err("error_frame");
    }
    if hdr.msg_type != AICP_MSG_STATUS {
        return Err("bad_type");
    }
    let len = hdr.payload_len as usize;
    if hdr.header_len as usize != AICP_HEADER_LEN || buf.len() != AICP_HEADER_LEN + len {
        return Err("bad_len");
    }
    let payload = &buf[AICP_HEADER_LEN..];
    if hdr.crc16 != frame_crc(hdr, payload) {
        return Err("bad_crc");
    }
    let status = status_from_payload(payload).ok_or("bad_status")?;
    Ok((hdr, status))
}

fn header_to_wire(hdr: Header, out: &mut [u8]) {
    out[0..2].copy_from_slice(&hdr.magic.to_be_bytes());
    out[2] = hdr.version;
    out[3] = hdr.msg_type;
    out[4..6].copy_from_slice(&hdr.flags.to_be_bytes());
    out[6..8].copy_from_slice(&hdr.header_len.to_be_bytes());
    out[8..12].copy_from_slice(&hdr.payload_len.to_be_bytes());
    out[12..16].copy_from_slice(&hdr.seq.to_be_bytes());
    out[16..24].copy_from_slice(&hdr.timestamp_ns.to_be_bytes());
    out[24..26].copy_from_slice(&hdr.error_code.to_be_bytes());
    out[26..28].copy_from_slice(&hdr.crc16.to_be_bytes());
    out[28..32].copy_from_slice(&hdr.reserved.to_be_bytes());
}

fn header_from_wire(wire: &[u8]) -> Header {
    Header {
        magic: u16::from_be_bytes([wire[0], wire[1]]),
        version: wire[2],
        msg_type: wire[3],
        flags: u16::from_be_bytes([wire[4], wire[5]]),
        header_len: u16::from_be_bytes([wire[6], wire[7]]),
        payload_len: u32::from_be_bytes([wire[8], wire[9], wire[10], wire[11]]),
        seq: u32::from_be_bytes([wire[12], wire[13], wire[14], wire[15]]),
        timestamp_ns: u64::from_be_bytes([
            wire[16], wire[17], wire[18], wire[19], wire[20], wire[21], wire[22], wire[23],
        ]),
        error_code: u16::from_be_bytes([wire[24], wire[25]]),
        crc16: u16::from_be_bytes([wire[26], wire[27]]),
        reserved: u32::from_be_bytes([wire[28], wire[29], wire[30], wire[31]]),
    }
}

fn frame_crc(mut hdr: Header, payload: &[u8]) -> u16 {
    hdr.crc16 = 0;
    let mut wire = [0u8; AICP_HEADER_LEN];
    header_to_wire(hdr, &mut wire);
    crc16_ccitt_update(crc16_ccitt_update(0xffff, &wire), payload)
}

fn crc16_ccitt_update(mut crc: u16, data: &[u8]) -> u16 {
    for byte in data {
        crc ^= (*byte as u16) << 8;
        for _ in 0..8 {
            crc = if (crc & 0x8000) != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn control_to_payload(control: ControlPayload) -> [u8; 24] {
    let mut out = [0u8; 24];
    out[0..4].copy_from_slice(&control.target.to_ne_bytes());
    out[4..8].copy_from_slice(&control.kp.to_ne_bytes());
    out[8..12].copy_from_slice(&control.ki.to_ne_bytes());
    out[12..16].copy_from_slice(&control.kd.to_ne_bytes());
    out[16..20].copy_from_slice(&control.feed_forward.to_ne_bytes());
    out[20..24].copy_from_slice(&control.mode.to_ne_bytes());
    out
}

fn status_from_payload(payload: &[u8]) -> Option<StatusPayload> {
    if payload.len() != 24 {
        return None;
    }
    Some(StatusPayload {
        setpoint: f32::from_ne_bytes(payload[0..4].try_into().ok()?),
        measured: f32::from_ne_bytes(payload[4..8].try_into().ok()?),
        control_output: f32::from_ne_bytes(payload[8..12].try_into().ok()?),
        error: f32::from_ne_bytes(payload[12..16].try_into().ok()?),
        mode: u32::from_ne_bytes(payload[16..20].try_into().ok()?),
        applied_seq: u32::from_ne_bytes(payload[20..24].try_into().ok()?),
    })
}

fn parse_usize(value: Option<&str>, default: usize) -> usize {
    value.and_then(parse_decimal_usize).unwrap_or(default)
}

fn parse_u16(value: Option<&str>, default: u16) -> u16 {
    value
        .and_then(parse_decimal_usize)
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(default)
}

fn parse_decimal_usize(value: &str) -> Option<usize> {
    let mut out = 0usize;
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        out = out.checked_mul(10)?.checked_add((byte - b'0') as usize)?;
    }
    Some(out)
}

fn parse_ipv4(value: &str) -> ax_std::net::Ipv4Addr {
    let mut octets = [0u8; 4];
    let mut part = 0usize;
    let mut acc = 0usize;
    let mut saw_digit = false;

    for byte in value.bytes().chain(core::iter::once(b'.')) {
        if byte == b'.' {
            if !saw_digit || part >= 4 || acc > 255 {
                return ax_std::net::Ipv4Addr::new(10, 0, 3, 2);
            }
            octets[part] = acc as u8;
            part += 1;
            acc = 0;
            saw_digit = false;
        } else if byte.is_ascii_digit() {
            acc = acc * 10 + (byte - b'0') as usize;
            saw_digit = true;
        } else {
            return ax_std::net::Ipv4Addr::new(10, 0, 3, 2);
        }
    }

    if part == 4 {
        ax_std::net::Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3])
    } else {
        ax_std::net::Ipv4Addr::new(10, 0, 3, 2)
    }
}

fn log_peer_neighbor_discovery() {
    let iface = option_env!("AICP_STARRY_IFACE").unwrap_or("eth0");
    let peer_ip = parse_ipv4_octets(option_env!("AICP_STARRY_SERVER").unwrap_or("10.0.3.2"));
    let peer_mac = parse_mac(option_env!("AICP_STARRY_SERVER_MAC").unwrap_or("52:54:00:aa:03:02"));

    ax_println!(
        "AICP_STARRY_NEIGHBOR_DISCOVERY iface={} peer_ip={}.{}.{}.{} \
         peer_mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        iface,
        peer_ip[0],
        peer_ip[1],
        peer_ip[2],
        peer_ip[3],
        peer_mac[0],
        peer_mac[1],
        peer_mac[2],
        peer_mac[3],
        peer_mac[4],
        peer_mac[5]
    );
}

fn parse_ipv4_octets(value: &str) -> [u8; 4] {
    let ip = parse_ipv4(value).octets();
    [ip[0], ip[1], ip[2], ip[3]]
}

fn parse_mac(value: &str) -> [u8; 6] {
    let mut out = [0u8; 6];
    let mut part = 0usize;
    let mut acc = 0u8;
    let mut digits = 0usize;

    for byte in value.bytes().chain(core::iter::once(b':')) {
        if byte == b':' || byte == b'-' {
            if digits == 0 || part >= 6 {
                return [0x52, 0x54, 0x00, 0xaa, 0x03, 0x02];
            }
            out[part] = acc;
            part += 1;
            acc = 0;
            digits = 0;
            continue;
        }

        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return [0x52, 0x54, 0x00, 0xaa, 0x03, 0x02],
        };
        digits += 1;
        if digits > 2 {
            return [0x52, 0x54, 0x00, 0xaa, 0x03, 0x02];
        }
        acc = (acc << 4) | nibble;
    }

    if part == 6 {
        out
    } else {
        [0x52, 0x54, 0x00, 0xaa, 0x03, 0x02]
    }
}
