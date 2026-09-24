//! The legacy 255-bit CCSDS pseudo-randomiser.
//!
//! A receiver recovers its bit clock from transitions in the signal, and a transfer frame
//! full of zeroes has none. So CCSDS 131.0-B exclusive-ors the frame with a fixed
//! pseudo-random sequence before transmission, which guarantees transitions whatever the
//! payload is, and the ground undoes it by doing exactly the same thing again.
//!
//! There are two such sequences and this module is the second of them. CCSDS 131.0-B-5
//! §10.4.1 *requires* a 131071-bit sequence of degree seventeen; §10.4.2 allows a 255-bit
//! sequence of degree eight to be generated instead, "for backward compatibility with legacy
//! systems". What is below is the §10.4.2 one, and only that one — see the TODO under
//! [`SEQUENCE_LENGTH`] for what the other would cost and why it is not here.
//!
//! That is the whole of it: the operation is its own inverse, and [`randomize`] and
//! [`derandomize`] are two names for one function. Both exist because a test that builds a
//! frame the way a spacecraft would should read as `randomize`, and a pipeline undoing it
//! should read as `derandomize`, and a reader of either should not have to work out which
//! direction the XOR was meant to go.
//!
//! # Bit order
//!
//! CCSDS 131.0-B-5 §10.4.2 gives the generator as `h(x) = x^8 + x^7 + x^5 + x^3 + 1`, and §10.4.3
//! the all-ones initial state. That alone does not determine a byte table: a shift register can
//! be written in Fibonacci or Galois form, can tap either end, and the bits it produces can be
//! packed into a byte most- or least-significant first. Those choices give different tables from
//! the same polynomial, and only one of them is the one on the wire.
//!
//! The one that is: an eight-bit state, all ones to start, output taken from **bit 7**, the
//! state shifted **left**, and the feedback `s7 ^ s4 ^ s2 ^ s0` shifted in at bit 0. Writing
//! `s7` as `x_n` and `s0` as `x_{n+7}`, that recurrence is `x_{n+8} = x_{n+7} + x_{n+5} +
//! x_{n+3} + x_n`, which is `h(x)` and nothing else. The first bit out exclusive-ors the
//! **most** significant bit of the first byte, because CCSDS 131.0-B transmits a byte
//! most-significant bit first.
//!
//! Getting that wrong is not a subtle failure — it produces a table that starts with some
//! other byte than `FF` — so [`sequence`] is checked against the start the Blue Book prints.
//! That is 40 bits, `FF 48 0E C0 9A` and no more (§10.4.3, NOTE 2), which is why the tests at
//! the bottom of this file pin the rest of the period against an independent published table
//! instead.
//!
//! # Period
//!
//! `h(x)` is primitive, so the bit sequence repeats every 255 bits, as §10.4.3 says it shall. 255
//! is odd, so the *byte* sequence repeats every `lcm(255, 8) / 8 = 255` bytes: the table below is
//! one whole period and the index wraps at 255, not at 256. Every byte value except `0x00`
//! appears in it exactly once, which is what a maximal-length sequence of degree 8 looks like
//! when it is sliced into bytes.

// TODO(gs-link-derand-degree-17): the sequence CCSDS 131.0-B-5 §10.4.1 requires —
// `h(x) = x^17 + x^14 + 1`, 131071 bits, the generator seeded `11000111000111000` per
// §10.4.3 — is not implemented, and a mission that uses it cannot be received here. That is
// a decision and not an oversight: the one mission this station is built against uses the
// §10.4.2 sequence below, and the first thing the other one needs is not in this file. What
// has to be decided is the configuration point — `Framing::TmFrames`'s `derandomize: bool`
// (pipeline.rs) and the `--derandomize` flag behind it (xtce-gs-cli/src/args.rs) are a
// boolean where three states are wanted: off, the 255-bit sequence, the 131071-bit one. Both
// are public, so widening them is an API change and rule 1's kind of decision. The code
// itself costs more than a second table: 131071 is odd, so the *byte* period is 131071 bytes,
// 128 KiB of `.rodata` against the 255 below, enough that the degree-17 generator should run
// its shift register per byte instead — which gives `derandomize` a state to carry and stops
// it being the offset-free function its own contract depends on. Until that is done a
// spacecraft randomised per §10.4.1 is refused rather than silently decoded: the bytes that
// come out of the wrong sequence fail the frame version check or the frame checksum, so they
// are counted as bad frames and never believed.
/// Length of the 255-bit sequence, in bytes.
pub const SEQUENCE_LENGTH: usize = 255;

