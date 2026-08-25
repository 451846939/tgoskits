// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use aicp_rust_protocol::{
    CONTROL_PAYLOAD_LEN, ControlPayload, ERROR_BAD_PAYLOAD, ERROR_BAD_TYPE, ERROR_CRC, ERROR_OK,
    ERROR_SEQUENCE, ERROR_VERSION, HEADER_LEN, Header, MAX_PAYLOAD, MSG_CONTROL_SET, MSG_ERROR,
    MSG_HEARTBEAT, MSG_HELLO, MSG_STATUS, ProtocolError, STATUS_PAYLOAD_LEN, StatusPayload,
    VERSION, decode_control_payload, decode_frame, decode_header, encode_frame, encode_header,
    encode_status_payload, frame_crc, validate_header_shape,
};
#[cfg(test)]
use aicp_rust_protocol::{decode_status_payload, encode_control_payload};
#[cfg(feature = "arceos")]
use ax_std as _;

const CONTROL_PERIOD_NS: u64 = 20_000_000;
const PERIODIC_REPORT_SAMPLES: usize = 128;
const PERIODIC_SAMPLE_LOG_INTERVAL: usize = 32;
const PERIODIC_OUTLIER_NS: u64 = 5_000_000;
const MAX_AICP_DATAGRAM_LEN: usize = HEADER_LEN + MAX_PAYLOAD;
// A one-byte sentinel distinguishes a maximum valid datagram from a datagram
// that the socket layer truncated to the same length.
const UDP_RECEIVE_BUFFER_LEN: usize = MAX_AICP_DATAGRAM_LEN + 1;

