//! Zero-allocation ECSS-E-ST-70-41C (PUS-C) Space Packet inspection and CRC-16-CCITT validation.
//!
//! When a downlink carries ECSS PUS-C telemetry or telecommand verification packets inside
//! CCSDS 133.0-B Space Packets, this module validates the trailing 16-bit Packet Error Control
//! (CRC-16-CCITT, `0x1021`, initial syndrome `0xFFFF`) and decodes the PUS secondary header
//! and standard service payload (`ST[01]`, `ST[03]`, `ST[05]`, `ST[06]`, `ST[08]`, `ST[11]`,
//! `ST[12]`, `ST[17]`, `ST[20]`) without heap allocations.

use xtce_gs_core::RawPacket;

pub use xtce_pus::{
    AckFlags, CucTimestamp, PEC_LEN, PRIMARY_HEADER_LEN, PUS_VERSION_C, PusError, PusHeader,
    PusPacket, PusService, TC_SECONDARY_HEADER_LEN, TM_SECONDARY_HEADER_LEN, verify_crc16_ccitt,
};

/// Parses and validates a raw CCSDS + ECSS PUS-C Space Packet slice, checking both the CCSDS
/// declared length and the trailing 16-bit CRC-16-CCITT Packet Error Control field.
///
/// # Errors
///
/// Returns [`PusError`] if `bytes` is shorter than a PUS packet, lacks a secondary header,
/// carries an unsupported PUS version, or fails the CRC-16-CCITT check.
pub fn inspect_pus_bytes(bytes: &[u8]) -> Result<PusPacket<'_>, PusError> {
    PusPacket::parse(bytes)
}

/// Parses and validates an assembled [`RawPacket`] as an ECSS PUS-C Space Packet.
///
/// # Errors
///
/// Returns [`PusError`] if the underlying packet bytes fail CCSDS length, secondary header,
/// PUS version, or CRC-16-CCITT verification.
pub fn inspect_raw_packet(packet: &RawPacket) -> Result<PusPacket<'_>, PusError> {
    PusPacket::parse(&packet.bytes)
}

/// Returns `true` if `packet` is at least two bytes long and ends with a valid big-endian
/// ECSS-E-ST-70-41C CRC-16-CCITT syndrome (`0x0000` residue).
#[must_use]
pub fn verify_raw_packet_crc(packet: &RawPacket) -> bool {
    verify_crc16_ccitt(&packet.bytes)
}

#[cfg(test)]
mod tests {
    use xtce_gs_core::RawPacket;
    use xtce_pus::{
        AckFlags, CucTimestamp, HousekeepingMessage, ObcHousekeeping, PusError, PusHeader,
        PusService, TcParams, TestMessage, TmParams, VerificationReport, crc16_ccitt, encode_tc,
        encode_tm,
    };

    use super::{inspect_pus_bytes, inspect_raw_packet, verify_raw_packet_crc};
    use crate::frame;

    #[test]
    fn frame_and_pus_crc16_ccitt_implementations_agree() {
        let vector = b"123456789";
        assert_eq!(frame::crc16_ccitt(vector), 0x29B1);
        assert_eq!(crc16_ccitt(vector), 0x29B1);
    }

    #[test]
    fn inspects_valid_ecss_pus_tm_housekeeping_packet() {
        let hk = ObcHousekeeping {
            uptime_s: 120,
            bus_voltage_mv: 28_100,
            obc_temp_cdeg: 2350,
            mode: 2,
        };
        let app_data = hk.to_app_data();

        let mut buf = [0u8; 64];
        let total = encode_tm(
            &TmParams {
                apid: 0x010,
                sequence_count: 42,
                service: 3,
                subtype: 25,
                msg_counter: 9,
                destination_id: 0x0001,
                timestamp: CucTimestamp {
                    coarse_seconds: 1_700_000_000,
                    fine_subseconds: 0x8000,
                },
            },
            &app_data,
            &mut buf,
        )
        .expect("encodes TM[3,25]");

        let raw = RawPacket::now(buf[..total].to_vec());
        assert!(verify_raw_packet_crc(&raw));

        let parsed = inspect_raw_packet(&raw).expect("valid PUS-C TM packet");
        assert_eq!(parsed.apid, 0x010);
        assert_eq!(parsed.sequence_count, 42);
        assert_eq!(
            parsed.header,
            PusHeader::Telemetry {
                version: 2,
                time_ref_status: 0,
                service: 3,
                subtype: 25,
                msg_counter: 9,
                destination_id: 0x0001,
                timestamp: CucTimestamp {
                    coarse_seconds: 1_700_000_000,
                    fine_subseconds: 0x8000,
                },
            }
        );

        let service = parsed.decode_service().expect("decodes ST[03]");
        let PusService::Housekeeping(HousekeepingMessage::ParameterReport { sid, parameters }) =
            service
        else {
            panic!("expected ST[03] ParameterReport");
        };
        assert_eq!(sid, ObcHousekeeping::SID);
        assert_eq!(ObcHousekeeping::from_parameter_bytes(parameters), Some(hk));
    }

    #[test]
    fn inspects_st17_test_and_st01_verification_packets_and_rejects_bad_crc() {
        let mut command_wire = [0u8; 32];
        let command_written = encode_tc(
            &TcParams {
                apid: 0x020,
                sequence_count: 15,
                ack: AckFlags::ACCEPT_AND_COMPLETE,
                service: 17,
                subtype: 1,
                source_id: 0x0042,
            },
            &[],
            &mut command_wire,
        )
        .expect("encodes TC[17,1]");

        let parsed_command =
            inspect_pus_bytes(&command_wire[..command_written]).expect("valid TC[17,1]");
        assert_eq!(
            parsed_command.decode_service().expect("ST[17]"),
            PusService::Test(TestMessage::AreYouAliveCommand)
        );

        let mut report_wire = [0u8; 32];
        let req_bytes = parsed_command.request_id.to_be_bytes();
        let report_written = encode_tm(
            &TmParams {
                apid: 0x020,
                sequence_count: 1,
                service: 1,
                subtype: 1,
                msg_counter: 1,
                destination_id: 0x0042,
                timestamp: CucTimestamp::default(),
            },
            &req_bytes,
            &mut report_wire,
        )
        .expect("encodes TM[1,1]");

        let parsed_report =
            inspect_pus_bytes(&report_wire[..report_written]).expect("valid TM[1,1]");
        assert_eq!(
            parsed_report.decode_service().expect("ST[01]"),
            PusService::Verification(VerificationReport::AcceptanceSuccess {
                request_id: parsed_command.request_id,
            })
        );

        report_wire[8] ^= 0x01;
        let corrupted = RawPacket::now(report_wire[..report_written].to_vec());
        assert!(!verify_raw_packet_crc(&corrupted));
        assert!(matches!(
            inspect_raw_packet(&corrupted),
            Err(PusError::CrcMismatch { .. })
        ));
    }
}
