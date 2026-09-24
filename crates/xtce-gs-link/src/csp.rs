//! CSP headers.
//!
//! CSP is what a cubesat built on `libcsp` puts around its telemetry: a four- or six-byte
//! header with an address, a port and a handful of flags, and optionally a CRC-32C on the
//! end. It is not a CCSDS standard and there is no Blue Book for it — the authority is
//! `libcsp`'s `csp_types.h`, and that is deliberately the only place the bit positions are
//! written down, because a second copy of them in a doc comment is a second copy to get
//! wrong.
//!
//! This crate reads the header and hands back the payload. It does not reassemble CSP
//! fragments, does not verify HMACs and does not decrypt: a ground station that silently
//! accepted an unauthenticated frame because it could not check the HMAC would be worse than
//! one that refused it.
//!
//! # Where the bit positions came from
//!
//! Fetched from `libcsp` at `master` on 2026-09-12 and read, not remembered:
//!
//! * `src/csp_id.c` — `csp_id1_extract` with the `CSP_ID1_*_OFFSET`/`_MASK` constants for the
//!   32-bit V1 header, and `csp_id2_extract` with the `CSP_ID2_*` constants for the 48-bit V2
//!   one. **The two versions do not order their fields the same way.** V1 is priority,
//!   source, destination; V2 is priority, *destination*, source. The ASCII diagrams above
//!   each function say so and the offsets confirm it.
//! * `include/csp/csp_types.h` — the `CSP_HEADER_FLAGS` group: `CSP_FFRAG` `0x10`,
//!   `CSP_FHMAC` `0x08`, `CSP_FRDP` `0x02`, `CSP_FCRC32` `0x01`. `CSP_FXTEA` `0x04` is in the
//!   same group at tag `v1.6` and is gone from `master`, so encryption is a V1-only flag and
//!   there is no V2 bit for it.
//! * `src/csp_crc32.c` — the CRC table, `csp_crc32_init`, `csp_crc32_final` and, for what a
//!   receiver has to accept, `csp_crc32_verify`. See [`crc32c`].

/// Which version of the CSP header is on the wire.
///
/// Not detectable from the bytes — the two layouts are different fields in the same first
/// octets — so it is configured per mission.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CspVersion {
    /// CSP 1.x: a four-byte header, five-bit addresses.
    #[default]
    V1,
    /// CSP 2.x: a six-byte header, fourteen-bit addresses.
    V2,
}

impl CspVersion {
    /// Length of this version's header.
    #[must_use]
    pub const fn header_bytes(self) -> usize {
        match self {
            Self::V1 => 4,
            Self::V2 => 6,
        }
    }
}

/// A parsed CSP header.
///
/// The fields are widened to the largest version's: an address is a `u16` because CSP 2 has
/// fourteen bits of one, a port is a `u8` because both versions have six. A V1 header read
/// into this loses nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CspHeader {
    version: CspVersion,
    priority: u8,
    source: u16,
    destination: u16,
    source_port: u8,
    destination_port: u8,
    flags: u8,
}

/// `CSP_FFRAG`: the payload is one fragment of a larger message.
const FLAG_FRAGMENT: u8 = 0x10;

/// `CSP_FHMAC`: an HMAC is appended to the payload.
const FLAG_HMAC: u8 = 0x08;

/// `CSP_FXTEA`: the payload is XTEA-encrypted. Defined by `libcsp` v1.6 and by no V2 header —
/// the V2 flags field is six bits wide and `master` leaves this one unassigned.
const FLAG_XTEA: u8 = 0x04;

/// `CSP_FRDP`: the payload carries a five-octet RDP trailer. See [`CspHeader::parse`].
const FLAG_RDP: u8 = 0x02;

/// `CSP_FCRC32`: a big-endian CRC-32C is appended to the payload.
const FLAG_CRC32: u8 = 0x01;

/// Bytes of CRC-32C appended when [`FLAG_CRC32`] is set.
const CRC32_BYTES: usize = 4;

