// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(not(feature = "std"), no_std)]

pub const MAGIC: u16 = 0xA1C0;
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 32;
pub const MAX_PAYLOAD: usize = 4096;

pub const MSG_HELLO: u8 = 0x01;
pub const MSG_CONTROL_SET: u8 = 0x02;
pub const MSG_STATUS: u8 = 0x03;
pub const MSG_ERROR: u8 = 0x04;
pub const MSG_HEARTBEAT: u8 = 0x05;

pub const ERROR_OK: u16 = 0;
pub const ERROR_VERSION: u16 = 1;
pub const ERROR_CRC: u16 = 2;
pub const ERROR_BAD_TYPE: u16 = 3;
pub const ERROR_BAD_PAYLOAD: u16 = 4;
pub const ERROR_SEQUENCE: u16 = 7;
pub const FLAG_ACK_REQUIRED: u16 = 1 << 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub magic: u16,
    pub version: u8,
    pub msg_type: u8,
    pub flags: u16,
    pub header_len: u16,
    pub payload_len: u32,
    pub seq: u32,
    pub timestamp_ns: u64,
    pub error_code: u16,
    pub crc16: u16,
    pub reserved: u32,
}

impl Header {
    pub const fn new(
        msg_type: u8,
        flags: u16,
        payload_len: u32,
        seq: u32,
        timestamp_ns: u64,
        error_code: u16,
    ) -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            msg_type,
            flags,
            header_len: HEADER_LEN as u16,
            payload_len,
            seq,
            timestamp_ns,
            error_code,
            crc16: 0,
            reserved: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    BadMagic,
    UnsupportedVersion,
    BadHeaderLength,
    PayloadTooLarge,
    PayloadLengthMismatch,
    OutputTooSmall,
    CrcMismatch,
}

impl core::fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let message = match self {
            Self::BadMagic => "bad AICP magic",
            Self::UnsupportedVersion => "unsupported AICP version",
            Self::BadHeaderLength => "bad AICP header length",
            Self::PayloadTooLarge => "AICP payload too large",
            Self::PayloadLengthMismatch => "AICP payload length mismatch",
            Self::OutputTooSmall => "AICP output buffer too small",
            Self::CrcMismatch => "AICP CRC mismatch",
        };
        formatter.write_str(message)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ProtocolError {}

pub fn encode_header(header: Header) -> [u8; HEADER_LEN] {
    let mut output = [0u8; HEADER_LEN];
    output[0..2].copy_from_slice(&header.magic.to_be_bytes());
    output[2] = header.version;
    output[3] = header.msg_type;
    output[4..6].copy_from_slice(&header.flags.to_be_bytes());
    output[6..8].copy_from_slice(&header.header_len.to_be_bytes());
    output[8..12].copy_from_slice(&header.payload_len.to_be_bytes());
    output[12..16].copy_from_slice(&header.seq.to_be_bytes());
    output[16..24].copy_from_slice(&header.timestamp_ns.to_be_bytes());
    output[24..26].copy_from_slice(&header.error_code.to_be_bytes());
    output[26..28].copy_from_slice(&header.crc16.to_be_bytes());
    output[28..32].copy_from_slice(&header.reserved.to_be_bytes());
    output
}

pub fn decode_header(input: &[u8; HEADER_LEN]) -> Header {
    Header {
        magic: u16::from_be_bytes([input[0], input[1]]),
        version: input[2],
        msg_type: input[3],
        flags: u16::from_be_bytes([input[4], input[5]]),
        header_len: u16::from_be_bytes([input[6], input[7]]),
        payload_len: u32::from_be_bytes([input[8], input[9], input[10], input[11]]),
        seq: u32::from_be_bytes([input[12], input[13], input[14], input[15]]),
        timestamp_ns: u64::from_be_bytes([
            input[16], input[17], input[18], input[19], input[20], input[21], input[22], input[23],
        ]),
        error_code: u16::from_be_bytes([input[24], input[25]]),
        crc16: u16::from_be_bytes([input[26], input[27]]),
        reserved: u32::from_be_bytes([input[28], input[29], input[30], input[31]]),
    }
}

pub fn validate_header(header: Header) -> Result<(), ProtocolError> {
    if header.magic != MAGIC {
        return Err(ProtocolError::BadMagic);
    }
    if header.version != VERSION {
        return Err(ProtocolError::UnsupportedVersion);
    }
    if header.header_len as usize != HEADER_LEN {
        return Err(ProtocolError::BadHeaderLength);
    }
    if header.payload_len as usize > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge);
    }
    Ok(())
}

pub fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub fn frame_crc(mut header: Header, payload: &[u8]) -> u16 {
    header.crc16 = 0;
    crc16_update(crc16_update(0xffff, &encode_header(header)), payload)
}

pub fn validate_frame(header: Header, payload: &[u8]) -> Result<(), ProtocolError> {
    validate_header(header)?;
    if payload.len() != header.payload_len as usize {
        return Err(ProtocolError::PayloadLengthMismatch);
    }
    if frame_crc(header, payload) != header.crc16 {
        return Err(ProtocolError::CrcMismatch);
    }
    Ok(())
}

