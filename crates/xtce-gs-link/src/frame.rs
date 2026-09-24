//! CCSDS 132.0-B TM transfer frames.
//!
//! A transfer frame is a fixed-length container with a six-byte primary header, an optional
//! secondary header, a data field carrying space packets, an optional four-byte operational
//! control field and an optional two-byte frame error control field. All of that is fixed for
//! a mission and none of it is self-describing, which is why [`FrameOptions`] is configured
//! rather than detected: a frame with a trailing checksum and one without are the same bytes
//! with the last two read differently, and nothing in the frame says which it is.

use std::fmt;

/// Size of the TM transfer frame primary header.
pub const PRIMARY_HEADER_BYTES: usize = 6;

/// Size of the operational control field.
pub const OCF_BYTES: usize = 4;

/// Size of the frame error control field.
pub const FECF_BYTES: usize = 2;

/// First-header-pointer value meaning the data field holds only idle data.
///
/// The frame is fill: discard it, count it, and leave any partial packet alone.
/// CCSDS 132.0-B-3 §4.1.2.7.6.5.
pub const ONLY_IDLE_DATA: u16 = 0x7FE;

/// First-header-pointer value meaning no packet *starts* in this frame.
///
/// The whole data field is the continuation of a packet that started in an earlier frame.
/// This is not an idle frame, and treating it as one is how a station drops one packet in
/// every few long ones and still reports a clean link. CCSDS 132.0-B-3 §4.1.2.7.6.4.
pub const NO_PACKET_START: u16 = 0x7FF;

/// The transfer frame version number of a TM frame, CCSDS 132.0-B-3 §4.1.2.2.2.
///
/// Two bits. `01` is an AOS transfer frame (CCSDS 732.0-B, not this document), which is a
/// different format read by a different parser — see the TODO on [`TmFrame::vcid`].
const TM_VERSION: u8 = 0;

/// What a mission put in its frames, since the frames do not say.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FrameOptions {
    /// Whether the last two bytes are a frame error control field.
    pub has_fecf: bool,
    /// Whether four bytes before the checksum are an operational control field.
    ///
    /// Configured *and* flagged per frame: the flag in the header says whether this frame
    /// carries one, and this says whether the mission uses them at all. A frame whose flag
    /// disagrees with the configuration is a frame that was not decoded as it was encoded.
    pub has_ocf: bool,
    /// Bytes of insert zone between the primary header and the data field.
    ///
    /// An AOS idea that missions bolt onto TM frames for time or attitude. Zero for a plain
    /// TM frame.
    pub insert_zone: usize,
}

/// A view of one transfer frame, borrowing it.
///
/// Borrowing because a frame is read once, emptied of packets and thrown away, and the
/// packets that come out of it own their own bytes. Nothing keeps a frame.
#[derive(Clone, Copy)]
pub struct TmFrame<'a> {
    bytes: &'a [u8],
    options: FrameOptions,
}