impl CspHeader {
    /// Reads the header and returns it with the payload behind it.
    ///
    /// The payload borrows `bytes`. Nothing here copies: a CSP packet carrying a CCSDS space
    /// packet is the case this exists for, and the pipeline hands the returned slice straight
    /// to packet assembly.
    ///
    /// When [`CspHeader::has_crc32`] is set the last four octets are the checksum, big-endian
    /// (`csp_crc32_append` in `libcsp`'s `src/csp_crc32.c` converts with `htobe32`), and they
    /// are stripped from the payload. They are checked over the header *and* the payload
    /// first and over the payload alone second, which is not a guess: `csp_crc32_verify`
    /// does exactly that, in that order, because CSP 2.1 changed the covered range and a
    /// receiver has to accept both.
    ///
    /// # Errors
    ///
    /// [`CspError::TooShort`] when the bytes cannot hold the header, or the checksum the
    /// flags promise; [`CspError::BadChecksum`] when the CRC-32C does not match; and
    /// [`CspError::Unsupported`] for a packet this crate will not interpret — an HMAC it
    /// cannot verify, encryption it cannot undo, a fragment it will not reassemble.
    pub fn parse(bytes: &[u8], version: CspVersion) -> Result<(Self, &[u8]), CspError> {
        let header_bytes = version.header_bytes();
        if bytes.len() < header_bytes {
            return Err(CspError::TooShort {
                needed: header_bytes,
                found: bytes.len(),
            });
        }
        let header = Self::read(bytes, version);
        if header.has_hmac() {
            return Err(CspError::Unsupported { what: "HMAC" });
        }
        if header.is_encrypted() {
            return Err(CspError::Unsupported { what: "encryption" });
        }
        if header.is_fragmented() {
            return Err(CspError::Unsupported {
                what: "fragmentation",
            });
        }
        if header.uses_rdp() {
            // Refused rather than passed through. RDP appends a five-octet trailer to the
            // payload (`CSP_RDP_HEADER_SIZE`, libcsp's `csp_types.h`), so handing the payload
            // on whole gives the packet stage five octets that are not a packet, and it
            // desynchronises on the first one — silently, because a length field read out of
            // a trailer is still a number. Stripping it instead would need RDP's own layout
            // and would still leave a connection protocol whose acknowledgements a
            // receive-only station cannot send.
            return Err(CspError::Unsupported { what: "RDP" });
        }
        if !header.has_crc32() {
            return Ok((header, &bytes[header_bytes..]));
        }
        let Some(split) = bytes
            .len()
            .checked_sub(CRC32_BYTES)
            .filter(|split| *split >= header_bytes)
        else {
            return Err(CspError::TooShort {
                needed: header_bytes + CRC32_BYTES,
                found: bytes.len(),
            });
        };
        let found = u32::from_be_bytes([
            bytes[split],
            bytes[split + 1],
            bytes[split + 2],
            bytes[split + 3],
        ]);
        let with_header = crc32c(&bytes[..split]);
        if found != with_header && found != crc32c(&bytes[header_bytes..split]) {
            return Err(CspError::BadChecksum {
                expected: with_header,
                found,
            });
        }
        Ok((header, &bytes[header_bytes..split]))
    }

    /// Unpacks the header fields, the caller having checked the length.
    ///
    /// `libcsp` `src/csp_id.c`: V1 is a 32-bit big-endian word, V2 a 48-bit one read into the
    /// low six octets of a `u64` — which is what `csp_id2_extract`'s `be64toh(...) >> 16`
    /// arrives at, with the offsets below applying unchanged.
    fn read(bytes: &[u8], version: CspVersion) -> Self {
        let byte = |index: usize| bytes.get(index).copied().unwrap_or(0);
        match version {
            CspVersion::V1 => {
                let word = u32::from_be_bytes([byte(0), byte(1), byte(2), byte(3)]);
                Self {
                    version,
                    priority: ((word >> 30) & 0x3) as u8,
                    source: ((word >> 25) & 0x1F) as u16,
                    destination: ((word >> 20) & 0x1F) as u16,
                    destination_port: ((word >> 14) & 0x3F) as u8,
                    source_port: ((word >> 8) & 0x3F) as u8,
                    flags: (word & 0xFF) as u8,
                }
            }
            CspVersion::V2 => {
                let word = u64::from_be_bytes([
                    0,
                    0,
                    byte(0),
                    byte(1),
                    byte(2),
                    byte(3),
                    byte(4),
                    byte(5),
                ]);
                Self {
                    version,
                    priority: ((word >> 46) & 0x3) as u8,
                    destination: ((word >> 32) & 0x3FFF) as u16,
                    source: ((word >> 18) & 0x3FFF) as u16,
                    destination_port: ((word >> 12) & 0x3F) as u8,
                    source_port: ((word >> 6) & 0x3F) as u8,
                    flags: (word & 0x3F) as u8,
                }
            }
        }
    }

