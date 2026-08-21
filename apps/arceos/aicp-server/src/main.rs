// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use aicp_rust_protocol::{
    ERROR_BAD_PAYLOAD, ERROR_BAD_TYPE, ERROR_CRC, ERROR_OK, ERROR_SEQUENCE, ERROR_VERSION,
    HEADER_LEN, Header, MAX_PAYLOAD, MSG_CONTROL_SET, MSG_ERROR, MSG_HEARTBEAT, MSG_HELLO,
    MSG_STATUS, ProtocolError, VERSION, decode_frame, decode_header, encode_frame, encode_header,
    frame_crc, validate_header,
};
#[cfg(feature = "arceos")]
use ax_std as _;

const CONTROL_PERIOD_NS: u64 = 20_000_000;
const PERIODIC_REPORT_SAMPLES: usize = 128;
const PERIODIC_SAMPLE_LOG_INTERVAL: usize = 32;
const PERIODIC_OUTLIER_NS: u64 = 5_000_000;

static PERIODIC_PROBE_ACTIVE: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "arceos")]
fn configure_aicp_network() -> io::Result<()> {
    let interface = ax_net::interfaces()
        .into_iter()
        .find(|interface| interface.name == "eth0")
        .ok_or_else(|| io::Error::other("AICP network interface eth0 is unavailable"))?;

    ax_net::set_interface_ipv4(interface.id, std::net::Ipv4Addr::new(10, 0, 3, 2), 24)
        .map_err(|err| io::Error::other(format!("configure AICP static IPv4: {err:?}")))?;
    println!("AICP_RTOS_NET_READY iface=eth0 ip=10.0.3.2/24");
    Ok(())
}

#[cfg(not(feature = "arceos"))]
fn configure_aicp_network() -> io::Result<()> {
    Ok(())
}

fn udp_drop_every() -> u32 {
    option_env!("AICP_UDP_DROP_EVERY")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn seq_is_newer(seq: u32, previous: u32) -> bool {
    (seq.wrapping_sub(previous) as i32) > 0
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct ControlPayload {
    target: f32,
    kp: f32,
    ki: f32,
    kd: f32,
    feed_forward: f32,
    mode: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct StatusPayload {
    setpoint: f32,
    measured: f32,
    control_output: f32,
    error: f32,
    mode: u32,
    applied_seq: u32,
}

#[derive(Clone, Copy, Debug)]
struct ControlState {
    setpoint: f32,
    measured: f32,
    integral: f32,
    last_error: f32,
    control_output: f32,
    mode: u32,
    applied_seq: u32,
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            setpoint: 0.5,
            measured: 0.0,
            integral: 0.0,
            last_error: 0.0,
            control_output: 0.0,
            mode: 0,
            applied_seq: 0,
        }
    }
}

impl ControlState {
    fn step(&mut self, control: ControlPayload, seq: u32) -> StatusPayload {
        self.setpoint = control.target;
        self.mode = control.mode;
        self.applied_seq = seq;

        let error = control.target - self.measured;
        self.integral = (self.integral + error * 0.02).clamp(-1.0, 1.0);
        let derivative = (error - self.last_error) / 0.02;
        self.last_error = error;

        let raw = control.kp * error
            + control.ki * self.integral
            + control.kd * derivative
            + control.feed_forward;
        self.control_output = raw.clamp(-1.0, 1.0);
        self.measured += 0.18 * (self.control_output - self.measured) + 0.04 * error;

        self.status()
    }

    fn status(&self) -> StatusPayload {
        StatusPayload {
            setpoint: self.setpoint,
            measured: self.measured,
            control_output: self.control_output,
            error: self.setpoint - self.measured,
            mode: self.mode,
            applied_seq: self.applied_seq,
        }
    }
}

#[derive(Default)]
struct TimingState {
    last_start: Option<Instant>,
}

impl TimingState {
    fn observe(&mut self, seq: u32, start: Instant, service_ns: u64) {
        let request_interval_ns = self
            .last_start
            .map(|last| duration_ns(start.duration_since(last)))
            .unwrap_or(0);
        let request_interval_deviation_ns = if request_interval_ns == 0 {
            0
        } else {
            request_interval_ns as i128 - CONTROL_PERIOD_NS as i128
        };
        self.last_start = Some(start);
        println!(
            "AICP_RTOS_REQUEST_TIMING seq={} service_ns={} request_interval_ns={} \
             request_interval_deviation_ns={}",
            seq, service_ns, request_interval_ns, request_interval_deviation_ns
        );
    }
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

fn wall_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(duration_ns)
        .unwrap_or(0)
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[(sorted.len() - 1) * percentile / 100]
}

fn periodic_probe() {
    let period = Duration::from_nanos(CONTROL_PERIOD_NS);
    let mut wake_lateness = Vec::new();
    let mut interval_abs_jitter = Vec::new();
    let mut missed_deadlines = 0u64;

    while !PERIODIC_PROBE_ACTIVE.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(1));
    }

    let mut previous_wake = Instant::now();
    let mut deadline = previous_wake + period;
    println!(
        "AICP_RTOS_PERIODIC_READY period_ns={} report_samples={}",
        CONTROL_PERIOD_NS, PERIODIC_REPORT_SAMPLES
    );

    loop {
        let remaining = deadline.duration_since(Instant::now());
        if !remaining.is_zero() {
            thread::sleep(remaining);
        }

        let wake = Instant::now();
        let lateness_ns = duration_ns(wake.duration_since(deadline));
        let interval_ns = duration_ns(wake.duration_since(previous_wake));
        let interval_jitter_ns = interval_ns.abs_diff(CONTROL_PERIOD_NS);
        let missed = lateness_ns / CONTROL_PERIOD_NS;
        missed_deadlines = missed_deadlines.saturating_add(missed);
        wake_lateness.push(lateness_ns);
        interval_abs_jitter.push(interval_jitter_ns);

        let samples = wake_lateness.len();
        if samples == 1
            || samples.is_multiple_of(PERIODIC_SAMPLE_LOG_INTERVAL)
            || lateness_ns >= PERIODIC_OUTLIER_NS
            || missed != 0
        {
            println!(
                "AICP_RTOS_PERIODIC sample={} wake_lateness_ns={} interval_ns={} \
                 interval_abs_jitter_ns={} missed_deadlines={}",
                samples, lateness_ns, interval_ns, interval_jitter_ns, missed_deadlines
            );
        }
        if samples.is_multiple_of(PERIODIC_REPORT_SAMPLES) {
            println!(
                "AICP_RTOS_PERIODIC_DONE samples={} period_ns={} wake_lateness_avg_ns={} \
                 wake_lateness_p99_ns={} wake_lateness_max_ns={} interval_abs_jitter_avg_ns={} \
                 interval_abs_jitter_p99_ns={} interval_abs_jitter_max_ns={} missed_deadlines={}",
                samples,
                CONTROL_PERIOD_NS,
                wake_lateness.iter().sum::<u64>() / samples as u64,
                percentile(&wake_lateness, 99),
                wake_lateness.iter().copied().max().unwrap_or(0),
                interval_abs_jitter.iter().sum::<u64>() / samples as u64,
                percentile(&interval_abs_jitter, 99),
                interval_abs_jitter.iter().copied().max().unwrap_or(0),
                missed_deadlines,
            );
        }

        previous_wake = wake;
        deadline +=
            Duration::from_nanos(CONTROL_PERIOD_NS.saturating_mul(missed.saturating_add(1)));
    }
}