pub fn encode_frame(
    mut header: Header,
    payload: &[u8],
    output: &mut [u8],
) -> Result<usize, ProtocolError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge);
    }
    let frame_len = HEADER_LEN + payload.len();
    if output.len() < frame_len {
        return Err(ProtocolError::OutputTooSmall);
    }

    header.magic = MAGIC;
    header.version = VERSION;
    header.header_len = HEADER_LEN as u16;
    header.payload_len = payload.len() as u32;
    header.crc16 = frame_crc(header, payload);
    output[..HEADER_LEN].copy_from_slice(&encode_header(header));
    output[HEADER_LEN..frame_len].copy_from_slice(payload);
    Ok(frame_len)
}

pub fn decode_frame(input: &[u8]) -> Result<(Header, &[u8]), ProtocolError> {
    if input.len() < HEADER_LEN {
        return Err(ProtocolError::PayloadLengthMismatch);
    }
    let mut wire = [0u8; HEADER_LEN];
    wire.copy_from_slice(&input[..HEADER_LEN]);
    let header = decode_header(&wire);
    validate_header(header)?;

    let frame_len = HEADER_LEN + header.payload_len as usize;
    if input.len() != frame_len {
        return Err(ProtocolError::PayloadLengthMismatch);
    }
    let payload = &input[HEADER_LEN..];
    validate_frame(header, payload)?;
    Ok((header, payload))
}

#[cfg(feature = "std")]
pub mod io {
    use std::io::{self, Read, Write};

    use super::{
        HEADER_LEN, Header, MAX_PAYLOAD, ProtocolError, decode_header, encode_header, frame_crc,
        validate_header,
    };

    fn invalid_data(error: ProtocolError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }

    pub fn send_frame<W: Write>(
        writer: &mut W,
        mut header: Header,
        payload: &[u8],
    ) -> io::Result<()> {
        if payload.len() > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                ProtocolError::PayloadTooLarge,
            ));
        }
        header.magic = super::MAGIC;
        header.version = super::VERSION;
        header.header_len = HEADER_LEN as u16;
        header.payload_len = payload.len() as u32;
        header.crc16 = frame_crc(header, payload);
        writer.write_all(&encode_header(header))?;
        writer.write_all(payload)
    }

    pub fn receive_frame<R: Read>(reader: &mut R) -> io::Result<(Header, Vec<u8>)> {
        let mut wire = [0u8; HEADER_LEN];
        reader.read_exact(&mut wire)?;
        let header = decode_header(&wire);
        validate_header(header).map_err(invalid_data)?;

        let mut payload = vec![0u8; header.payload_len as usize];
        if !payload.is_empty() {
            reader.read_exact(&mut payload)?;
        }
        if frame_crc(header, &payload) != header.crc16 {
            return Err(invalid_data(ProtocolError::CrcMismatch));
        }
        Ok((header, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_header(payload_len: u32) -> Header {
        Header::new(MSG_HELLO, 0, payload_len, 7, 123_456_789, ERROR_OK)
    }

    #[test]
    fn header_round_trip() {
        let header = test_header(5);
        assert_eq!(decode_header(&encode_header(header)), header);
    }

    #[test]
    fn crc_matches_protocol_vector() {
        assert_eq!(frame_crc(test_header(5), b"hello"), 0xec45);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut header = test_header(0);
        header.magic = 0;
        assert_eq!(validate_header(header), Err(ProtocolError::BadMagic));
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut header = test_header(0);
        header.version = VERSION + 1;
        assert_eq!(
            validate_header(header),
            Err(ProtocolError::UnsupportedVersion)
        );
    }

    #[test]
    fn rejects_bad_header_length() {
        let mut header = test_header(0);
        header.header_len = 0;
        assert_eq!(validate_header(header), Err(ProtocolError::BadHeaderLength));
    }

    #[test]
    fn rejects_oversized_payload() {
        let header = test_header((MAX_PAYLOAD + 1) as u32);
        assert_eq!(validate_header(header), Err(ProtocolError::PayloadTooLarge));
    }

    #[test]
    fn rejects_crc_mismatch() {
        let payload = b"hello";
        let mut header = test_header(payload.len() as u32);
        header.crc16 = frame_crc(header, payload);
        assert_eq!(
            validate_frame(header, b"jello"),
            Err(ProtocolError::CrcMismatch)
        );
    }

    #[test]
    fn frame_round_trip_without_allocation() {
        let payload = b"hello";
        let mut output = [0u8; HEADER_LEN + 5];
        let len = encode_frame(test_header(0), payload, &mut output).unwrap();
        let (header, decoded) = decode_frame(&output[..len]).unwrap();
        assert_eq!(header.payload_len, payload.len() as u32);
        assert_eq!(decoded, payload);
    }

    #[test]
    fn rejects_truncated_frame() {
        let payload = b"hello";
        let mut output = [0u8; HEADER_LEN + 5];
        let len = encode_frame(test_header(0), payload, &mut output).unwrap();
        assert_eq!(
            decode_frame(&output[..len - 1]),
            Err(ProtocolError::PayloadLengthMismatch)
        );
    }
}