    /// Which header layout this was read as.
    #[must_use]
    pub const fn version(self) -> CspVersion {
        self.version
    }

    /// Priority, two bits, zero being the most urgent.
    #[must_use]
    pub const fn priority(self) -> u8 {
        self.priority
    }

    /// Source address.
    #[must_use]
    pub const fn source(self) -> u16 {
        self.source
    }

    /// Destination address — the ground, on a downlink.
    #[must_use]
    pub const fn destination(self) -> u16 {
        self.destination
    }

    /// Source port: which service on the spacecraft sent this.
    #[must_use]
    pub const fn source_port(self) -> u8 {
        self.source_port
    }

    /// Destination port.
    #[must_use]
    pub const fn destination_port(self) -> u8 {
        self.destination_port
    }

    /// The raw flag bits, for a `probe` that wants to print them.
    #[must_use]
    pub const fn flags(self) -> u8 {
        self.flags
    }

    /// Whether a CRC-32C is appended to the payload. `CSP_FCRC32`, both versions.
    #[must_use]
    pub fn has_crc32(self) -> bool {
        self.flags & FLAG_CRC32 != 0
    }

    /// Whether an HMAC is appended. This crate refuses such a packet rather than ignoring it.
    ///
    /// `CSP_FHMAC`, both versions.
    #[must_use]
    pub fn has_rdp(self) -> bool {
        self.flags & FLAG_RDP != 0
    }

    /// Whether the sender set `CSP_FHMAC`.
    #[must_use]
    pub fn has_hmac(self) -> bool {
        self.flags & FLAG_HMAC != 0
    }

    /// Whether the payload is encrypted.
    ///
    /// `CSP_FXTEA`, and V1 only: `libcsp` `master` dropped the define, the V2 flags field is
    /// six bits, and no V2 bit replaced it. Always `false` for [`CspVersion::V2`], so that a
    /// V2 packet whose bit `0x04` is set for some mission-local reason is not refused as
    /// encrypted.
    #[must_use]
    pub fn is_encrypted(self) -> bool {
        self.version == CspVersion::V1 && self.flags & FLAG_XTEA != 0
    }

    /// Whether this is one fragment of a larger message. `CSP_FFRAG`, both versions.
    ///
    /// TODO(gs-link-csp): refusing every fragment drops legitimate telemetry on a mission
    /// that uses SFP, `libcsp`'s fragmentation protocol. SFP carries its own header inside
    /// the payload and reassembly is what is missing; deciding it needs `csp_sfp.c` and a
    /// policy for a fragment whose siblings never arrive.
    #[must_use]
    pub fn is_fragmented(self) -> bool {
        self.flags & FLAG_FRAGMENT != 0
    }

    /// Whether the payload carries an RDP trailer. `CSP_FRDP`, both versions.
    ///
    /// Reported and not refused — see [`CspHeader::parse`] for what that costs.
    #[must_use]
    pub fn uses_rdp(self) -> bool {
        self.flags & FLAG_RDP != 0
    }
}

/// The reflected Castagnoli polynomial, `0x1EDC_6F41` reversed.
const CRC32C_POLYNOMIAL: u32 = 0x82F6_3B78;

/// The byte-at-a-time table, the same 256 words `libcsp`'s `src/csp_crc32.c` writes out.
///
/// Built here rather than pasted, and checked against four of that file's entries in the
/// tests: a transcribed table is 256 chances to make a typo that only shows up on one byte
/// value in 256.
const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut word = index as u32;
        let mut bit = 0;
        while bit < 8 {
            word = if word & 1 == 0 {
                word >> 1
            } else {
                (word >> 1) ^ CRC32C_POLYNOMIAL
            };
            bit += 1;
        }
        table[index] = word;
        index += 1;
    }
    table
};