impl<'a> TmFrame<'a> {
    /// Wraps a frame and checks everything `options` says it can check.
    ///
    /// The order of the checks is the order in which one failure makes the next check
    /// meaningless: the length that lets the header be read at all, the version that says
    /// this is a TM frame, the length the header's own flags imply, the secondary header that
    /// moves the data field, the checksum that says the rest of the bits are the transmitted
    /// ones, and only then the pointer into a data field whose bounds are now known.
    ///
    /// CCSDS 132.0-B-3 §4.1.
    ///
    /// # Errors
    ///
    /// [`FrameError::TooShort`] when the bytes cannot hold the configured layout,
    /// [`FrameError::BadVersion`] for anything that is not a TM frame,
    /// [`FrameError::BadSecondaryHeader`] when the declared secondary header does not fit,
    /// [`FrameError::BadChecksum`] when the FECF does not match, and
    /// [`FrameError::BadFirstHeaderPointer`] when the pointer lands outside the data field.
    pub fn parse(bytes: &'a [u8], options: FrameOptions) -> Result<Self, FrameError> {
        // Before the primary header is readable the only minimum available is the configured
        // one; `has_ocf` stands in for a flag that cannot be read yet.
        if bytes.len() < PRIMARY_HEADER_BYTES {
            let configured = PRIMARY_HEADER_BYTES
                .saturating_add(options.insert_zone)
                .saturating_add(if options.has_ocf { OCF_BYTES } else { 0 })
                .saturating_add(if options.has_fecf { FECF_BYTES } else { 0 });
            return Err(FrameError::TooShort {
                needed: configured,
                found: bytes.len(),
            });
        }

        let frame = Self { bytes, options };

        let version = frame.version();
        if version != TM_VERSION {
            return Err(FrameError::BadVersion { version });
        }

        // With the header readable the minimum is exact, except for the secondary header,
        // which needs its own first octet to be present before it can declare a length.
        let fixed = PRIMARY_HEADER_BYTES
            .saturating_add(options.insert_zone)
            .saturating_add(frame.trailer_bytes())
            .saturating_add(usize::from(frame.secondary_header_flag()));
        if bytes.len() < fixed {
            return Err(FrameError::TooShort {
                needed: fixed,
                found: bytes.len(),
            });
        }

        // Everything between the insert zone and the trailers: the secondary header and the
        // data field share it, so a secondary header longer than this has eaten the checksum.
        let available = bytes
            .len()
            .saturating_sub(PRIMARY_HEADER_BYTES)
            .saturating_sub(options.insert_zone)
            .saturating_sub(frame.trailer_bytes());
        let declared = frame.secondary_header_bytes();
        if declared > available {
            return Err(FrameError::BadSecondaryHeader {
                declared,
                available,
            });
        }

        // `fecf` is `None` exactly when the layout has no checksum, so there is nothing to
        // check and nothing to fall back to. CCSDS 132.0-B-3 §4.1.6.
        if let Some(found) = frame.fecf() {
            let split = bytes.len().saturating_sub(FECF_BYTES);
            let expected = crc16_ccitt(bytes.get(..split).unwrap_or_default());
            if expected != found {
                return Err(FrameError::BadChecksum { expected, found });
            }
        }

        // CCSDS 132.0-B-3 §4.1.2.7: the first header pointer is only defined when the
        // synchronisation flag is 0. On a virtual channel access frame those eleven bits are
        // undefined, and refusing the frame for them would drop a frame that is not wrong.
        if !frame.sync_flag() {
            let pointer = frame.first_header_pointer();
            if pointer != ONLY_IDLE_DATA && pointer != NO_PACKET_START {
                let data_field = frame.data_field().len();
                // A pointer equal to the length leaves no room for the header it points at.
                if usize::from(pointer) >= data_field {
                    return Err(FrameError::BadFirstHeaderPointer {
                        pointer,
                        data_field,
                    });
                }
            }
        }

        Ok(frame)
    }

    /// The whole frame, primary header first.
    #[must_use]
    pub const fn bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// The layout this frame was parsed against.
    #[must_use]
    pub const fn options(self) -> FrameOptions {
        self.options
    }

    /// Length of the whole frame in bytes.
    #[must_use]
    pub const fn len(self) -> usize {
        self.bytes.len()
    }