static PERIODIC_PROBE_ACTIVE: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "arceos")]
fn configure_aicp_network() -> io::Result<()> {
    let interface = ax_net::interfaces()
        .into_iter()
        .find(|interface| interface.name == "eth0")
        .ok_or_else(|| io::Error::other("AICP network interface eth0 is unavailable"))?;

    let ip = std::net::Ipv4Addr::new(10, 0, 3, 2);
    let configured = interface
        .ipv4
        .ok_or_else(|| io::Error::other("AICP static IPv4 is missing on eth0"))?;
    if std::net::Ipv4Addr::from(configured.address.address().octets()) != ip
        || configured.address.prefix_len() != 24
    {
        return Err(io::Error::other(format!(
            "AICP eth0 address is {}/{} instead of {ip}/24",
            configured.address.address(),
            configured.address.prefix_len()
        )));
    }
    println!("AICP_RTOS_NET_READY iface=eth0 ip={ip}/24");
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

struct PeriodicSamples {
    wake_lateness: Vec<u64>,
    interval_abs_jitter: Vec<u64>,
}

impl PeriodicSamples {
    fn new() -> Self {
        Self {
            wake_lateness: Vec::with_capacity(PERIODIC_REPORT_SAMPLES),
            interval_abs_jitter: Vec::with_capacity(PERIODIC_REPORT_SAMPLES),
        }
    }

    fn push(&mut self, lateness_ns: u64, interval_jitter_ns: u64) {
        debug_assert!(self.wake_lateness.len() < PERIODIC_REPORT_SAMPLES);
        debug_assert!(self.interval_abs_jitter.len() < PERIODIC_REPORT_SAMPLES);
        self.wake_lateness.push(lateness_ns);
        self.interval_abs_jitter.push(interval_jitter_ns);
    }

    fn len(&self) -> usize {
        self.wake_lateness.len()
    }

    fn report_and_reset(&mut self, missed_deadlines: u64) {
        let samples = self.len();
        debug_assert_eq!(samples, self.interval_abs_jitter.len());
        println!(
            "AICP_RTOS_PERIODIC_DONE samples={} period_ns={} wake_lateness_avg_ns={} \
             wake_lateness_p99_ns={} wake_lateness_max_ns={} interval_abs_jitter_avg_ns={} \
             interval_abs_jitter_p99_ns={} interval_abs_jitter_max_ns={} missed_deadlines={}",
            samples,
            CONTROL_PERIOD_NS,
            self.wake_lateness.iter().sum::<u64>() / samples as u64,
            percentile(&self.wake_lateness, 99),
            self.wake_lateness.iter().copied().max().unwrap_or(0),
            self.interval_abs_jitter.iter().sum::<u64>() / samples as u64,
            percentile(&self.interval_abs_jitter, 99),
            self.interval_abs_jitter.iter().copied().max().unwrap_or(0),
            missed_deadlines,
        );
        self.wake_lateness.clear();
        self.interval_abs_jitter.clear();
    }
}

fn periodic_probe() {
    let period = Duration::from_nanos(CONTROL_PERIOD_NS);
    let mut samples = PeriodicSamples::new();
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
        samples.push(lateness_ns, interval_jitter_ns);

        let sample_count = samples.len();
        if sample_count == 1
            || sample_count.is_multiple_of(PERIODIC_SAMPLE_LOG_INTERVAL)
            || lateness_ns >= PERIODIC_OUTLIER_NS
            || missed != 0
        {
            println!(
                "AICP_RTOS_PERIODIC sample={} wake_lateness_ns={} interval_ns={} \
                 interval_abs_jitter_ns={} missed_deadlines={}",
                sample_count, lateness_ns, interval_ns, interval_jitter_ns, missed_deadlines
            );
        }
        if sample_count == PERIODIC_REPORT_SAMPLES {
            samples.report_and_reset(missed_deadlines);
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
    validate_header_shape(hdr).map_err(protocol_io_error)?;
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
    let wire: &[u8; CONTROL_PAYLOAD_LEN] = payload.try_into().ok()?;

    Some(decode_control_payload(wire))
}

fn status_to_payload(status: StatusPayload) -> [u8; STATUS_PAYLOAD_LEN] {
    encode_status_payload(status)
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

#[derive(Clone, Copy)]
enum ControlReply {
    Status(StatusPayload),
    Error(u16),
}

fn udp_send_status(
    socket: &UdpSocket,
    peer: SocketAddr,
    status: StatusPayload,
    seq: u32,
) -> io::Result<()> {
    let payload = status_to_payload(status);
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

fn udp_send_reply(
    socket: &UdpSocket,
    peer: SocketAddr,
    seq: u32,
    reply: ControlReply,
) -> io::Result<()> {
    match reply {
        ControlReply::Status(status) => udp_send_status(socket, peer, status, seq),
        ControlReply::Error(code) => udp_send_error(socket, peer, seq, code),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UdpSequenceState {
    New,
    Duplicate,
    OutOfOrder,
}

#[derive(Default)]
struct UdpPeerSession {
    last_seq: Option<u32>,
    last_peer: Option<SocketAddr>,
    last_reply: Option<ControlReply>,
}

impl UdpPeerSession {
    fn classify(&self, peer: SocketAddr, seq: u32) -> UdpSequenceState {
        if self.last_peer != Some(peer) {
            return UdpSequenceState::New;
        }
        match self.last_seq {
            Some(previous) if seq == previous => UdpSequenceState::Duplicate,
            Some(previous) if !seq_is_newer(seq, previous) => UdpSequenceState::OutOfOrder,
            _ => UdpSequenceState::New,
        }
    }

    fn record(&mut self, peer: SocketAddr, seq: u32, reply: ControlReply) {
        self.last_peer = Some(peer);
        self.last_seq = Some(seq);
        self.last_reply = Some(reply);
    }

    fn cached_reply(&self) -> Option<ControlReply> {
        self.last_reply
    }
}

fn udp_parse_datagram(buf: &[u8]) -> Result<(Header, &[u8]), u16> {
    decode_frame(buf).map_err(|error| match error {
        ProtocolError::UnsupportedVersion => ERROR_VERSION,
        ProtocolError::CrcMismatch => ERROR_CRC,
        _ => ERROR_BAD_PAYLOAD,
    })
}

fn datagram_sequence(datagram: &[u8]) -> u32 {
    if datagram.len() < HEADER_LEN {
        return 0;
    }
    let mut wire = [0u8; HEADER_LEN];
    wire.copy_from_slice(&datagram[..HEADER_LEN]);
    decode_header(&wire).seq
}

fn serve_udp(socket: UdpSocket) -> io::Result<()> {
    let mut state = ControlState::default();
    let mut timing = TimingState::default();
    let mut session = UdpPeerSession::default();
    let mut last_dropped_seq = None::<u32>;
    let drop_every = udp_drop_every();
    let mut buf = vec![0u8; UDP_RECEIVE_BUFFER_LEN];

    println!("AICP ArceOS RTOS UDP server listening on 0.0.0.0:8800");
    loop {
        let (len, peer) = socket.recv_from(&mut buf)?;
        if len > MAX_AICP_DATAGRAM_LEN {
            let seq = datagram_sequence(&buf[..len]);
            udp_send_error(&socket, peer, seq, ERROR_BAD_PAYLOAD)?;
            continue;
        }
        let (hdr, payload) = match udp_parse_datagram(&buf[..len]) {
            Ok(frame) => frame,
            Err(code) => {
                let seq = datagram_sequence(&buf[..len]);
                udp_send_error(&socket, peer, seq, code)?;
                continue;
            }
        };

        match session.classify(peer, hdr.seq) {
            UdpSequenceState::Duplicate => {
                println!("AICP UDP duplicate seq={} peer={}", hdr.seq, peer);
                let reply = session.cached_reply().ok_or_else(|| {
                    io::Error::other("AICP UDP duplicate is missing its cached reply")
                })?;
                udp_send_reply(&socket, peer, hdr.seq, reply)?;
                continue;
            }
            UdpSequenceState::OutOfOrder => {
                println!("AICP UDP out_of_order seq={} peer={}", hdr.seq, peer);
                udp_send_error(&socket, peer, hdr.seq, ERROR_SEQUENCE)?;
                continue;
            }
            UdpSequenceState::New => {}
        }

        match hdr.msg_type {
            MSG_HELLO => {
                println!(
                    "AICP UDP HELLO seq={} payload_len={} peer={}",
                    hdr.seq,
                    payload.len(),
                    peer
                );
                let reply = ControlReply::Status(state.status());
                session.record(peer, hdr.seq, reply);
                udp_send_reply(&socket, peer, hdr.seq, reply)?;
            }
            MSG_HEARTBEAT => {
                let reply = ControlReply::Status(state.status());
                session.record(peer, hdr.seq, reply);
                udp_send_reply(&socket, peer, hdr.seq, reply)?;
            }
            MSG_CONTROL_SET => {
                PERIODIC_PROBE_ACTIVE.store(true, Ordering::Release);
                let start = Instant::now();
                let Some(control) = control_from_payload(payload) else {
                    let reply = ControlReply::Error(ERROR_BAD_PAYLOAD);
                    session.record(peer, hdr.seq, reply);
                    udp_send_reply(&socket, peer, hdr.seq, reply)?;
                    continue;
                };
                let status = state.step(control, hdr.seq);
                let service_ns = duration_ns(start.elapsed());
                println!(
                    "CONTROL seq={} target={:.3} measured={:.3} output={:.3}",
                    hdr.seq, status.setpoint, status.measured, status.control_output
                );
                timing.observe(hdr.seq, start, service_ns);
                let reply = ControlReply::Status(status);
                session.record(peer, hdr.seq, reply);
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
                udp_send_reply(&socket, peer, hdr.seq, reply)?;
            }
            _ => {
                let reply = ControlReply::Error(ERROR_BAD_TYPE);
                session.record(peer, hdr.seq, reply);
                udp_send_reply(&socket, peer, hdr.seq, reply)?;
            }
        }
    }
}

#[derive(Default)]
struct TcpControlService {
    state: ControlState,
    timing: TimingState,
}

type SharedTcpControlService = Arc<Mutex<TcpControlService>>;

fn lock_control_service(
    service: &SharedTcpControlService,
) -> io::Result<std::sync::MutexGuard<'_, TcpControlService>> {
    service
        .lock()
        .map_err(|_| io::Error::other("AICP control service lock is poisoned"))
}

#[derive(Default)]
struct TcpConnectionSession {
    last_seq: Option<u32>,
    last_reply: Option<ControlReply>,
}

impl TcpConnectionSession {
    fn replay_last_reply(&self, stream: &mut TcpStream) -> io::Result<()> {
        let Some(seq) = self.last_seq else {
            return Ok(());
        };
        match self.last_reply {
            Some(ControlReply::Status(status)) => {
                let payload = status_to_payload(status);
                send_frame(
                    stream,
                    make_header(MSG_STATUS, payload.len(), seq, ERROR_OK),
                    &payload,
                )
            }
            Some(ControlReply::Error(code)) => send_error(stream, seq, code),
            None => Ok(()),
        }
    }

    fn send_status(
        &mut self,
        stream: &mut TcpStream,
        status: StatusPayload,
        seq: u32,
    ) -> io::Result<()> {
        let payload = status_to_payload(status);
        send_frame(
            stream,
            make_header(MSG_STATUS, payload.len(), seq, ERROR_OK),
            &payload,
        )?;
        self.last_seq = Some(seq);
        self.last_reply = Some(ControlReply::Status(status));
        Ok(())
    }

    fn send_error(&mut self, stream: &mut TcpStream, seq: u32, code: u16) -> io::Result<()> {
        send_error(stream, seq, code)?;
        self.last_seq = Some(seq);
        self.last_reply = Some(ControlReply::Error(code));
        Ok(())
    }
}

fn serve_client(service: &SharedTcpControlService, mut stream: TcpStream) -> io::Result<()> {
    let mut payload = [0u8; MAX_PAYLOAD];
    let mut session = TcpConnectionSession::default();

    loop {
        let (hdr, len) = recv_frame(&mut stream, &mut payload)?;
        if hdr.version != VERSION {
            send_error(&mut stream, hdr.seq, ERROR_VERSION)?;
            continue;
        }
        if session.last_seq == Some(hdr.seq) {
            session.replay_last_reply(&mut stream)?;
            continue;
        }
        if let Some(previous) = session.last_seq
            && !seq_is_newer(hdr.seq, previous)
        {
            send_error(&mut stream, hdr.seq, ERROR_SEQUENCE)?;
            continue;
        }
        match hdr.msg_type {
            MSG_HELLO => {
                println!("AICP HELLO seq={} payload_len={}", hdr.seq, len);
                let status = lock_control_service(service)?.state.status();
                session.send_status(&mut stream, status, hdr.seq)?;
            }
            MSG_HEARTBEAT => {
                let status = lock_control_service(service)?.state.status();
                session.send_status(&mut stream, status, hdr.seq)?;
            }
            MSG_CONTROL_SET => {
                PERIODIC_PROBE_ACTIVE.store(true, Ordering::Release);
                let start = Instant::now();
                let Some(control) = control_from_payload(&payload[..len]) else {
                    session.send_error(&mut stream, hdr.seq, ERROR_BAD_PAYLOAD)?;
                    continue;
                };
                let mut service = lock_control_service(service)?;
                let status = service.state.step(control, hdr.seq);
                let service_ns = duration_ns(start.elapsed());
                println!(
                    "CONTROL seq={} target={:.3} measured={:.3} output={:.3}",
                    hdr.seq, status.setpoint, status.measured, status.control_output
                );
                service.timing.observe(hdr.seq, start, service_ns);
                drop(service);
                session.send_status(&mut stream, status, hdr.seq)?;
            }
            _ => session.send_error(&mut stream, hdr.seq, ERROR_BAD_TYPE)?,
        }
    }
}

fn serve_accepted_client(service: SharedTcpControlService, stream: TcpStream, addr: SocketAddr) {
    println!("AICP client connected: {addr}");
    thread::spawn(move || {
        if let Err(err) = serve_client(&service, stream) {
            println!("AICP client closed: {err:?}");
        }
    });
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
    let service = Arc::new(Mutex::new(TcpControlService::default()));
    loop {
        let (stream, addr) = listener.accept()?;
        serve_accepted_client(Arc::clone(&service), stream, addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_service() -> SharedTcpControlService {
        Arc::new(Mutex::new(TcpControlService::default()))
    }

    fn write_frame(stream: &mut TcpStream, mut header: Header, payload: &[u8]) {
        header.payload_len = payload.len() as u32;
        header.crc16 = frame_crc(header, payload);
        stream.write_all(&encode_header(header)).unwrap();
        stream.write_all(payload).unwrap();
    }

    fn write_frame_with_version(
        stream: &mut TcpStream,
        mut header: Header,
        payload: &[u8],
        version: u8,
    ) {
        header.version = version;
        write_frame(stream, header, payload);
    }

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

    #[test]
    fn udp_receive_buffer_detects_a_trailing_byte_after_a_maximum_frame() {
        assert_eq!(UDP_RECEIVE_BUFFER_LEN, MAX_AICP_DATAGRAM_LEN + 1);

        let payload = vec![0u8; MAX_PAYLOAD];
        let mut frame = udp_datagram(
            make_header(MSG_HEARTBEAT, payload.len(), 9, ERROR_OK),
            &payload,
        )
        .unwrap();
        frame.push(0);

        assert_eq!(frame.len(), UDP_RECEIVE_BUFFER_LEN);
        assert!(udp_parse_datagram(&frame).is_err());
    }

    #[test]
    fn udp_invalid_control_consumes_its_sequence_and_replays_the_error() {
        let peer = "127.0.0.1:8800".parse().unwrap();
        let seq = 9;
        let mut session = UdpPeerSession::default();
        let mut state = ControlState::default();

        assert!(control_from_payload(&[0; CONTROL_PAYLOAD_LEN - 1]).is_none());
        session.record(peer, seq, ControlReply::Error(ERROR_BAD_PAYLOAD));

        assert_eq!(session.classify(peer, seq), UdpSequenceState::Duplicate);
        assert!(matches!(
            session.cached_reply(),
            Some(ControlReply::Error(ERROR_BAD_PAYLOAD))
        ));

        let valid = encode_control_payload(ControlPayload {
            target: 0.8,
            kp: 0.72,
            ki: 0.08,
            kd: 0.02,
            feed_forward: 0.03,
            mode: 1,
        });
        let valid = control_from_payload(&valid).unwrap();
        match session.classify(peer, seq) {
            UdpSequenceState::New => {
                state.step(valid, seq);
            }
            UdpSequenceState::Duplicate | UdpSequenceState::OutOfOrder => {}
        }
        assert_eq!(state.status().applied_seq, 0);
    }

    #[test]
    fn periodic_samples_are_bounded_by_the_reporting_window() {
        let mut samples = PeriodicSamples::new();
        let wake_capacity = samples.wake_lateness.capacity();
        let jitter_capacity = samples.interval_abs_jitter.capacity();
        for index in 0..PERIODIC_REPORT_SAMPLES * 3 {
            samples.push(index as u64, index as u64 + 1);
            if samples.len() == PERIODIC_REPORT_SAMPLES {
                samples.report_and_reset(0);
            }
            assert!(samples.len() < PERIODIC_REPORT_SAMPLES);
            assert_eq!(samples.wake_lateness.capacity(), wake_capacity);
            assert_eq!(samples.interval_abs_jitter.capacity(), jitter_capacity);
        }
    }

    #[test]
    fn tcp_server_returns_version_error_for_valid_unknown_version_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let service = shared_service();
            serve_client(&service, stream)
        });
        let mut client = TcpStream::connect(address).unwrap();

        write_frame_with_version(
            &mut client,
            make_header(MSG_HELLO, 0, 41, ERROR_OK),
            b"",
            VERSION + 1,
        );

        let mut payload = [0u8; MAX_PAYLOAD];
        let (response, response_len) = recv_frame(&mut client, &mut payload).unwrap();
        assert_eq!(response.msg_type, MSG_ERROR);
        assert_eq!(response.error_code, ERROR_VERSION);
        assert_eq!(response.seq, 41);
        assert_eq!(
            &payload[..response_len],
            b"{\"error\":\"invalid AICP frame\"}"
        );

        drop(client);
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn tcp_duplicate_control_replays_cached_status_without_another_step() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let service = shared_service();
            let result = serve_client(&service, stream);
            let state = lock_control_service(&service).unwrap().state;
            (result, state)
        });
        let mut client = TcpStream::connect(address).unwrap();
        let control = ControlPayload {
            target: 0.8,
            kp: 0.72,
            ki: 0.08,
            kd: 0.02,
            feed_forward: 0.03,
            mode: 1,
        };
        let payload = encode_control_payload(control);

        let request = make_header(MSG_CONTROL_SET, payload.len(), 42, ERROR_OK);
        write_frame(&mut client, request, &payload);
        let mut response_payload = [0u8; MAX_PAYLOAD];
        let (first, first_len) = recv_frame(&mut client, &mut response_payload).unwrap();
        let first_status = response_payload[..first_len].to_vec();
        write_frame(&mut client, request, &payload);
        let (second, second_len) = recv_frame(&mut client, &mut response_payload).unwrap();

        assert_eq!(first.msg_type, MSG_STATUS);
        assert_eq!(second.msg_type, MSG_STATUS);
        assert_eq!(first.seq, 42);
        assert_eq!(second.seq, 42);
        assert_eq!(&response_payload[..second_len], first_status);
        drop(client);
        let (result, state) = server.join().unwrap();
        assert!(result.is_err());
        assert_eq!(state.applied_seq, 42);
        let first_status = decode_status_payload(first_status.as_slice().try_into().unwrap());
        assert_eq!(state.status().measured, first_status.measured);
    }

    #[test]
    fn tcp_hello_consumes_its_sequence_and_prevents_control_replay() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let service = shared_service();
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                assert!(serve_client(&service, stream).is_err());
            }
            lock_control_service(&service).unwrap().state
        });
        let first_control = encode_control_payload(ControlPayload {
            target: 0.0,
            kp: 0.0,
            ki: 0.0,
            kd: 0.0,
            feed_forward: 0.0,
            mode: 0,
        });
        let mut first_client = TcpStream::connect(address).unwrap();
        write_frame(
            &mut first_client,
            make_header(MSG_CONTROL_SET, first_control.len(), 10, ERROR_OK),
            &first_control,
        );
        let mut response_payload = [0u8; MAX_PAYLOAD];
        let (first_response, _) = recv_frame(&mut first_client, &mut response_payload).unwrap();
        assert_eq!(first_response.msg_type, MSG_STATUS);
        drop(first_client);

        let second_control = encode_control_payload(ControlPayload {
            target: 0.5,
            kp: 0.6,
            ki: 0.1,
            kd: 0.01,
            feed_forward: 0.05,
            mode: 1,
        });
        let mut second_client = TcpStream::connect(address).unwrap();
        write_frame(
            &mut second_client,
            make_header(MSG_HELLO, 0, 1, ERROR_OK),
            b"",
        );
        let (hello_response, hello_response_len) =
            recv_frame(&mut second_client, &mut response_payload).unwrap();
        assert_eq!(hello_response.msg_type, MSG_STATUS);
        assert_eq!(hello_response.seq, 1);
        let hello_status =
            decode_status_payload(response_payload[..hello_response_len].try_into().unwrap());
        assert_eq!(hello_status.applied_seq, 10);

        write_frame(
            &mut second_client,
            make_header(MSG_CONTROL_SET, second_control.len(), 1, ERROR_OK),
            &second_control,
        );
        let (response, response_len) =
            recv_frame(&mut second_client, &mut response_payload).unwrap();

        assert_eq!(response.msg_type, MSG_STATUS);
        assert_eq!(response.seq, 1);
        assert_eq!(response.error_code, ERROR_OK);
        let status = decode_status_payload(response_payload[..response_len].try_into().unwrap());
        assert_eq!(status.applied_seq, 10);
        drop(second_client);
        assert_eq!(server.join().unwrap().applied_seq, 10);
    }

    #[test]
    fn partial_tcp_frame_does_not_prevent_a_second_client_from_using_the_service() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let service = shared_service();

        let mut stalled_client = TcpStream::connect(address).unwrap();
        let (stalled_stream, stalled_addr) = listener.accept().unwrap();
        serve_accepted_client(Arc::clone(&service), stalled_stream, stalled_addr);
        stalled_client.write_all(&[0xaa]).unwrap();

        let mut active_client = TcpStream::connect(address).unwrap();
        active_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let (active_stream, active_addr) = listener.accept().unwrap();
        serve_accepted_client(Arc::clone(&service), active_stream, active_addr);
        write_frame(
            &mut active_client,
            make_header(MSG_HELLO, 0, 1, ERROR_OK),
            b"",
        );

        let mut payload = [0u8; MAX_PAYLOAD];
        let (response, _) = recv_frame(&mut active_client, &mut payload).unwrap();
        assert_eq!(response.msg_type, MSG_STATUS);
        assert_eq!(response.seq, 1);

        drop(active_client);
        drop(stalled_client);
    }

    #[test]
    fn tcp_stale_control_returns_sequence_error_without_changing_state() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let service = shared_service();
            let result = serve_client(&service, stream);
            let state = lock_control_service(&service).unwrap().state;
            (result, state)
        });
        let mut client = TcpStream::connect(address).unwrap();
        let payload = encode_control_payload(ControlPayload {
            target: 0.0,
            kp: 0.0,
            ki: 0.0,
            kd: 0.0,
            feed_forward: 0.0,
            mode: 0,
        });
        write_frame(
            &mut client,
            make_header(MSG_CONTROL_SET, payload.len(), 2, ERROR_OK),
            &payload,
        );
        let mut response_payload = [0u8; MAX_PAYLOAD];
        let _ = recv_frame(&mut client, &mut response_payload).unwrap();
        write_frame(
            &mut client,
            make_header(MSG_CONTROL_SET, payload.len(), 1, ERROR_OK),
            &payload,
        );
        let (response, _) = recv_frame(&mut client, &mut response_payload).unwrap();

        assert_eq!(response.msg_type, MSG_ERROR);
        assert_eq!(response.error_code, ERROR_SEQUENCE);
        assert_eq!(response.seq, 1);
        drop(client);
        let (result, state) = server.join().unwrap();
        assert!(result.is_err());
        assert_eq!(state.applied_seq, 2);
    }
}