/// The 255-byte sequence of CCSDS 131.0-B-5 §10.4.2, derived from the shift register at
/// compile time.
///
/// A constant rather than a lazily built table: it is 255 bytes, it never changes, and a
/// `static` behind a lock would cost an atomic per frame to produce a number that was decided
/// in 1984. Private, because [`sequence`] is the accessor the contract names; see the module
/// documentation for the bit order it is derived under.
const SEQUENCE: [u8; SEQUENCE_LENGTH] = build_sequence();

/// Runs the degree-8 shift register of CCSDS 131.0-B-5 §10.4.2 for one full period.
///
/// `const` so the table is in `.rodata` and the derivation is still the thing in the source:
/// a hard-coded array of 255 hex bytes cannot be reviewed against a polynomial, and copying
/// one out of another project's source is how a wrong table spreads.
const fn build_sequence() -> [u8; SEQUENCE_LENGTH] {
    let mut sequence = [0u8; SEQUENCE_LENGTH];
    let mut state: u8 = 0xFF;
    let mut index = 0;
    while index < SEQUENCE_LENGTH {
        let mut byte: u8 = 0;
        let mut bit = 0;
        while bit < 8 {
            // Output first, most significant bit of the byte first.
            byte = (byte << 1) | (state >> 7);
            // x^8 = x^7 + x^5 + x^3 + 1, taken off bits 7, 4, 2 and 0 of the state.
            let feedback = ((state >> 7) ^ (state >> 4) ^ (state >> 2) ^ state) & 1;
            state = (state << 1) | feedback;
            bit += 1;
        }
        sequence[index] = byte;
        index += 1;
    }
    sequence
}

/// The 255-byte sequence, as it is applied to the first 255 bytes of a codeword.
///
/// Exposed so a test can compare it against the first bytes printed in the Blue Book rather
/// than against this crate's own output, which would only prove the implementation agrees
/// with itself.
#[must_use]
pub const fn sequence() -> [u8; SEQUENCE_LENGTH] {
    SEQUENCE
}

/// Undoes the randomiser, in place.
///
/// The sequence restarts at `data[0]`, so `data` must begin at the first byte of a codeword —
/// the byte after the attached sync marker, parity included when the link is Reed-Solomon
/// coded. CCSDS 131.0-B-5 §10.3.2 starts the sequence at the first bit of the codeword and
/// §10.3.4 derandomises from the byte after the marker, which is not itself randomised. A
/// slice that starts anywhere else silently produces noise, which is why this takes no
/// offset: there is no correct value for one.
pub fn derandomize(data: &mut [u8]) {
    // One borrow of the table for the whole slice. Calling `sequence()` per byte would copy
    // 255 bytes per byte of frame.
    let sequence = &SEQUENCE;
    for (index, byte) in data.iter_mut().enumerate() {
        *byte ^= sequence[index % SEQUENCE_LENGTH];
    }
}

/// Applies the randomiser, in place.
///
/// The same operation as [`derandomize`]. It exists for test fixtures and for a future
/// transmit path; nothing on the receive side calls it.
pub fn randomize(data: &mut [u8]) {
    derandomize(data);
}

#[cfg(test)]
mod tests {
    use super::{SEQUENCE, SEQUENCE_LENGTH, derandomize, randomize, sequence};