    /// Whether the frame carries no bytes at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bytes.is_empty()
    }

    /// Two octets of the frame read big-endian, or zero where the frame does not reach.
    ///
    /// Every header accessor goes through this, so none of them can index past the slice.
    /// [`TmFrame::parse`] has already refused anything shorter than the primary header, so
    /// the fallback is unreachable for the header words and is there to keep it that way.
    fn header_word(self, index: usize) -> u16 {
        let hi = self.bytes.get(index).copied().unwrap_or(0);
        let lo = self
            .bytes
            .get(index.saturating_add(1))
            .copied()
            .unwrap_or(0);
        u16::from_be_bytes([hi, lo])
    }

    /// Transfer frame version number. `0` is a TM frame; `1` would be AOS.
    ///
    /// Two bits, CCSDS 132.0-B-3 §4.1.2.2.2.
    #[must_use]
    pub fn version(self) -> u8 {
        (self.header_word(0) >> 14) as u8
    }

    /// Spacecraft identifier.
    ///
    /// Ten bits, CCSDS 132.0-B-3 §4.1.2.2.3.
    #[must_use]
    pub fn spacecraft_id(self) -> u16 {
        (self.header_word(0) >> 4) & 0x03FF
    }

    // TODO(gs-link-frame-aos): AOS (CCSDS 732.0-B) is not read here. Its virtual channel
    // identifier is six bits, not three, and VCID 63 is its idle channel — so an AOS frame
    // cannot be read by narrowing this field, and `parse` refuses it at the version check
    // instead of pretending. Adding AOS means a second parser and a `Framing::AosFrames`
    // variant; what has to be decided first is whether the pipeline keeps one frame type with
    // a discriminant or two, which reaches `PacketAssembler` as well.
    /// Virtual channel identifier.
    ///
    /// Three bits, so `0..=7`. CCSDS 132.0-B-3 §4.1.2.3.
    #[must_use]
    pub fn vcid(self) -> u8 {
        ((self.header_word(0) >> 1) & 0x0007) as u8
    }

    /// Whether this frame carries an operational control field.
    ///
    /// CCSDS 132.0-B-3 §4.1.2.4. The flag alone; [`TmFrame::ocf`] is what says whether four
    /// bytes are actually taken out of this frame.
    #[must_use]
    pub fn ocf_flag(self) -> bool {
        self.header_word(0) & 0x0001 == 1
    }

    /// Master channel frame count: every frame from this spacecraft, in order, modulo 256.
    ///
    /// CCSDS 132.0-B-3 §4.1.2.5.
    #[must_use]
    pub fn master_frame_count(self) -> u8 {
        self.bytes.get(2).copied().unwrap_or(0)
    }

    /// Virtual channel frame count: every frame on this virtual channel, modulo 256.
    ///
    /// This is the one packet assembly cares about. A jump in it means frames were lost, and
    /// a packet spanning the jump cannot be completed. CCSDS 132.0-B-3 §4.1.2.6.
    #[must_use]
    pub fn virtual_frame_count(self) -> u8 {
        self.bytes.get(3).copied().unwrap_or(0)
    }

    /// Whether a frame secondary header follows the primary one.
    ///
    /// Bit 32, CCSDS 132.0-B-3 §4.1.2.7.2.
    #[must_use]
    pub fn secondary_header_flag(self) -> bool {
        self.header_word(4) >> 15 == 1
    }

    /// Synchronisation flag: `false` for a data field of packets, `true` for a virtual
    /// channel access service carrying something this crate does not unpack.
    ///
    /// Bit 33, CCSDS 132.0-B-3 §4.1.2.7.3.
    #[must_use]
    pub fn sync_flag(self) -> bool {
        (self.header_word(4) >> 14) & 0x0001 == 1
    }

    /// Packet order flag — which never orders anything.
    ///
    /// Bit 34, CCSDS 132.0-B-3 §4.1.2.7.4: while [`TmFrame::sync_flag`] is *clear* the bit is
    /// reserved for future use by CCSDS and is set to `0`, so a `true` here on a frame
    /// carrying packets is a malformed frame and not a signal. While the sync flag is set its
    /// use is undefined and a `true` means nothing at all.
    #[must_use]
    pub fn packet_order_flag(self) -> bool {
        (self.header_word(4) >> 13) & 0x0001 == 1
    }

    /// Segment length identifier. `0b11` on a frame carrying packets.
    ///
    /// Bits 35–36, CCSDS 132.0-B-3 §4.1.2.7.5: source packet segments are no longer defined,
    /// and `0b11` is the value that denoted their non-use. Undefined while
    /// [`TmFrame::sync_flag`] is set (§4.1.2.7.5, NOTE 2).
    #[must_use]
    pub fn segment_length_id(self) -> u8 {
        ((self.header_word(4) >> 11) & 0x0003) as u8
    }

    // TODO(gs-link-frame-vca): a frame with [`TmFrame::sync_flag`] set carries virtual
    // channel access service data and not packets, so its data field must not be taken apart
    // by `PacketAssembler::push_frame` and its first header pointer must not be used as an
    // offset — `parse` deliberately leaves that field unvalidated there, because CCSDS
    // 132.0-B-3 §4.1.2.7.6.2 leaves it undefined. Test `sync_flag()` before the pointer and
    // before the data field. What has to be decided is whether the pipeline drops such
    // frames, hands them out whole on a channel of their own, or refuses the configuration.
    /// Offset into the data field at which the first packet header starts.
    ///
    /// Compare against [`ONLY_IDLE_DATA`] and [`NO_PACKET_START`] before using it as an
    /// offset; both are values in the same eleven bits and neither is a position.
    ///
    /// These eleven bits are an offset only while [`TmFrame::sync_flag`] is clear. With the
    /// sync flag set they are undefined, [`TmFrame::parse`] does not range-check them, and
    /// this can return any value at all — including one past the end of the data field.
    ///
    /// Bits 37–47, CCSDS 132.0-B-3 §4.1.2.7.6, and the NOTE under §4.1.2.7.6.2 for the
    /// undefined case.
    #[must_use]
    pub fn first_header_pointer(self) -> u16 {
        self.header_word(4) & 0x07FF
    }

    /// Whether the data field is fill.
    ///
    /// True only for [`ONLY_IDLE_DATA`]. Not for [`NO_PACKET_START`], which carries real
    /// bytes belonging to a real packet — see that constant.
    #[must_use]
    pub fn is_idle(self) -> bool {
        !self.sync_flag() && self.first_header_pointer() == ONLY_IDLE_DATA
    }

    /// Whether no packet begins in this frame.
    ///
    /// The whole data field continues the packet from the previous frame on this virtual
    /// channel.
    #[must_use]
    pub fn has_no_packet_start(self) -> bool {
        !self.sync_flag() && self.first_header_pointer() == NO_PACKET_START
    }

    /// Offset of the first byte after the primary header and the insert zone.
    ///
    /// The insert zone is at a fixed offset by definition — that is what makes it insertable
    /// without parsing anything — so the secondary header starts here and not before.
    fn secondary_header_start(self) -> usize {
        PRIMARY_HEADER_BYTES.saturating_add(self.options.insert_zone)
    }

    // TODO(gs-link-frame-sh-zero): CCSDS 132.0-B-3 §4.1.3.1.3 b) gives the secondary header data
    // field 1 to 63 octets, so the six-bit length field should never be zero and a one-octet
    // secondary header is malformed. It is accepted here as one octet rather than refused,
    // because refusing costs the frame and nothing downstream reads the secondary header.
    // Refusing it needs a `FrameError` variant and a decision about whether the frame's packets
    // are still worth having; the same question covers the two version bits of that octet, which
    // §4.1.3.2.2.2 requires to be `00` and which are not checked either.
    /// Length of the whole frame secondary header, its identification octet included.
    ///
    /// CCSDS 132.0-B-3 §4.1.3.2.3.2: the low six bits of the first octet hold one less than
    /// the length of the secondary header, counting that octet. Zero when the flag is clear, and
    /// zero when the octet itself is missing — which [`TmFrame::parse`] has already refused.
    fn secondary_header_bytes(self) -> usize {
        if !self.secondary_header_flag() {
            return 0;
        }
        match self.bytes.get(self.secondary_header_start()) {
            Some(id) => usize::from(id & 0x3F).saturating_add(1),
            None => 0,
        }
    }

    /// Whether four bytes of this frame are an operational control field.
    ///
    /// Both halves have to agree: the mission configured the field into the layout and this
    /// frame's header flagged it. CCSDS 132.0-B-3 §4.1.5 for the field, §4.1.2.4 for the flag.
    fn ocf_present(self) -> bool {
        self.options.has_ocf && self.ocf_flag()
    }

    // TODO(gs-link-frame-ocf-mismatch): a frame whose OCF flag disagrees with
    // `FrameOptions::has_ocf` is decoded with the flag and not reported. It is a real
    // symptom — a mission configured without OCFs whose frames flag one is a mission
    // configured wrongly — but reporting it belongs on the pipeline's event queue, not in a
    // `FrameError` that would drop the frame. What has to be decided is whether the
    // disagreement is per frame (noisy: one line per frame at the frame rate) or latched per
    // virtual channel.
    /// Bytes at the end of the frame that are not data field.
    fn trailer_bytes(self) -> usize {
        let ocf = if self.ocf_present() { OCF_BYTES } else { 0 };
        let fecf = if self.options.has_fecf { FECF_BYTES } else { 0 };
        ocf.saturating_add(fecf)
    }

    /// The frame secondary header, when the flag says there is one.
    ///
    /// Identification octet first: its low six bits are the length this slice has, less one.
    /// CCSDS 132.0-B-3 §4.1.3.
    #[must_use]
    pub fn secondary_header(self) -> Option<&'a [u8]> {
        if !self.secondary_header_flag() {
            return None;
        }
        let start = self.secondary_header_start();
        let end = start.saturating_add(self.secondary_header_bytes());
        self.bytes.get(start..end)
    }

    /// The insert zone, when the layout has one.
    ///
    /// Fixed length and a fixed offset, immediately after the primary header: a mission
    /// inserts time or attitude here without parsing anything else in the frame. Nothing
    /// downstream reads it; it is here so `probe` can print it.
    #[must_use]
    pub fn insert_zone(self) -> Option<&'a [u8]> {
        if self.options.insert_zone == 0 {
            return None;
        }
        self.bytes
            .get(PRIMARY_HEADER_BYTES..self.secondary_header_start())
    }

    /// The bytes the packets are in.
    ///
    /// Everything after the primary header, the insert zone and the frame secondary header,
    /// and before the operational control field and the frame error control field. CCSDS
    /// 132.0-B-3 §4.1.4.
    #[must_use]
    pub fn data_field(self) -> &'a [u8] {
        let start = self
            .secondary_header_start()
            .saturating_add(self.secondary_header_bytes());
        let end = self.bytes.len().saturating_sub(self.trailer_bytes());
        self.bytes.get(start..end).unwrap_or_default()
    }

    /// The operational control field, when this frame carries one.
    ///
    /// Four bytes of CLCW or of mission-defined report. Nothing downstream reads it; it is
    /// here so that `data_field` stops in the right place and so a `probe` can print it.
    /// CCSDS 132.0-B-3 §4.1.5.
    #[must_use]
    pub fn ocf(self) -> Option<&'a [u8]> {
        if !self.ocf_present() {
            return None;
        }
        let end =
            self.bytes
                .len()
                .saturating_sub(if self.options.has_fecf { FECF_BYTES } else { 0 });
        let start = end.checked_sub(OCF_BYTES)?;
        self.bytes.get(start..end)
    }

    /// The frame error control field as it was transmitted, when the layout has one.
    ///
    /// [`TmFrame::parse`] refuses a frame whose checksum does not match, so this is the
    /// checksum of a frame that passed — what `probe` prints, not how a failure is reported.
    /// The two numbers of a failure are in [`FrameError::BadChecksum`], where no `TmFrame`
    /// exists to ask. CCSDS 132.0-B-3 §4.1.6.
    #[must_use]
    pub fn fecf(self) -> Option<u16> {
        if !self.options.has_fecf {
            return None;
        }
        let at = self.bytes.len().checked_sub(FECF_BYTES)?;
        let hi = *self.bytes.get(at)?;
        let lo = *self.bytes.get(at.saturating_add(1))?;
        Some(u16::from_be_bytes([hi, lo]))
    }
}