/// The CRC-32C that CSP appends when the flag is set.
///
/// CRC-32C — Castagnoli — and **not** the CRC-32 of zip and PNG, which uses a different
/// polynomial and would reject every packet. Which one `libcsp` means is not a matter of
/// opinion: `src/csp_crc32.c` holds `crc_tab[128] == 0x82F63B78`, the reflected Castagnoli
/// polynomial, `csp_crc32_init` starts the register at all ones and `csp_crc32_final` xors
/// with all ones. The check value for the ASCII string `123456789` is therefore `0xE3069283`
/// and not `0xCBF43F26`, and there is a test for it.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for byte in bytes {
        let index = ((crc ^ u32::from(*byte)) & 0xFF) as usize;
        crc = CRC32C_TABLE[index] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// A CSP packet that could not be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum CspError {
    /// Fewer bytes than the header and its trailers need.
    #[error("a CSP packet needs at least {needed} bytes, got {found}")]
    TooShort {
        /// Bytes required.
        needed: usize,
        /// Bytes supplied.
        found: usize,
    },

    /// The appended CRC-32C does not match.
    #[error("CSP checksum is {found:#010x}, computed {expected:#010x}")]
    BadChecksum {
        /// What the packet should have carried.
        expected: u32,
        /// What it carried.
        found: u32,
    },

    /// The packet uses a CSP feature this crate will not interpret.
    #[error("CSP packet uses {what}, which this link does not handle")]
    Unsupported {
        /// The feature: `HMAC`, `encryption`, `fragmentation`.
        what: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CSP 1 header with priority 2, source 5, destination 9, destination port 20, source
    /// port 33 and no flags, packed by hand from the `CSP_ID1_*_OFFSET` constants in
    /// `libcsp`'s `src/csp_id.c` — not by a packer written to agree with the reader below.
    const V1_HEADER: [u8; 4] = [0x8A, 0x95, 0x21, 0x00];

    /// The same, with `CSP_FCRC32` set.
    const V1_HEADER_CRC: [u8; 4] = [0x8A, 0x95, 0x21, 0x01];

    /// A CSP 2 header with priority 2, destination 0x1234, source 0x0ABC, destination port 20,
    /// source port 33 and no flags, packed by hand from the `CSP_ID2_*_OFFSET` constants.
    const V2_HEADER: [u8; 6] = [0x92, 0x34, 0x2A, 0xF1, 0x48, 0x40];

    /// A ten-byte CCSDS space packet: APID 2, four octets of data field.
    const SPACE_PACKET: [u8; 10] = [0x08, 0x02, 0xC0, 0x00, 0x00, 0x03, 0xDE, 0xAD, 0xBE, 0xEF];

    fn framed(header: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn the_crc32c_table_matches_libcsp() {
        // Four entries of `crc_tab` in libcsp's src/csp_crc32.c, including the one that is the
        // polynomial itself.
        assert_eq!(CRC32C_TABLE[0], 0x00000000);
        assert_eq!(CRC32C_TABLE[1], 0xF26B8303);
        assert_eq!(CRC32C_TABLE[128], 0x82F63B78);
        assert_eq!(CRC32C_TABLE[255], 0xAD7D5351);
    }

    #[test]
    fn crc32c_matches_the_published_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        // The CRC-32 of zip and PNG over the same string, which this must not be.
        assert_ne!(crc32c(b"123456789"), 0xCBF4_3F26);
        assert_eq!(crc32c(&[]), 0);
    }

    #[test]
    fn a_v1_header_unpacks_the_fields_in_libcsp_order() {
        let bytes = framed(&V1_HEADER, &SPACE_PACKET);
        let (header, payload) = CspHeader::parse(&bytes, CspVersion::V1).unwrap();
        assert_eq!(header.version(), CspVersion::V1);
        assert_eq!(header.priority(), 2);
        assert_eq!(header.source(), 5);
        assert_eq!(header.destination(), 9);
        assert_eq!(header.destination_port(), 20);
        assert_eq!(header.source_port(), 33);
        assert_eq!(header.flags(), 0);
        // Source and destination differ, so reading them the wrong way round fails here.
        assert_ne!(header.source(), header.destination());
        assert_eq!(payload, &SPACE_PACKET);
    }

    #[test]
    fn a_v2_header_puts_the_destination_before_the_source() {
        let bytes = framed(&V2_HEADER, &SPACE_PACKET);
        let (header, payload) = CspHeader::parse(&bytes, CspVersion::V2).unwrap();
        assert_eq!(header.priority(), 2);
        assert_eq!(header.destination(), 0x1234);
        assert_eq!(header.source(), 0x0ABC);
        assert_eq!(header.destination_port(), 20);
        assert_eq!(header.source_port(), 33);
        assert_eq!(header.flags(), 0);
        assert_eq!(payload.len(), SPACE_PACKET.len());
    }

    #[test]
    fn the_payload_borrows_the_frame_rather_than_copying_it() {
        let bytes = framed(&V1_HEADER, &SPACE_PACKET);
        let (_, payload) = CspHeader::parse(&bytes, CspVersion::V1).unwrap();
        assert_eq!(payload.as_ptr(), bytes[4..].as_ptr());
    }

    #[test]
    fn a_slice_too_short_for_the_header_is_refused() {
        assert_eq!(
            CspHeader::parse(&V1_HEADER[..3], CspVersion::V1),
            Err(CspError::TooShort {
                needed: 4,
                found: 3
            })
        );
        assert_eq!(
            CspHeader::parse(&V2_HEADER[..5], CspVersion::V2),
            Err(CspError::TooShort {
                needed: 6,
                found: 5
            })
        );
        assert_eq!(
            CspHeader::parse(&[], CspVersion::V1),
            Err(CspError::TooShort {
                needed: 4,
                found: 0
            })
        );
    }

    #[test]
    fn a_header_with_an_empty_payload_is_not_an_error() {
        let (header, payload) = CspHeader::parse(&V1_HEADER, CspVersion::V1).unwrap();
        assert_eq!(header.source(), 5);
        assert!(payload.is_empty());
    }

    #[test]
    fn a_crc32_flag_with_no_room_for_the_checksum_is_refused() {
        let bytes = framed(&V1_HEADER_CRC, &[0x01, 0x02, 0x03]);
        assert_eq!(
            CspHeader::parse(&bytes, CspVersion::V1),
            Err(CspError::TooShort {
                needed: 8,
                found: 7
            })
        );
        // Exactly the header, with the flag set: the four checksum octets are not there.
        assert_eq!(
            CspHeader::parse(&V1_HEADER_CRC, CspVersion::V1),
            Err(CspError::TooShort {
                needed: 8,
                found: 4
            })
        );
    }

    #[test]
    fn a_checksum_over_the_payload_alone_is_accepted() {
        let mut bytes = framed(&V1_HEADER_CRC, &SPACE_PACKET);
        bytes.extend_from_slice(&0x971F_10EF_u32.to_be_bytes());
        let (header, payload) = CspHeader::parse(&bytes, CspVersion::V1).unwrap();
        assert!(header.has_crc32());
        assert_eq!(payload, &SPACE_PACKET);
    }

    #[test]
    fn a_checksum_over_the_header_and_the_payload_is_also_accepted() {
        // CSP 2.1 changed the covered range; csp_crc32_verify tries this one first.
        let mut bytes = framed(&V1_HEADER_CRC, &SPACE_PACKET);
        bytes.extend_from_slice(&0xE7BC_A01C_u32.to_be_bytes());
        let (_, payload) = CspHeader::parse(&bytes, CspVersion::V1).unwrap();
        assert_eq!(payload, &SPACE_PACKET);
    }

    #[test]
    fn a_v2_checksum_is_taken_over_the_six_byte_header() {
        // The one combination the V1 tests cannot reach: the covered range and the payload
        // offset both come from `header_bytes`, which is the only thing that differs.
        let mut header = V2_HEADER;
        header[5] |= FLAG_CRC32;
        let mut bytes = framed(&header, &SPACE_PACKET);
        bytes.extend_from_slice(&0xCEA5_8E62_u32.to_be_bytes());
        let (parsed, payload) = CspHeader::parse(&bytes, CspVersion::V2).unwrap();
        assert!(parsed.has_crc32());
        assert_eq!(parsed.destination(), 0x1234);
        assert_eq!(payload, &SPACE_PACKET);

        // And the classic range, payload only, over the same header.
        let mut classic = framed(&header, &SPACE_PACKET);
        classic.extend_from_slice(&0x971F_10EF_u32.to_be_bytes());
        let (_, payload) = CspHeader::parse(&classic, CspVersion::V2).unwrap();
        assert_eq!(payload, &SPACE_PACKET);
    }

    #[test]
    fn a_wrong_checksum_is_refused_with_both_numbers() {
        let mut bytes = framed(&V1_HEADER_CRC, &SPACE_PACKET);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(
            CspHeader::parse(&bytes, CspVersion::V1),
            Err(CspError::BadChecksum {
                expected: 0xE7BC_A01C,
                found: 0,
            })
        );
    }

    #[test]
    fn a_flipped_payload_byte_is_caught() {
        let mut bytes = framed(&V1_HEADER_CRC, &SPACE_PACKET);
        bytes.extend_from_slice(&0x971F_10EF_u32.to_be_bytes());
        bytes[7] ^= 0x01;
        assert!(matches!(
            CspHeader::parse(&bytes, CspVersion::V1),
            Err(CspError::BadChecksum { .. })
        ));
    }

    #[test]
    fn an_hmac_is_refused_rather_than_ignored() {
        let mut header = V1_HEADER;
        header[3] |= FLAG_HMAC;
        let bytes = framed(&header, &SPACE_PACKET);
        assert_eq!(
            CspHeader::parse(&bytes, CspVersion::V1),
            Err(CspError::Unsupported { what: "HMAC" })
        );
    }

    #[test]
    fn encryption_and_fragmentation_are_refused() {
        let mut encrypted = V1_HEADER;
        encrypted[3] |= FLAG_XTEA;
        assert_eq!(
            CspHeader::parse(&framed(&encrypted, &SPACE_PACKET), CspVersion::V1),
            Err(CspError::Unsupported { what: "encryption" })
        );

        let mut fragmented = V1_HEADER;
        fragmented[3] |= FLAG_FRAGMENT;
        assert_eq!(
            CspHeader::parse(&framed(&fragmented, &SPACE_PACKET), CspVersion::V1),
            Err(CspError::Unsupported {
                what: "fragmentation"
            })
        );
    }

    #[test]
    fn a_v2_packet_has_no_encryption_bit() {
        // 0x04 is CSP_FXTEA in libcsp v1.6 and is unassigned in master, whose V2 flags field
        // is six bits wide. A V2 packet carrying it is not refused as encrypted.
        let mut header = V2_HEADER;
        header[5] |= FLAG_XTEA;
        let bytes = framed(&header, &SPACE_PACKET);
        let (parsed, payload) = CspHeader::parse(&bytes, CspVersion::V2).unwrap();
        assert!(!parsed.is_encrypted());
        assert_eq!(parsed.flags(), FLAG_XTEA);
        assert_eq!(payload, &SPACE_PACKET);
    }

    #[test]
    fn an_rdp_packet_is_refused_rather_than_desynchronising_the_stage_behind_it() {
        // RDP appends five octets to the payload (`CSP_RDP_HEADER_SIZE`). Handed on whole,
        // those five reach the packet stage as if they were a packet header, and a length
        // field read out of a trailer is still a number — so the desynchronisation is silent.
        let mut header = V1_HEADER;
        header[3] |= FLAG_RDP;
        let framed = framed(&header, &SPACE_PACKET);
        assert!(CspHeader::read(&framed, CspVersion::V1).uses_rdp());
        assert_eq!(
            CspHeader::parse(&framed, CspVersion::V1),
            Err(CspError::Unsupported { what: "RDP" })
        );
    }

    #[test]
    fn a_ccsds_packet_survives_the_unwrap_intact() {
        let bytes = framed(&V1_HEADER, &SPACE_PACKET);
        let (_, payload) = CspHeader::parse(&bytes, CspVersion::V1).unwrap();
        assert_eq!(crate::packets::declared_length(payload), Some(10));
        assert_eq!(payload.len(), 10);
    }

    #[test]
    fn the_header_length_is_the_one_libcsp_uses() {
        assert_eq!(CspVersion::V1.header_bytes(), 4);
        assert_eq!(CspVersion::V2.header_bytes(), 6);
        assert_eq!(CspVersion::default(), CspVersion::V1);
    }
}