    /// The sequence's start as the Blue Book prints it: CCSDS 131.0-B-5 §10.4.3, NOTE 2 gives
    /// the first 40 bits of the 255-bit randomiser as `1111 1111 0100 1000 0000 1110 1100 0000
    /// 1001 1010`, leftmost bit first, and that is five octets. Nothing past `0x9A` is printed
    /// there, so nothing past `0x9A` belongs in a constant named for what is printed.
    const PUBLISHED_START: [u8; 5] = [0xFF, 0x48, 0x0E, 0xC0, 0x9A];

    #[test]
    fn the_sequence_starts_with_the_bytes_the_blue_book_prints() {
        assert_eq!(&sequence()[..PUBLISHED_START.len()], &PUBLISHED_START);
    }

    /// The head alone does not pin the taps: a wrong tap set can reproduce the first octets
    /// and diverge later in the period. This is the byte the published tables end on.
    ///
    /// Checked against the independent table in `opensatelliteproject/libsathelper`
    /// (`src/derandomizer.cpp`), whose whole 255 bytes agree with this derivation.
    #[test]
    fn the_sequence_ends_on_the_byte_the_published_tables_end_on() {
        assert_eq!(sequence()[SEQUENCE_LENGTH - 1], 0x58);
        assert_eq!(
            &sequence()[247..],
            &[0x05, 0x08, 0x78, 0xC4, 0x4A, 0x66, 0xF5, 0x58]
        );
    }

    /// `h(x)` is primitive, so the run of 2040 bits is eight whole periods of a maximal-length
    /// sequence and every byte but the all-zero one turns up exactly once. A non-primitive tap
    /// set fails this even when it happens to match the head.
    #[test]
    fn every_byte_value_but_zero_appears_exactly_once() {
        let mut seen = [0u32; 256];
        for byte in sequence() {
            seen[byte as usize] += 1;
        }
        assert_eq!(seen[0], 0, "the all-zero byte cannot occur in the sequence");
        assert!(
            seen[1..].iter().all(|count| *count == 1),
            "every other byte occurs exactly once"
        );
    }

    #[test]
    fn the_sequence_wraps_at_two_hundred_and_fifty_five_not_two_hundred_and_fifty_six() {
        let mut data = vec![0u8; SEQUENCE_LENGTH + 4];
        derandomize(&mut data);
        assert_eq!(data[SEQUENCE_LENGTH], SEQUENCE[0]);
        assert_eq!(data[SEQUENCE_LENGTH + 1], SEQUENCE[1]);
    }

    #[test]
    fn a_frame_of_zeroes_comes_out_as_the_sequence_itself() {
        let mut data = [0u8; 16];
        derandomize(&mut data);
        assert_eq!(&data[..PUBLISHED_START.len()], &PUBLISHED_START);
    }

    #[test]
    fn derandomizing_a_randomized_frame_gives_the_frame_back() {
        let original: Vec<u8> = (0..1000u32).map(|i| (i * 37 % 251) as u8).collect();
        let mut data = original.clone();
        randomize(&mut data);
        assert_ne!(
            data, original,
            "the randomiser must actually change the bytes"
        );
        derandomize(&mut data);
        assert_eq!(data, original);
    }

    /// The claim the module makes in prose, asserted: there is one operation, not two.
    #[test]
    fn randomize_and_derandomize_are_the_same_operation() {
        let original: Vec<u8> = (0..600u32).map(|i| (i % 256) as u8).collect();
        let mut randomized = original.clone();
        let mut derandomized = original.clone();
        randomize(&mut randomized);
        derandomize(&mut derandomized);
        assert_eq!(randomized, derandomized);
    }

    #[test]
    fn an_empty_slice_is_left_alone() {
        let mut data: [u8; 0] = [];
        derandomize(&mut data);
        assert_eq!(data, [] as [u8; 0]);
    }

    /// A codeword longer than one period repeats the sequence rather than running off it.
    #[test]
    fn the_sequence_repeats_after_a_whole_period() {
        let mut data = vec![0u8; SEQUENCE_LENGTH * 2];
        derandomize(&mut data);
        let (first, second) = data.split_at(SEQUENCE_LENGTH);
        assert_eq!(first, second);
    }
}