impl fmt::Debug for TmFrame<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TmFrame")
            .field("spacecraft_id", &self.spacecraft_id())
            .field("vcid", &self.vcid())
            .field("virtual_frame_count", &self.virtual_frame_count())
            .field("first_header_pointer", &self.first_header_pointer())
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// The CCSDS frame error control field checksum.
///
/// Computed over everything in the frame before the checksum itself.
///
/// CCSDS 132.0-B-3 §4.1.6 and the CRC procedures in CCSDS 130.1-G: the generator is
/// x^16 + x^12 + x^5 + 1, the shift register starts at all ones, bits are shifted in most
/// significant first and there is no final inversion. That is the variant usually written
/// CRC-16/CCITT-FALSE, whose published check value over the ASCII digits `123456789` is
/// `0x29B1` — the test below uses it, because the common CRC-16 variants differ only in the
/// initial value and the final xor and every one of them verifies against itself.
#[must_use]
pub fn crc16_ccitt(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x1021
            };
        }
    }
    crc
}

/// A frame that could not be read as the configured layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum FrameError {
    /// The bytes cannot hold the header and the trailers the layout declares.
    #[error("a frame needs at least {needed} bytes for this layout, got {found}")]
    TooShort {
        /// Bytes the layout requires.
        needed: usize,
        /// Bytes supplied.
        found: usize,
    },

    /// The transfer frame version is not the one this parser reads.
    #[error("transfer frame version {version} is not a TM frame")]
    BadVersion {
        /// The version found in the header.
        version: u8,
    },

    /// The secondary header declares a length that does not fit in the frame.
    #[error("frame secondary header declares {declared} bytes, {available} available")]
    BadSecondaryHeader {
        /// Length the header declares, its identification octet included.
        declared: usize,
        /// Bytes left after the primary header and the insert zone, and before the
        /// operational control and frame error control fields — the room the secondary
        /// header and the data field share.
        available: usize,
    },

    /// The frame error control field does not match the frame.
    #[error("frame checksum is {found:#06x}, computed {expected:#06x}")]
    BadChecksum {
        /// What the frame should have carried.
        expected: u16,
        /// What it carried.
        found: u16,
    },

    /// The first header pointer does not point into the data field.
    #[error("first header pointer {pointer} is outside a {data_field}-byte data field")]
    BadFirstHeaderPointer {
        /// The pointer as it appeared in the header.
        pointer: u16,
        /// Length of the data field it was checked against.
        data_field: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        FECF_BYTES, FrameError, FrameOptions, NO_PACKET_START, ONLY_IDLE_DATA,
        PRIMARY_HEADER_BYTES, TmFrame, crc16_ccitt,
    };

    /// The layout of the hand-built frame: everything optional is present.
    const FULL: FrameOptions = FrameOptions {
        has_fecf: true,
        has_ocf: true,
        insert_zone: 4,
    };

    /// Appends the frame error control field so the frame checks out.
    fn seal(mut bytes: Vec<u8>) -> Vec<u8> {
        let crc = crc16_ccitt(&bytes);
        bytes.extend_from_slice(&crc.to_be_bytes());
        bytes
    }

    /// A 32-byte frame with every primary header field that *can* differ from its
    /// neighbours set to a value nothing else has.
    ///
    /// Version 0, spacecraft 0x2AB, virtual channel 5, OCF flagged; master count 0x11,
    /// virtual channel count 0x22; secondary header flagged, sync and order clear, segment
    /// length id 0b11, first header pointer 5. Insert zone 4, secondary header 3, data field
    /// 13, OCF 4, FECF 2.
    ///
    /// Three fields are *not* distinguishable here, for two different reasons. Two of them
    /// are fixed by this frame's clear synchronisation flag: CCSDS 132.0-B-3 §4.1.2.7.4
    /// reserves the packet order flag and sets it to `0`, and §4.1.2.7.5 sets the segment
    /// length identifier to `0b11`. The third is fixed whatever the frame carries —
    /// §4.1.3.2.2.2 requires the two secondary header version bits to be `00` in every frame,
    /// VCA frames included. A mask widened by one bit reads the same answer for each of the
    /// three here, so each gets a fixture of its own below where the bits beside it differ.
    fn distinguishable_frame() -> Vec<u8> {
        let mut bytes = vec![0x2A, 0xBB, 0x11, 0x22, 0x98, 0x05];
        bytes.extend_from_slice(&[0xE0, 0xE1, 0xE2, 0xE3]);
        bytes.extend_from_slice(&[0x02, 0xC1, 0xC2]);
        bytes.extend(0xD0_u8..=0xDC);
        bytes.extend_from_slice(&[0x0C, 0x0F, 0x00, 0x00]);
        seal(bytes)
    }

    /// A 16-byte frame with no insert zone, no secondary header and no OCF: 8 data bytes.
    fn plain_frame(status: u16) -> Vec<u8> {
        let mut bytes = vec![0x00, 0x00, 0x07, 0x08];
        bytes.extend_from_slice(&status.to_be_bytes());
        bytes.extend_from_slice(&[0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7]);
        seal(bytes)
    }

    const PLAIN: FrameOptions = FrameOptions {
        has_fecf: true,
        has_ocf: false,
        insert_zone: 0,
    };

    #[test]
    fn every_primary_header_field_reads_back_as_it_was_built() {
        let bytes = distinguishable_frame();
        assert_eq!(bytes.len(), 32);
        let frame = TmFrame::parse(&bytes, FULL).expect("the frame is well formed");

        assert_eq!(frame.version(), 0);
        assert_eq!(frame.spacecraft_id(), 0x2AB);
        assert_eq!(frame.vcid(), 5);
        assert!(frame.ocf_flag());
        assert_eq!(frame.master_frame_count(), 0x11);
        assert_eq!(frame.virtual_frame_count(), 0x22);
        assert!(frame.secondary_header_flag());
        assert!(!frame.sync_flag());
        assert!(!frame.packet_order_flag());
        assert_eq!(frame.segment_length_id(), 0b11);
        assert_eq!(frame.first_header_pointer(), 5);
        assert!(!frame.is_idle());
        assert!(!frame.has_no_packet_start());
        assert_eq!(frame.len(), 32);
        assert!(!frame.is_empty());
        assert_eq!(frame.options(), FULL);
        assert_eq!(frame.bytes(), &bytes[..]);
    }

    #[test]
    fn the_data_field_skips_the_insert_zone_and_the_secondary_header_at_once() {
        let bytes = distinguishable_frame();
        let frame = TmFrame::parse(&bytes, FULL).expect("the frame is well formed");

        assert_eq!(frame.insert_zone(), Some(&[0xE0, 0xE1, 0xE2, 0xE3][..]));
        assert_eq!(frame.secondary_header(), Some(&[0x02, 0xC1, 0xC2][..]));
        assert_eq!(frame.ocf(), Some(&[0x0C, 0x0F, 0x00, 0x00][..]));
        assert_eq!(
            frame.data_field(),
            &[
                0xD0, 0xD1, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xDB, 0xDC
            ][..]
        );
        // 6 + 4 + 3 + 13 + 4 + 2.
        assert_eq!(frame.data_field().len(), 13);
        let expected = crc16_ccitt(&bytes[..bytes.len() - FECF_BYTES]);
        assert_eq!(frame.fecf(), Some(expected));
    }

    #[test]
    fn a_frame_one_byte_short_is_refused() {
        let bytes = distinguishable_frame();
        let short = &bytes[..bytes.len() - 1];
        // The layout is fixed, so a missing byte does not shorten the data field — it moves
        // the checksum, and the frame is refused for the checksum it now appears to carry.
        assert!(matches!(
            TmFrame::parse(short, FULL),
            Err(FrameError::BadChecksum { .. })
        ));
    }

    #[test]
    fn a_frame_that_cannot_hold_its_own_trailers_is_refused_by_length() {
        // Sixteen bytes, OCF flagged, insert zone 4, FECF: 6 + 4 + 4 + 2 + 1 = 17 needed.
        let mut bytes = vec![0x00, 0x01, 0x00, 0x00, 0x80, 0x00];
        bytes.extend_from_slice(&[0x00; 10]);
        assert_eq!(
            TmFrame::parse(&bytes, FULL).unwrap_err(),
            FrameError::TooShort {
                needed: 17,
                found: 16
            }
        );
    }

    #[test]
    fn a_slice_too_small_for_a_primary_header_is_refused() {
        assert_eq!(
            TmFrame::parse(&[], FULL).unwrap_err(),
            FrameError::TooShort {
                needed: PRIMARY_HEADER_BYTES + 4 + 4 + 2,
                found: 0
            }
        );
        assert_eq!(
            TmFrame::parse(&[0x00; 5], PLAIN).unwrap_err(),
            FrameError::TooShort {
                needed: PRIMARY_HEADER_BYTES + 2,
                found: 5
            }
        );
    }

    #[test]
    fn a_frame_whose_checksum_is_wrong_is_refused_with_both_numbers() {
        let mut bytes = distinguishable_frame();
        bytes[12] ^= 0xFF;
        let transmitted = u16::from_be_bytes([bytes[30], bytes[31]]);
        let recomputed = crc16_ccitt(&bytes[..30]);

        match TmFrame::parse(&bytes, FULL) {
            Err(FrameError::BadChecksum { expected, found }) => {
                assert_eq!(found, transmitted);
                assert_eq!(expected, recomputed);
                assert_ne!(expected, found);
            }
            other => panic!("expected a checksum failure, got {other:?}"),
        }
    }

    #[test]
    fn an_idle_frame_is_idle_and_is_not_a_continuation() {
        let bytes = plain_frame(0x1800 | ONLY_IDLE_DATA);
        let frame = TmFrame::parse(&bytes, PLAIN).expect("an idle frame is a valid frame");

        assert_eq!(frame.first_header_pointer(), ONLY_IDLE_DATA);
        assert!(frame.is_idle());
        assert!(!frame.has_no_packet_start());
        assert_eq!(frame.data_field().len(), 8);
        assert_eq!(frame.secondary_header(), None);
        assert_eq!(frame.insert_zone(), None);
        assert_eq!(frame.ocf(), None);
    }

    #[test]
    fn a_frame_with_no_packet_start_is_not_an_idle_frame() {
        let bytes = plain_frame(0x1800 | NO_PACKET_START);
        let frame = TmFrame::parse(&bytes, PLAIN).expect("a continuation frame is a valid frame");

        assert!(frame.has_no_packet_start());
        assert!(!frame.is_idle());
        assert_eq!(
            frame.data_field(),
            &[0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7][..]
        );
    }

    #[test]
    fn a_first_header_pointer_at_or_past_the_data_field_is_refused() {
        let last = plain_frame(0x1800 | 7);
        assert!(TmFrame::parse(&last, PLAIN).is_ok());

        let past = plain_frame(0x1800 | 8);
        assert_eq!(
            TmFrame::parse(&past, PLAIN).unwrap_err(),
            FrameError::BadFirstHeaderPointer {
                pointer: 8,
                data_field: 8
            }
        );
    }

    #[test]
    fn a_virtual_channel_access_frame_keeps_its_undefined_pointer() {
        // Sync flag set: the eleven bits are undefined and may hold anything at all.
        let bytes = plain_frame(0x4000 | 0x1800 | 0x07A0);
        let frame = TmFrame::parse(&bytes, PLAIN).expect("the pointer is undefined, not wrong");

        assert!(frame.sync_flag());
        assert_eq!(frame.first_header_pointer(), 0x07A0);
        assert!(!frame.is_idle());
        assert!(!frame.has_no_packet_start());
    }

    /// `distinguishable_frame` has the packet order flag and the synchronisation flag beside
    /// it both clear, so a read one bit wide of its neighbour answers correctly there. This
    /// frame separates them, which CCSDS 132.0-B-3 §4.1.2.7.4 permits only this way round:
    /// the flag's use is undefined while the sync flag is set, and it is reserved and `0`
    /// while the sync flag is clear. Setting the sync flag makes this a virtual channel
    /// access frame, whose data field TODO(gs-link-frame-vca) is the open decision about —
    /// nothing here reads past the header word.
    #[test]
    fn the_packet_order_flag_is_not_the_synchronisation_flag_beside_it() {
        // Sync set, packet order clear, segment length id 0b11, pointer 0.
        let bytes = plain_frame(0x4000 | 0x1800);
        let frame =
            TmFrame::parse(&bytes, PLAIN).expect("parse does not range-check a set sync flag");

        assert!(frame.sync_flag());
        assert!(!frame.packet_order_flag());
    }

    /// The segment length identifier's two bits sit directly below the packet order flag, and
    /// `distinguishable_frame` has the identifier at `0b11` with the flag clear — so a mask
    /// widened into the flag answers correctly there. Here the flag is set and the identifier
    /// is not, which CCSDS 132.0-B-3 §4.1.2.7.4 and §4.1.2.7.5 NOTE 2 both allow while the
    /// synchronisation flag is set, because both fields are undefined on a VCA frame — the
    /// frame kind TODO(gs-link-frame-vca) is still open about. Only the header word is read.
    #[test]
    fn the_segment_length_identifier_does_not_reach_into_the_packet_order_flag() {
        // Sync set, packet order set, segment length id 0b01, pointer 0.
        let bytes = plain_frame(0x4000 | 0x2000 | 0x0800);
        let frame =
            TmFrame::parse(&bytes, PLAIN).expect("parse does not range-check a set sync flag");

        assert!(frame.packet_order_flag());
        assert_eq!(frame.segment_length_id(), 0b01);
    }

    /// The secondary header length is the low six bits of the identification octet and the
    /// two above it are a version number, which CCSDS 132.0-B-3 §4.1.3.2.2.2 requires to be
    /// `00` — so on every other fixture in this file a mask widened into them reads the same
    /// length. This octet is `0x42`: version `01`, length field 2, three octets in all.
    /// Accepting it is TODO(gs-link-frame-sh-zero)'s open decision not to check those bits;
    /// what this pins is that the length is read from its own six and no more.
    #[test]
    fn the_secondary_header_length_does_not_reach_into_the_version_bits() {
        let mut bytes = vec![0x00, 0x00, 0x00, 0x00, 0x98, 0x00];
        bytes.extend_from_slice(&[0x42, 0xC1, 0xC2]);
        bytes.extend_from_slice(&[0xB0, 0xB1, 0xB2, 0xB3, 0xB4]);
        let bytes = seal(bytes);
        let frame =
            TmFrame::parse(&bytes, PLAIN).expect("parse does not check the SH version bits");

        assert_eq!(frame.secondary_header(), Some(&[0x42, 0xC1, 0xC2][..]));
        assert_eq!(frame.data_field(), &[0xB0, 0xB1, 0xB2, 0xB3, 0xB4][..]);
    }

    #[test]
    fn a_frame_that_is_not_version_zero_is_refused() {
        let mut bytes = plain_frame(0x1800);
        bytes[0] |= 0x40;
        assert_eq!(
            TmFrame::parse(&bytes, PLAIN).unwrap_err(),
            FrameError::BadVersion { version: 1 }
        );
    }

    #[test]
    fn a_secondary_header_longer_than_the_frame_is_refused() {
        // Flagged, declaring 64 octets, in a frame with 8 bytes between header and checksum.
        let mut bytes = vec![0x00, 0x00, 0x00, 0x00, 0x80, 0x00];
        bytes.push(0x3F);
        bytes.extend_from_slice(&[0x00; 7]);
        let bytes = seal(bytes);
        assert_eq!(
            TmFrame::parse(&bytes, PLAIN).unwrap_err(),
            FrameError::BadSecondaryHeader {
                declared: 64,
                available: 8
            }
        );
    }

    #[test]
    fn an_unflagged_operational_control_field_stays_in_the_data_field() {
        // Configured for OCFs, but this frame does not flag one, so `ocf_present` takes the
        // flag and the four bytes stay in the data field. That is what this crate does, not
        // what CCSDS 132.0-B-3 settles: §4.1.2.4.3 makes the flag static within the channel
        // for a whole mission phase, so a frame that clears it on an OCF channel is a fault
        // and the station should say so. Reporting it is TODO(gs-link-frame-ocf-mismatch),
        // still open — the disagreement is silent here by decision, not by the standard.
        let options = FrameOptions {
            has_fecf: true,
            has_ocf: true,
            insert_zone: 0,
        };
        let bytes = plain_frame(0x1800);
        let frame = TmFrame::parse(&bytes, options).expect("the flag is clear, so there is no OCF");

        assert!(!frame.ocf_flag());
        assert_eq!(frame.ocf(), None);
        assert_eq!(frame.data_field().len(), 8);
    }

    #[test]
    fn a_layout_without_a_checksum_reads_the_last_two_bytes_as_data() {
        // The same sixteen bytes as every other plain frame, with the trailer configured
        // away: two bytes more of data field and no checksum to report or to check.
        let options = FrameOptions {
            has_fecf: false,
            has_ocf: false,
            insert_zone: 0,
        };
        let bytes = plain_frame(0x1800 | 1);
        let frame = TmFrame::parse(&bytes, options).expect("there is no checksum to fail");

        assert_eq!(frame.fecf(), None);
        assert_eq!(frame.data_field().len(), 10);
        assert_eq!(frame.data_field(), &bytes[PRIMARY_HEADER_BYTES..]);

        // And the same bytes with a wrong checksum are accepted, because nothing says these
        // last two are a checksum.
        let mut damaged = bytes.clone();
        damaged[15] ^= 0xFF;
        assert!(TmFrame::parse(&damaged, options).is_ok());
        assert!(TmFrame::parse(&damaged, PLAIN).is_err());
    }

    #[test]
    fn the_checksum_matches_the_published_check_value() {
        assert_eq!(crc16_ccitt(b"123456789"), 0x29B1);
    }

    #[test]
    fn the_checksum_of_nothing_is_the_initial_register() {
        // The three common CRC-16 variants differ in the initial value and the final xor, and
        // this is the input that separates them: 0xFFFF is init all-ones with no final xor.
        assert_eq!(crc16_ccitt(&[]), 0xFFFF);
    }

    #[test]
    fn a_sealed_frame_checks_out_against_its_own_trailer() {
        let bytes = plain_frame(0x1800 | 1);
        let split = bytes.len() - FECF_BYTES;
        assert_eq!(
            crc16_ccitt(&bytes[..split]),
            u16::from_be_bytes([bytes[split], bytes[split + 1]])
        );
    }
}