fn make_header(msg_type: u8, payload_len: usize, seq: u32, error_code: u16) -> Header {
    Header::new(
        msg_type,
        0,
        payload_len as u32,
        seq,
        wall_time_ns(),
        error_code,
    )
}

fn send_frame(stream: &mut TcpStream, mut hdr: Header, payload: &[u8]) -> io::Result<()> {
    hdr.payload_len = payload.len() as u32;
    hdr.crc16 = frame_crc(hdr, payload);
    stream.write_all(&encode_header(hdr))?;
    stream.write_all(payload)
}

fn recv_frame(
    stream: &mut TcpStream,
    payload: &mut [u8; MAX_PAYLOAD],
) -> io::Result<(Header, usize)> {
    let mut wire = [0u8; HEADER_LEN];
    stream.read_exact(&mut wire)?;
    let hdr = decode_header(&wire);
    validate_header(hdr).map_err(protocol_io_error)?;
    let len = hdr.payload_len as usize;
    stream.read_exact(&mut payload[..len])?;
    if hdr.crc16 != frame_crc(hdr, &payload[..len]) {
        return Err(protocol_io_error(ProtocolError::CrcMismatch));
    }
    Ok((hdr, len))
}

fn protocol_io_error(error: ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn control_from_payload(payload: &[u8]) -> Option<ControlPayload> {
    if payload.len() != 24 {
        return None;
    }
    Some(ControlPayload {
        target: f32::from_ne_bytes(payload[0..4].try_into().ok()?),
        kp: f32::from_ne_bytes(payload[4..8].try_into().ok()?),
        ki: f32::from_ne_bytes(payload[8..12].try_into().ok()?),
        kd: f32::from_ne_bytes(payload[12..16].try_into().ok()?),
        feed_forward: f32::from_ne_bytes(payload[16..20].try_into().ok()?),
        mode: u32::from_ne_bytes(payload[20..24].try_into().ok()?),
    })
}

fn status_to_payload(status: StatusPayload) -> [u8; 24] {
    let mut out = [0u8; 24];
    out[0..4].copy_from_slice(&status.setpoint.to_ne_bytes());
    out[4..8].copy_from_slice(&status.measured.to_ne_bytes());
    out[8..12].copy_from_slice(&status.control_output.to_ne_bytes());
    out[12..16].copy_from_slice(&status.error.to_ne_bytes());
    out[16..20].copy_from_slice(&status.mode.to_ne_bytes());
    out[20..24].copy_from_slice(&status.applied_seq.to_ne_bytes());
    out
}

fn send_status(stream: &mut TcpStream, state: &ControlState, seq: u32) -> io::Result<()> {
    let payload = status_to_payload(state.status());
    send_frame(
        stream,
        make_header(MSG_STATUS, payload.len(), seq, ERROR_OK),
        &payload,
    )
}

fn send_error(stream: &mut TcpStream, seq: u32, code: u16) -> io::Result<()> {
    let payload = b"{\"error\":\"invalid AICP frame\"}";
    send_frame(
        stream,
        make_header(MSG_ERROR, payload.len(), seq, code),
        payload,
    )
}

fn udp_datagram(hdr: Header, payload: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = vec![0u8; HEADER_LEN + payload.len()];
    let len = encode_frame(hdr, payload, &mut out).map_err(protocol_io_error)?;
    out.truncate(len);
    Ok(out)
}

fn udp_send_status(
    socket: &UdpSocket,
    peer: SocketAddr,
    state: &ControlState,
    seq: u32,
) -> io::Result<()> {
    let payload = status_to_payload(state.status());
    let frame = udp_datagram(
        make_header(MSG_STATUS, payload.len(), seq, ERROR_OK),
        &payload,
    )?;
    socket.send_to(&frame, peer).map(|_| ())
}

fn udp_send_error(socket: &UdpSocket, peer: SocketAddr, seq: u32, code: u16) -> io::Result<()> {
    let payload = b"{\"error\":\"invalid AICP datagram\"}";
    let frame = udp_datagram(make_header(MSG_ERROR, payload.len(), seq, code), payload)?;
    socket.send_to(&frame, peer).map(|_| ())
}

fn udp_parse_datagram(buf: &[u8]) -> Result<(Header, &[u8]), u16> {
    decode_frame(buf).map_err(|error| match error {
        ProtocolError::UnsupportedVersion => ERROR_VERSION,
        ProtocolError::CrcMismatch => ERROR_CRC,
        _ => ERROR_BAD_PAYLOAD,
    })
}

fn serve_udp(socket: UdpSocket) -> io::Result<()> {
    let mut state = ControlState::default();
    let mut timing = TimingState::default();
    let mut last_seq = None::<u32>;
    let mut last_peer = None::<SocketAddr>;
    let mut last_dropped_seq = None::<u32>;
    let drop_every = udp_drop_every();
    let mut buf = vec![0u8; HEADER_LEN + MAX_PAYLOAD];

    println!("AICP ArceOS RTOS UDP server listening on 0.0.0.0:8800");
    loop {
        let (len, peer) = socket.recv_from(&mut buf)?;
        let (hdr, payload) = match udp_parse_datagram(&buf[..len]) {
            Ok(frame) => frame,
            Err(code) => {
                let seq = if len >= HEADER_LEN {
                    let mut wire = [0u8; HEADER_LEN];
                    wire.copy_from_slice(&buf[..HEADER_LEN]);
                    decode_header(&wire).seq
                } else {
                    0
                };
                udp_send_error(&socket, peer, seq, code)?;
                continue;
            }
        };

        if last_peer == Some(peer) && last_seq == Some(hdr.seq) {
            println!("AICP UDP duplicate seq={} peer={}", hdr.seq, peer);
            udp_send_status(&socket, peer, &state, hdr.seq)?;
            continue;
        }
        if last_peer == Some(peer)
            && let Some(previous) = last_seq
            && !seq_is_newer(hdr.seq, previous)
        {
            println!(
                "AICP UDP out_of_order seq={} previous={} peer={}",
                hdr.seq, previous, peer
            );
            udp_send_error(&socket, peer, hdr.seq, ERROR_SEQUENCE)?;
            continue;
        }

        match hdr.msg_type {
            MSG_HELLO => {
                println!(
                    "AICP UDP HELLO seq={} payload_len={} peer={}",
                    hdr.seq,
                    payload.len(),
                    peer
                );
                last_seq = Some(hdr.seq);
                last_peer = Some(peer);
                udp_send_status(&socket, peer, &state, hdr.seq)?;
            }
            MSG_HEARTBEAT => {
                last_seq = Some(hdr.seq);
                last_peer = Some(peer);
                udp_send_status(&socket, peer, &state, hdr.seq)?;
            }
            MSG_CONTROL_SET => {
                PERIODIC_PROBE_ACTIVE.store(true, Ordering::Release);
                let start = Instant::now();
                let Some(control) = control_from_payload(payload) else {
                    udp_send_error(&socket, peer, hdr.seq, ERROR_BAD_PAYLOAD)?;
                    continue;
                };
                let status = state.step(control, hdr.seq);
                let service_ns = duration_ns(start.elapsed());
                println!(
                    "CONTROL seq={} target={:.3} measured={:.3} output={:.3}",
                    hdr.seq, status.setpoint, status.measured, status.control_output
                );
                timing.observe(hdr.seq, start, service_ns);
                last_seq = Some(hdr.seq);
                last_peer = Some(peer);
                if drop_every != 0
                    && hdr.seq.is_multiple_of(drop_every)
                    && last_dropped_seq != Some(hdr.seq)
                {
                    last_dropped_seq = Some(hdr.seq);
                    println!(
                        "AICP UDP fault_drop seq={} drop_every={} peer={}",
                        hdr.seq, drop_every, peer
                    );
                    continue;
                }
                udp_send_status(&socket, peer, &state, hdr.seq)?;
            }
            _ => udp_send_error(&socket, peer, hdr.seq, ERROR_BAD_TYPE)?,
        }
    }
}

fn serve_client(mut stream: TcpStream) -> io::Result<()> {
    let mut state = ControlState::default();
    let mut timing = TimingState::default();
    let mut payload = [0u8; MAX_PAYLOAD];

    loop {
        let (hdr, len) = recv_frame(&mut stream, &mut payload)?;
        if hdr.version != VERSION {
            send_error(&mut stream, hdr.seq, ERROR_VERSION)?;
            continue;
        }
        match hdr.msg_type {
            MSG_HELLO => println!("AICP HELLO seq={} payload_len={}", hdr.seq, len),
            MSG_HEARTBEAT => send_status(&mut stream, &state, hdr.seq)?,
            MSG_CONTROL_SET => {
                PERIODIC_PROBE_ACTIVE.store(true, Ordering::Release);
                let start = Instant::now();
                let Some(control) = control_from_payload(&payload[..len]) else {
                    send_error(&mut stream, hdr.seq, ERROR_BAD_PAYLOAD)?;
                    continue;
                };
                let status = state.step(control, hdr.seq);
                let service_ns = duration_ns(start.elapsed());
                println!(
                    "CONTROL seq={} target={:.3} measured={:.3} output={:.3}",
                    hdr.seq, status.setpoint, status.measured, status.control_output
                );
                timing.observe(hdr.seq, start, service_ns);
                send_status(&mut stream, &state, hdr.seq)?;
            }
            _ => send_error(&mut stream, hdr.seq, ERROR_BAD_TYPE)?,
        }
    }
}

fn main() -> io::Result<()> {
    configure_aicp_network()?;
    thread::spawn(periodic_probe);

    let udp = UdpSocket::bind("0.0.0.0:8800")?;
    thread::spawn(move || {
        if let Err(err) = serve_udp(udp) {
            println!("AICP UDP server closed: {err:?}");
        }
    });

    let listener = TcpListener::bind("0.0.0.0:8800")?;
    println!("AICP ArceOS RTOS TCP server listening on 0.0.0.0:8800");
    println!("AICP_RTOS_READY");
    loop {
        let (stream, addr) = listener.accept()?;
        println!("AICP client connected: {addr}");
        if let Err(err) = serve_client(stream) {
            println!("AICP client closed: {err:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_changes_when_payload_changes() {
        let hdr = make_header(MSG_HELLO, 3, 7, ERROR_OK);
        assert_ne!(frame_crc(hdr, b"abc"), frame_crc(hdr, b"abd"));
    }

    #[test]
    fn control_step_reduces_error_with_ai_gain() {
        let mut state = ControlState::default();
        let target = 0.8;
        let before = (target - state.measured).abs();
        let status = state.step(
            ControlPayload {
                target,
                kp: 0.72,
                ki: 0.08,
                kd: 0.02,
                feed_forward: 0.03,
                mode: 1,
            },
            42,
        );
        assert_eq!(status.applied_seq, 42);
        assert!(status.error.abs() < before);
    }

    #[test]
    fn udp_sequence_order_handles_wrap_and_stale_packets() {
        assert!(seq_is_newer(11, 10));
        assert!(!seq_is_newer(10, 10));
        assert!(!seq_is_newer(9, 10));
        assert!(seq_is_newer(0, u32::MAX));
    }
}
