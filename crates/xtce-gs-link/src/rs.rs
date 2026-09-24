//! Reed-Solomon over GF(256), as CCSDS 131.0-B specifies it.
//!
//! The code is RS(255, 223) — 223 information symbols and 32 parity symbols per codeword,
//! correcting up to 16 symbol errors — or its shortened sibling RS(255, 239) with 16 parity
//! symbols and a correction capacity of 8. Several codewords are interleaved across one
//! transfer frame so that a burst of errors is spread thinly over all of them instead of
//! landing entirely inside one: symbol `i` of the codeblock belongs to codeword `i mod I`, so
//! the block is `k * I` information symbols followed by `2E * I` check symbols, CCSDS
//! 131.0-B-5 §4.4.1.
//!
//! # The parameters, none of which are the textbook ones
//!
//! CCSDS 131.0-B-5 §4.3 fixes four things, and a decoder that guesses any of them agrees with
//! itself and with nothing on a real downlink:
//!
//! * The field generator is `F(x) = x^8 + x^7 + x^2 + x + 1` — `0x187`, not the `0x11D` of
//!   every CD-ROM and QR-code implementation.
//! * The code generator is `g(x) = product over j of (x - alpha^(11j))`, `j` running from
//!   `128 - E` to `127 + E`. The roots are spaced 11 apart, not 1, and they do not start at
//!   `alpha^0` or `alpha^1`. The range is written around 128 so that `g(x)` comes out
//!   self-reciprocal, which is what let the 1980s hardware encoders share a multiplier; the
//!   tests check that property rather than trusting the arithmetic here.
//! * `E = 16` gives the first root 112 and `E = 8` gives 120. **The first root is not 112 for
//!   both codes.** Using 112 for RS(255, 239) builds a perfectly good Reed-Solomon code that
//!   is not the one CCSDS defines.
//! * The symbols on the link are in the dual basis, below.
//!
//! # The dual basis, and what it does and does not move
//!
//! CCSDS transmits Berlekamp's dual-basis representation of GF(256) rather than the
//! conventional polynomial one, because the dual basis is what lets a transmitter compute the
//! parity bit-serially out of the data stream as it stands. That is the whole point of it, and
//! it settles the question a reader asks next: **the information octets are not rewritten.**
//! The transfer frame octets on the wire *are* the dual-basis representation of the
//! information symbols. Only the arithmetic changes basis — [`from_dual_basis`] on the way
//! into the syndrome computation, [`to_dual_basis`] on the way back out for the symbols the
//! decoder actually repaired.
//!
//! The Blue Book says so twice. Annex F's figure F-1 puts `T^-1` on the information symbols
//! *entering* a conventional encoder and `T` on the check symbols *leaving* it, with nothing
//! on the information path out; and §4.4.1 closes the interleaving discussion with "the
//! original kI consecutive information symbols that entered the encoder appear unchanged at
//! the output of the encoder with 2EI R-S check symbols appended".
//!
//! So [`ReedSolomon::encode`] copies `data` through untouched and transforms only the parity
//! it computed, and [`ReedSolomon::decode`] leaves every symbol it did not correct exactly as
//! it found it. A decoder that ran the transform across the whole block would hand the frame
//! parser `from_dual_basis` of a primary header, and every frame would fail its version check.
//! This matches Phil Karn's `encode_rs_ccsds`/`decode_rs_ccsds`, which the tests at the bottom
//! of this file compare against byte for byte.
//!
//! # What a decoder must not do
//!
//! Beyond its capacity a Reed-Solomon decoder does not fail cleanly. It can land on a
//! *different* valid codeword and hand back a block that passes every check and is not what
//! was transmitted — `tests::a_constructed_near_miss_is_corrected_into_the_wrong_codeword_and_called_success`
//! builds one on purpose. [`RsError::Uncorrectable`] is therefore reported rather than
//! guessed around, and the depth of each correction is counted, because a link correcting
//! fourteen symbols a frame is a link about to start lying.
//!
//! `LinkStats::rs_symbols` is a running total over the whole run, and on its own it says
//! little: what an operator wants is symbols per *corrected frame*, the ratio that says how
//! close the coding is to giving up. The status bar draws exactly that, beside the frame
//! count and only once something has been corrected, so a clean pass costs the row no width.
//!
//! Three things are checked before a correction is applied, because the cheap ones catch
//! almost everything: the locator degree must not exceed `E`, the Chien search must find as
//! many roots as the locator has degree, and the corrected codeword's syndromes must all come
//! back zero. The last one is the only one that is conclusive, and even it cannot see a
//! received block that landed inside another codeword's decoding sphere — nothing can. It is
//! run only when there was something to correct, so a clean pass pays for the syndromes and
//! nothing else.
//!
//! # The lie this module can tell, and where it is pinned
//!
//! `Ok(0)` from [`ReedSolomon::decode`] means *the block was already a codeword*, and a
//! codeword is not the same thing as the right data. The 255-octet CCSDS pseudo-randomiser
//! sequence is itself a valid RS(255, 223) codeword, and the code is linear, so a codeblock
//! that reached this module without being derandomised first is still a codeword: the decode
//! reports no errors, and every octet it hands on is wrong. Nothing here can detect it — the
//! order the randomiser is undone in is a property of the pipeline, not of the code — which is
//! why `pipeline.rs` pins it in
//! `tests::the_randomiser_sequence_is_a_codeword_so_the_wrong_order_decodes_cleanly_and_lies`.
//! That the sequence is a codeword is an empirical property that test checks, not something a
//! Blue Book section states.

/// Symbols in a codeword, before shortening or interleaving.
pub const CODEWORD_SYMBOLS: usize = 255;

/// Field generator `F(x) = x^8 + x^7 + x^2 + x + 1`, CCSDS 131.0-B-5 §4.3.3.
const FIELD_GENERATOR: u16 = 0x187;

/// Spacing between the roots of `g(x)`, CCSDS 131.0-B-5 §4.3.4. Eleven, not one.
const ROOT_SPACING: usize = 11;

/// The inverse of [`ROOT_SPACING`] modulo 255: `11 * 116 = 1276 = 5 * 255 + 1`.
///
/// The Chien search walks the roots of `lambda(x)` in steps of `alpha`, but a root
/// `alpha^(-11i)` names symbol `i`, so stepping from one symbol position to the next means
/// stepping by this. A decoder that uses 1 here finds the right *number* of errors and
/// repairs the wrong *positions*, which is worse than not repairing them.
const INVERSE_ROOT_SPACING: usize = 116;

/// Index-form stand-in for `log(0)`, which has no value: `alpha^255 = alpha^0 = 1`, so 255 is
/// free to mean "minus infinity". The antilog table maps it back to zero.
const LOG_ZERO: u8 = 255;

/// The most parity symbols any CCSDS code here has, and so the size of every scratch buffer.
const MAX_PARITY: usize = 32;

/// The deepest interleaving CCSDS 131.0-B-5 §4.3.5.1 allows.
const MAX_INTERLEAVE: usize = 8;

/// The most symbols one block can need repaired: `E` per codeword, `I` codewords.
const MAX_CORRECTIONS: usize = (MAX_PARITY / 2) * MAX_INTERLEAVE;

/// The interleaving depths CCSDS 131.0-B-5 §4.3.5.1 defines: six and seven are not among
/// them, and the list is exhaustive rather than a range.
const INTERLEAVE_DEPTHS: [usize; 6] = [1, 2, 3, 4, 5, 8];

/// Every scratch buffer in this module is sized from [`MAX_PARITY`] and [`MAX_INTERLEAVE`],
/// and [`ReedSolomon::ccsds`] is the only thing that keeps a caller inside them. Adding a depth
/// to [`INTERLEAVE_DEPTHS`] without raising [`MAX_INTERLEAVE`] would be an index out of bounds
/// in [`ReedSolomon::encode`] on the first frame of a mission, so it is a build error instead.
/// The parity side needs no such check: `build_generator` of anything past [`MAX_PARITY`]
/// indexes its own array out of bounds and fails to compile.
const _: () = {
    let mut index = 0;
    while index < INTERLEAVE_DEPTHS.len() {
        assert!(INTERLEAVE_DEPTHS[index] <= MAX_INTERLEAVE);
        index += 1;
    }
};

/// Antilog: `ALPHA_TO[i]` is `alpha^i`, with `ALPHA_TO[255] = 0` for the [`LOG_ZERO`] slot.
const ALPHA_TO: [u8; 256] = build_antilog();

/// Log: `INDEX_OF[x]` is the `i` with `alpha^i = x`, and `INDEX_OF[0]` is [`LOG_ZERO`].
const INDEX_OF: [u8; 256] = build_log(&ALPHA_TO);

/// `g(x)` for `E = 8` — RS(255, 239), first root `alpha^(11*120)` — in index form.
const GENERATOR_E8: [u8; MAX_PARITY + 1] = build_generator(16);

/// `g(x)` for `E = 16` — RS(255, 223), first root `alpha^(11*112)` — in index form.
const GENERATOR_E16: [u8; MAX_PARITY + 1] = build_generator(32);

/// Berlekamp's dual basis, as the eight octets that are the images of the eight conventional
/// basis vectors: `[z0..z7] = [u7..u0] T`, CCSDS 131.0-B-5 §4.3.9.3 and annex F2.
///
/// These eight bytes are the rows of the Blue Book's `T` matrix read straight off the page,
/// most significant bit first. The tests check the resulting table against the Blue Book's
/// separately printed inverse matrix and against Phil Karn's libfec.
const DUAL_BASIS: [u8; 8] = [0x8d, 0xef, 0xec, 0x86, 0xfa, 0x99, 0xaf, 0x7b];

/// Conventional basis to dual basis, one lookup per symbol.
const TO_DUAL: [u8; 256] = build_to_dual();

/// Dual basis to conventional basis: [`TO_DUAL`] read backwards.
const FROM_DUAL: [u8; 256] = build_from_dual(&TO_DUAL);

/// Runs the field's shift register for one full period.
///
/// `const` rather than built in [`ReedSolomon::ccsds`] for the reason the randomiser table is
/// one: it is decided by a polynomial printed in 1984, and a table of hex bytes in the source
/// cannot be reviewed against that polynomial.
const fn build_antilog() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut state: u16 = 1;
    let mut power = 0;
    while power < CODEWORD_SYMBOLS {
        table[power] = state as u8;
        state <<= 1;
        if state & 0x100 != 0 {
            state ^= FIELD_GENERATOR;
        }
        state &= 0xFF;
        power += 1;
    }
    // alpha^(-inf) = 0, in the slot LOG_ZERO points at.
    table[LOG_ZERO as usize] = 0;
    table
}

/// Inverts [`build_antilog`]. Zero has no logarithm, so its slot holds [`LOG_ZERO`].
const fn build_log(antilog: &[u8; 256]) -> [u8; 256] {
    let mut table = [0u8; 256];
    table[0] = LOG_ZERO;
    let mut power = 0;
    while power < CODEWORD_SYMBOLS {
        table[antilog[power] as usize] = power as u8;
        power += 1;
    }
    table
}

/// Multiplies out `g(x) = product over j of (x - alpha^(11j))` for `j` in `128-E ..= 127+E`.
///
/// Returned in index (log) form, so the encoder's inner loop multiplies by adding. The tail
/// beyond `parity_symbols` is never read.
const fn build_generator(parity_symbols: usize) -> [u8; MAX_PARITY + 1] {
    let mut poly = [0u8; MAX_PARITY + 1];
    poly[0] = 1;
    let mut root = first_root(parity_symbols) * ROOT_SPACING;
    let mut degree = 0;
    while degree < parity_symbols {
        poly[degree + 1] = 1;
        // Multiply the polynomial so far by (x - alpha^root), coefficients high to low.
        let mut i = degree;
        while i > 0 {
            poly[i] = if poly[i] == 0 {
                poly[i - 1]
            } else {
                poly[i - 1]
                    ^ ALPHA_TO[(INDEX_OF[poly[i] as usize] as usize + root) % CODEWORD_SYMBOLS]
            };
            i -= 1;
        }
        // The constant term is a product of roots and so is never zero.
        poly[0] = ALPHA_TO[(INDEX_OF[poly[0] as usize] as usize + root) % CODEWORD_SYMBOLS];
        root += ROOT_SPACING;
        degree += 1;
    }
    let mut i = 0;
    while i <= parity_symbols {
        poly[i] = INDEX_OF[poly[i] as usize];
        i += 1;
    }
    poly
}

/// The first consecutive root of `g(x)` in index form: `128 - E`, CCSDS 131.0-B-5 §4.3.4.
///
/// 112 for the `E = 16` code and 120 for the `E = 8` one. Writing the relation rather than the
/// two numbers is what stops the second code quietly inheriting the first one's roots.
const fn first_root(parity_symbols: usize) -> usize {
    128 - parity_symbols / 2
}

/// The dual-basis transform: bit `k` of the input selects `DUAL_BASIS[7 - k]`.
///
/// A linear map over GF(2)^8, so the table is the sum of the basis images the input's set bits
/// select, and one table lookup replaces eight parity computations per symbol.
const fn build_to_dual() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut symbol = 0;
    while symbol < 256 {
        let mut image = 0u8;
        let mut bit = 0;
        while bit < 8 {
            if symbol & (1 << bit) != 0 {
                image ^= DUAL_BASIS[7 - bit];
            }
            bit += 1;
        }
        table[symbol] = image;
        symbol += 1;
    }
    table
}

/// Reads [`build_to_dual`] backwards. The map is invertible, so every slot is written once.
const fn build_from_dual(forward: &[u8; 256]) -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut symbol = 0;
    while symbol < 256 {
        table[forward[symbol] as usize] = symbol as u8;
        symbol += 1;
    }
    table
}

/// Reed-Solomon refused a block.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum RsError {
    /// More symbols are in error than the code can correct.
    #[error("more symbol errors than the code can correct")]
    Uncorrectable,

    /// The block handed in is not the length this code works on.
    #[error("expected a {expected}-byte block, got {found}")]
    BadLength {
        /// Length the code requires.
        expected: usize,
        /// Length that was supplied.
        found: usize,
    },
}

/// Where the Chien search found the roots of `lambda(x)`.
///
/// A struct rather than three out-parameters so that [`ReedSolomon::forney`] stays inside the
/// argument count a reader can hold, and so the invariant `count <= degree` travels with them.
struct ErrorLocations {
    /// Roots of `lambda(x)` in index form, in the order the search met them.
    roots: [u8; MAX_PARITY],
    /// The symbol position each root names.
    positions: [u8; MAX_PARITY],
    /// How many entries of the two above are filled in.
    count: usize,
}

/// A configured CCSDS Reed-Solomon codec.
pub struct ReedSolomon {
    parity_symbols: usize,
    interleave: usize,
    /// `128 - E`, taken once so the syndrome and Forney loops cannot disagree about it.
    first_root: usize,
    alpha_to: [u8; 256],
    index_of: [u8; 256],
    /// `g(x)` in index form, degree `parity_symbols`.
    generator: [u8; MAX_PARITY + 1],
    /// `log(alpha^(11 * (first_root + i)))` for each root, so the syndrome loop multiplies by
    /// adding a byte it already has rather than by reducing a product per symbol per root.
    root_logs: [u8; MAX_PARITY],
}

/// Prints the parameters and not the tables.
///
/// `Pipeline` derives `Debug` and holds one of these; a derive here would put 800 bytes of
/// field tables into every line an operator ever logs.
impl core::fmt::Debug for ReedSolomon {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReedSolomon")
            .field("parity_symbols", &self.parity_symbols)
            .field("interleave", &self.interleave)
            .field("first_root", &self.first_root)
            // The field tables and the generator are 800 bytes of constant and are the
            // reason this impl is not a derive.
            .finish_non_exhaustive()
    }
}

impl ReedSolomon {
    /// A codec for `parity_symbols` parity symbols per codeword, `interleave` codewords deep.
    ///
    /// `None` when the pair is not one CCSDS defines. Not a [`RsError`]: the only two things
    /// that can be wrong are already named in the argument list, and there is nothing a
    /// caller would do with an error type it would not do with `None`. The pipeline turns it
    /// into an event, because a configuration that silently disabled the error correction on
    /// a marginal link would be discovered as a rise in the CRC failure count and blamed on
    /// the weather.
    ///
    /// The accepted pairs are `parity_symbols` of 16 or 32 against an `interleave` of 1, 2, 3,
    /// 4, 5 or 8 — the depths CCSDS 131.0-B-5 §4.3.5.1 lists. Six and seven are refused: the
    /// arithmetic here would carry them, but a mission that asked for one of them has a
    /// configuration error, and accepting it would decode its neighbour's bytes.
    #[must_use]
    pub fn ccsds(parity_symbols: usize, interleave: usize) -> Option<Self> {
        let generator = match parity_symbols {
            16 => GENERATOR_E8,
            32 => GENERATOR_E16,
            _ => return None,
        };
        if !INTERLEAVE_DEPTHS.contains(&interleave) {
            return None;
        }
        let first_root = first_root(parity_symbols);
        let mut root_logs = [0u8; MAX_PARITY];
        for (index, log) in root_logs.iter_mut().enumerate().take(parity_symbols) {
            *log = (((first_root + index) * ROOT_SPACING) % CODEWORD_SYMBOLS) as u8;
        }
        Some(Self {
            parity_symbols,
            interleave,
            first_root,
            alpha_to: ALPHA_TO,
            index_of: INDEX_OF,
            generator,
            root_logs,
        })
    }

    /// Parity symbols per codeword: 16 or 32.
    #[must_use]
    pub const fn parity_symbols(&self) -> usize {
        self.parity_symbols
    }

    /// How many codewords are interleaved across one block.
    #[must_use]
    pub const fn interleave(&self) -> usize {
        self.interleave
    }

    /// Symbol errors per codeword this code can correct: half the parity symbols.
    #[must_use]
    pub const fn correction_capacity(&self) -> usize {
        self.parity_symbols / 2
    }

    /// Length of a whole interleaved block, parity included.
    #[must_use]
    pub const fn block_length(&self) -> usize {
        CODEWORD_SYMBOLS.saturating_mul(self.interleave)
    }

    /// Length of the data part of a block: the transfer frame that comes out of it.
    #[must_use]
    pub const fn data_length(&self) -> usize {
        CODEWORD_SYMBOLS
            .saturating_sub(self.parity_symbols)
            .saturating_mul(self.interleave)
    }

    /// Appends the whole block — data then parity, interleaved and in the dual basis — to
    /// `out`.
    ///
    /// Only tests and a future transmit path call this. It exists because a decoder tested
    /// solely against its own encoder proves nothing, and a decoder tested against no encoder
    /// at all cannot be given a corrupted codeword to repair.
    ///
    /// `data` goes out byte for byte: its octets already *are* the dual-basis representation
    /// of the information symbols, per the module documentation, so only the parity this
    /// computes needs [`to_dual_basis`] applied to it.
    ///
    /// # Errors
    ///
    /// [`RsError::BadLength`] when `data` is not [`ReedSolomon::data_length`] bytes.
    /// `BadLength` rather than a silent pad, because a caller who miscounted the frame length
    /// would otherwise get a block that decodes cleanly to the wrong bytes.
    pub fn encode(&self, data: &[u8], out: &mut Vec<u8>) -> Result<(), RsError> {
        let expected = self.data_length();
        if data.len() != expected {
            return Err(RsError::BadLength {
                expected,
                found: data.len(),
            });
        }
        let parity_symbols = self.parity_symbols;
        let interleave = self.interleave;
        out.extend_from_slice(data);

        let mut block = [0u8; MAX_PARITY * MAX_INTERLEAVE];
        let mut parity = [0u8; MAX_PARITY];
        for lane in 0..interleave {
            parity[..parity_symbols].fill(0);
            // Symbol i of the block belongs to codeword i mod I, so one lane is one stride.
            for symbol in data.iter().skip(lane).step_by(interleave) {
                let feedback = self.index_of[(FROM_DUAL[*symbol as usize] ^ parity[0]) as usize];
                if feedback != LOG_ZERO {
                    // Tap `parity[j]` with `g[parity_symbols - j]`, so the generator is read
                    // from the top down as the register is walked from the bottom up. Both
                    // addends are logs of non-zero elements, so the sum reduces into 0..=254
                    // and the antilog index is in range: no coefficient of a consecutive-root
                    // generator is zero, and the tests check that.
                    let taps = self.generator[1..parity_symbols].iter().rev();
                    for (slot, tap) in parity[1..parity_symbols].iter_mut().zip(taps) {
                        *slot ^=
                            self.alpha_to[(feedback as usize + *tap as usize) % CODEWORD_SYMBOLS];
                    }
                }
                parity.copy_within(1..parity_symbols, 0);
                parity[parity_symbols - 1] = if feedback == LOG_ZERO {
                    0
                } else {
                    self.alpha_to
                        [(feedback as usize + self.generator[0] as usize) % CODEWORD_SYMBOLS]
                };
            }
            for (offset, symbol) in parity[..parity_symbols].iter().enumerate() {
                block[offset * interleave + lane] = TO_DUAL[*symbol as usize];
            }
        }
        out.extend_from_slice(&block[..parity_symbols * interleave]);
        Ok(())
    }

    /// Corrects `block` in place and returns how many symbols were repaired.
    ///
    /// `block` is the whole interleaved block as it came off the link, and on success its
    /// first [`ReedSolomon::data_length`] bytes are the transfer frame. A return of `0` means
    /// the block arrived intact.
    ///
    /// Every symbol the decoder did not repair is left bit for bit as it arrived — see the
    /// module documentation for why the dual-basis transform does not touch the data.
    ///
    /// # Errors
    ///
    /// [`RsError::BadLength`] when `block` is not [`ReedSolomon::block_length`] bytes, and
    /// [`RsError::Uncorrectable`] when the errors exceed what the code can repair. In the
    /// second case `block` is left as it was found: a partially corrected block is not a
    /// frame, and handing one on is how a station reports packets it never received. That is
    /// why the corrections are buffered and applied only once every codeword has succeeded —
    /// with `I` codewords, the last one can fail after the first `I - 1` have been repaired.
    pub fn decode(&self, block: &mut [u8]) -> Result<usize, RsError> {
        let expected = self.block_length();
        if block.len() != expected {
            return Err(RsError::BadLength {
                expected,
                found: block.len(),
            });
        }
        let interleave = self.interleave;
        // Stack scratch, sized by the largest code: the decode path allocates nothing.
        let mut fixes = [(0u16, 0u8); MAX_CORRECTIONS];
        let mut applied = 0;
        let mut codeword = [0u8; CODEWORD_SYMBOLS];
        let mut repaired = [0u8; MAX_PARITY];

        for lane in 0..interleave {
            for (symbol, source) in codeword
                .iter_mut()
                .zip(block[lane..].iter().step_by(interleave))
            {
                *symbol = FROM_DUAL[*source as usize];
            }
            let count = self.correct_codeword(&mut codeword, &mut repaired)?;
            for position in &repaired[..count] {
                let position = *position as usize;
                // 254 * 8 + 7 fits a u16 with room to spare.
                fixes[applied] = (
                    (position * interleave + lane) as u16,
                    TO_DUAL[codeword[position] as usize],
                );
                applied += 1;
            }
        }
        for (position, symbol) in &fixes[..applied] {
            block[*position as usize] = *symbol;
        }
        Ok(applied)
    }

    /// Corrects one de-interleaved codeword in the conventional basis, in place.
    ///
    /// Returns how many symbols changed and writes their positions to `repaired`. The
    /// codeword is scratch owned by [`ReedSolomon::decode`], so leaving it half-corrected on
    /// the error path costs nothing — the block itself is not touched until every codeword
    /// has come back `Ok`.
    fn correct_codeword(
        &self,
        codeword: &mut [u8; CODEWORD_SYMBOLS],
        repaired: &mut [u8; MAX_PARITY],
    ) -> Result<usize, RsError> {
        let mut syndromes = [0u8; MAX_PARITY];
        if !self.syndromes(codeword, &mut syndromes) {
            // The common case on a good pass: the block is already a codeword and nothing
            // past the syndromes has to run.
            return Ok(0);
        }
        let mut lambda = [0u8; MAX_PARITY + 1];
        let degree = self.error_locator(&syndromes, &mut lambda);
        // Non-zero syndromes with a degree-zero locator, or a locator of higher degree than
        // the code can correct, both mean the error pattern is outside the decoding sphere.
        if degree == 0 || degree > self.correction_capacity() {
            return Err(RsError::Uncorrectable);
        }
        let locations = self.chien_search(&lambda, degree);
        // `lambda` has `degree` roots in the field if and only if the errors it describes are
        // real; fewer means the received word is not within `E` of any codeword.
        if locations.count != degree {
            return Err(RsError::Uncorrectable);
        }
        let count = self.forney(codeword, &syndromes, &lambda, degree, &locations, repaired);
        // The conclusive check, and the reason a mis-correction is not reported as success.
        let mut check = [0u8; MAX_PARITY];
        if self.syndromes(codeword, &mut check) {
            return Err(RsError::Uncorrectable);
        }
        Ok(count)
    }

    /// Evaluates the received polynomial at the roots of `g(x)`.
    ///
    /// Returns whether any syndrome was non-zero, and leaves `out` in index (log) form ready
    /// for Berlekamp-Massey. Horner's rule, one pass over the codeword for all of the roots at
    /// once, because the codeword is what does not fit in cache.
    fn syndromes(&self, codeword: &[u8; CODEWORD_SYMBOLS], out: &mut [u8; MAX_PARITY]) -> bool {
        let accumulators = &mut out[..self.parity_symbols];
        accumulators.fill(codeword[0]);
        for symbol in &codeword[1..] {
            for (accumulator, root) in accumulators.iter_mut().zip(self.root_logs.iter()) {
                // Zero has no logarithm, so the multiply is skipped rather than mis-indexed.
                *accumulator = if *accumulator == 0 {
                    *symbol
                } else {
                    *symbol
                        ^ self.alpha_to[(self.index_of[*accumulator as usize] as usize
                            + *root as usize)
                            % CODEWORD_SYMBOLS]
                };
            }
        }
        let mut any = 0u8;
        for accumulator in accumulators.iter_mut() {
            any |= *accumulator;
            *accumulator = self.index_of[*accumulator as usize];
        }
        any != 0
    }

    /// Berlekamp-Massey: the shortest `lambda(x)` whose recurrence generates the syndromes.
    ///
    /// Returns `deg(lambda)` and leaves `lambda` in index (log) form. Euclid's algorithm would
    /// answer the same question; this is the form with no polynomial division in it.
    fn error_locator(
        &self,
        syndromes: &[u8; MAX_PARITY],
        lambda: &mut [u8; MAX_PARITY + 1],
    ) -> usize {
        let parity_symbols = self.parity_symbols;
        lambda[..=parity_symbols].fill(0);
        lambda[0] = 1;
        // The previous locator, shifted, in index form: index form of `lambda(x) = 1`.
        let mut previous = [LOG_ZERO; MAX_PARITY + 1];
        previous[0] = 0;
        let mut candidate = [0u8; MAX_PARITY + 1];
        let mut length = 0;

        for step in 1..=parity_symbols {
            let mut discrepancy = 0u8;
            for (offset, coefficient) in lambda[..step].iter().enumerate() {
                let syndrome = syndromes[step - offset - 1];
                if *coefficient != 0 && syndrome != LOG_ZERO {
                    discrepancy ^= self.alpha_to[(self.index_of[*coefficient as usize] as usize
                        + syndrome as usize)
                        % CODEWORD_SYMBOLS];
                }
            }
            let discrepancy = self.index_of[discrepancy as usize];
            if discrepancy == LOG_ZERO {
                previous.copy_within(0..parity_symbols, 1);
                previous[0] = LOG_ZERO;
                continue;
            }
            // candidate(x) = lambda(x) - discrepancy * x * previous(x)
            candidate[0] = lambda[0];
            for offset in 0..parity_symbols {
                candidate[offset + 1] = if previous[offset] == LOG_ZERO {
                    lambda[offset + 1]
                } else {
                    lambda[offset + 1]
                        ^ self.alpha_to
                            [(discrepancy as usize + previous[offset] as usize) % CODEWORD_SYMBOLS]
                };
            }
            if 2 * length < step {
                length = step - length;
                // previous(x) = lambda(x) / discrepancy
                for (slot, coefficient) in previous[..=parity_symbols]
                    .iter_mut()
                    .zip(lambda[..=parity_symbols].iter())
                {
                    *slot = if *coefficient == 0 {
                        LOG_ZERO
                    } else {
                        ((self.index_of[*coefficient as usize] as usize + CODEWORD_SYMBOLS
                            - discrepancy as usize)
                            % CODEWORD_SYMBOLS) as u8
                    };
                }
            } else {
                previous.copy_within(0..parity_symbols, 1);
                previous[0] = LOG_ZERO;
            }
            lambda[..=parity_symbols].copy_from_slice(&candidate[..=parity_symbols]);
        }

        let mut degree = 0;
        for (offset, coefficient) in lambda[..=parity_symbols].iter_mut().enumerate() {
            *coefficient = self.index_of[*coefficient as usize];
            if *coefficient != LOG_ZERO {
                degree = offset;
            }
        }
        degree
    }

    /// Chien search: every field element tried as a root of `lambda(x)`.
    ///
    /// Stops as soon as `degree` roots have been found, because a locator of degree `d` has at
    /// most `d` of them and the remaining positions cannot change the answer. The position a
    /// root names steps by [`INVERSE_ROOT_SPACING`], not by one: the roots are `alpha^(-11i)`.
    fn chien_search(&self, lambda: &[u8; MAX_PARITY + 1], degree: usize) -> ErrorLocations {
        let mut found = ErrorLocations {
            roots: [0u8; MAX_PARITY],
            positions: [0u8; MAX_PARITY],
            count: 0,
        };
        let mut register = [0u8; MAX_PARITY + 1];
        register[1..=degree].copy_from_slice(&lambda[1..=degree]);
        let mut position = INVERSE_ROOT_SPACING - 1;

        for root in 1..=CODEWORD_SYMBOLS {
            // lambda[0] is always alpha^0 = 1, so the sum starts there.
            let mut sum = 1u8;
            for power in (1..=degree).rev() {
                if register[power] != LOG_ZERO {
                    register[power] = ((register[power] as usize + power) % CODEWORD_SYMBOLS) as u8;
                    sum ^= self.alpha_to[register[power] as usize];
                }
            }
            if sum == 0 {
                // The search visits each of the 255 positions once, so no position repeats.
                found.roots[found.count] = root as u8;
                found.positions[found.count] = position as u8;
                found.count += 1;
                if found.count == degree {
                    break;
                }
            }
            position = (position + INVERSE_ROOT_SPACING) % CODEWORD_SYMBOLS;
        }
        found
    }

    /// Forney's formula: the magnitude of the error at each located position.
    ///
    /// Applies each correction to `codeword`, records the positions that actually changed in
    /// `repaired`, and returns how many there were. A located position whose magnitude comes
    /// out zero was not in error, so it is not counted — the caller's number is symbols
    /// repaired, not roots found.
    ///
    /// It cannot fail. Karn's `decode_rs.h` refuses here when the formal derivative of
    /// `lambda` vanishes at a root — a repeated root, describing no real error pattern — but
    /// this decoder has already excluded that: it runs only when the Chien search returned
    /// `degree` roots, and the search steps through each of the 255 positions exactly once,
    /// so the roots it reports are distinct, `lambda` splits into distinct linear factors and
    /// its derivative is non-zero at every one of them. The conclusive guard against a
    /// correction that does not produce a codeword is the syndrome re-check in
    /// [`ReedSolomon::correct_codeword`], which runs whatever this returns.
    fn forney(
        &self,
        codeword: &mut [u8; CODEWORD_SYMBOLS],
        syndromes: &[u8; MAX_PARITY],
        lambda: &[u8; MAX_PARITY + 1],
        degree: usize,
        locations: &ErrorLocations,
        repaired: &mut [u8; MAX_PARITY],
    ) -> usize {
        // omega(x) = syndromes(x) * lambda(x) mod x^parity_symbols, in index form.
        let omega_degree = degree - 1;
        let mut omega = [0u8; MAX_PARITY + 1];
        for term in 0..=omega_degree {
            let mut sum = 0u8;
            for (offset, coefficient) in lambda[..=term].iter().enumerate() {
                let syndrome = syndromes[term - offset];
                if syndrome != LOG_ZERO && *coefficient != LOG_ZERO {
                    sum ^= self.alpha_to
                        [(syndrome as usize + *coefficient as usize) % CODEWORD_SYMBOLS];
                }
            }
            omega[term] = self.index_of[sum as usize];
        }

        let mut count = 0;
        for index in 0..locations.count {
            let root = locations.roots[index] as usize;
            // Numerator: omega evaluated at the inverse of the error locator.
            let mut numerator = 0u8;
            for (power, coefficient) in omega[..=omega_degree].iter().enumerate() {
                if *coefficient != LOG_ZERO {
                    numerator ^=
                        self.alpha_to[(*coefficient as usize + power * root) % CODEWORD_SYMBOLS];
                }
            }
            // The generator does not start at alpha^1, so the magnitude is scaled by the
            // locator raised to first_root - 1. Dropping this is the classic FCR bug.
            let scale = self.alpha_to[(root * (self.first_root - 1)) % CODEWORD_SYMBOLS];
            // Denominator: the formal derivative of lambda, which over GF(2) is its odd
            // coefficients, evaluated at the same point.
            let mut derivative = 0u8;
            // Karn clamps this to `parity_symbols - 1` because his `deg_lambda` is not
            // bounded and would index past the array. `correct_codeword` has already refused
            // `degree > correction_capacity()`, which is `parity_symbols / 2`, so the clamp
            // here could never choose its second argument.
            let mut power = degree & !1;
            loop {
                if lambda[power + 1] != LOG_ZERO {
                    derivative ^= self.alpha_to
                        [(lambda[power + 1] as usize + power * root) % CODEWORD_SYMBOLS];
                }
                if power < 2 {
                    break;
                }
                power -= 2;
            }
            if numerator != 0 {
                let position = locations.positions[index] as usize;
                // Every logarithm here is of a non-zero element, so each is at most 254 and
                // the sum reduces into 0..=254.
                codeword[position] ^= self.alpha_to[(self.index_of[numerator as usize] as usize
                    + self.index_of[scale as usize] as usize
                    + CODEWORD_SYMBOLS
                    - self.index_of[derivative as usize] as usize)
                    % CODEWORD_SYMBOLS];
                repaired[count] = position as u8;
                count += 1;
            }
        }
        count
    }
}

// TODO(gs-link-rs-erasures): erasure decoding is not here. A demodulator that reports which
// symbols it is unsure of lets the code correct 2E erasures instead of E errors, which is most
// of the coding gain on a fading link. Adding it means threading an erasure position list into
// `correct_codeword`, seeding `lambda` with the erasure locator polynomial before
// Berlekamp-Massey and starting the recursion at `r = no_eras` — Phil Karn's `decode_rs.h` has
// the shape. It needs a decision first: `Source` carries bytes and nothing else today, so
// there is nowhere for a per-symbol confidence to come from, and inventing one would mean
// changing the link's input type.

// TODO(gs-link-rs-shortened): virtual fill is not here. CCSDS 131.0-B-5 §4.3.7 allows a
// shortened codeblock and §4.3.8.2 fixes the fill: all zeros, never transmitted, only at the
// beginning of the codeblock, and only in whole multiples of 8I bits. Missions that want a
// frame shorter than `k * I` use it. The arithmetic is a `pad` count of leading symbols that
// are known zero, subtracted from every Chien position and skipped in the syndrome loop, as in
// Karn's `PAD`. It needs a configuration field first: `RsConfig` has no way to say how much
// fill, and "the block was short so the difference must be fill" is wrong for any mission that
// sends a short frame inside a full block.

/// Converts a symbol from the conventional basis into the dual basis CCSDS transmits in.
///
/// Applied to the parity an encoder computed, and to the symbols a decoder repaired. It is not
/// applied to the information symbols, because their octets on the wire are already the
/// dual-basis ones — see the module documentation.
#[must_use]
pub fn to_dual_basis(symbol: u8) -> u8 {
    TO_DUAL[symbol as usize]
}

/// Converts a received symbol out of the dual basis into the conventional basis the
/// arithmetic is done in.
#[must_use]
pub fn from_dual_basis(symbol: u8) -> u8 {
    FROM_DUAL[symbol as usize]
}

#[cfg(test)]
mod tests {
    use super::{
        ALPHA_TO, CODEWORD_SYMBOLS, FROM_DUAL, GENERATOR_E8, GENERATOR_E16, INDEX_OF,
        INTERLEAVE_DEPTHS, INVERSE_ROOT_SPACING, LOG_ZERO, ROOT_SPACING, ReedSolomon, RsError,
        TO_DUAL, first_root, from_dual_basis, to_dual_basis,
    };

    // The four constants below were produced by Phil Karn's libfec — the reference every
    // amateur and several professional CCSDS ground stations decode real downlinks with — and
    // not by anything in this repository. They are here rather than fetched at test time so
    // the check survives the network being gone. To reproduce:
    //
    //   for f in gen_ccsds_tal.c init_rs_char.c encode_rs_char.c decode_rs_char.c \
    //            char.h rs-common.h init_rs.h encode_rs.h decode_rs.h; do
    //     curl -sO https://raw.githubusercontent.com/quiet/libfec/master/$f
    //   done
    //
    // `gen_ccsds_tal.c` prints the two tables. The codeword came from `encode_rs_char` with
    // `init_rs_char(8, 0x187, 112, 11, 32, 0)` wrapped exactly as `encode_rs_ccsds.c` wraps
    // it: the message octets are the ones on the wire, converted to the conventional basis on
    // the way in, and only the parity is converted back on the way out.
    const KARN_TALTAB: [u8; 256] = [
        0x00, 0x7b, 0xaf, 0xd4, 0x99, 0xe2, 0x36, 0x4d, 0xfa, 0x81, 0x55, 0x2e, 0x63, 0x18, 0xcc,
        0xb7, 0x86, 0xfd, 0x29, 0x52, 0x1f, 0x64, 0xb0, 0xcb, 0x7c, 0x07, 0xd3, 0xa8, 0xe5, 0x9e,
        0x4a, 0x31, 0xec, 0x97, 0x43, 0x38, 0x75, 0x0e, 0xda, 0xa1, 0x16, 0x6d, 0xb9, 0xc2, 0x8f,
        0xf4, 0x20, 0x5b, 0x6a, 0x11, 0xc5, 0xbe, 0xf3, 0x88, 0x5c, 0x27, 0x90, 0xeb, 0x3f, 0x44,
        0x09, 0x72, 0xa6, 0xdd, 0xef, 0x94, 0x40, 0x3b, 0x76, 0x0d, 0xd9, 0xa2, 0x15, 0x6e, 0xba,
        0xc1, 0x8c, 0xf7, 0x23, 0x58, 0x69, 0x12, 0xc6, 0xbd, 0xf0, 0x8b, 0x5f, 0x24, 0x93, 0xe8,
        0x3c, 0x47, 0x0a, 0x71, 0xa5, 0xde, 0x03, 0x78, 0xac, 0xd7, 0x9a, 0xe1, 0x35, 0x4e, 0xf9,
        0x82, 0x56, 0x2d, 0x60, 0x1b, 0xcf, 0xb4, 0x85, 0xfe, 0x2a, 0x51, 0x1c, 0x67, 0xb3, 0xc8,
        0x7f, 0x04, 0xd0, 0xab, 0xe6, 0x9d, 0x49, 0x32, 0x8d, 0xf6, 0x22, 0x59, 0x14, 0x6f, 0xbb,
        0xc0, 0x77, 0x0c, 0xd8, 0xa3, 0xee, 0x95, 0x41, 0x3a, 0x0b, 0x70, 0xa4, 0xdf, 0x92, 0xe9,
        0x3d, 0x46, 0xf1, 0x8a, 0x5e, 0x25, 0x68, 0x13, 0xc7, 0xbc, 0x61, 0x1a, 0xce, 0xb5, 0xf8,
        0x83, 0x57, 0x2c, 0x9b, 0xe0, 0x34, 0x4f, 0x02, 0x79, 0xad, 0xd6, 0xe7, 0x9c, 0x48, 0x33,
        0x7e, 0x05, 0xd1, 0xaa, 0x1d, 0x66, 0xb2, 0xc9, 0x84, 0xff, 0x2b, 0x50, 0x62, 0x19, 0xcd,
        0xb6, 0xfb, 0x80, 0x54, 0x2f, 0x98, 0xe3, 0x37, 0x4c, 0x01, 0x7a, 0xae, 0xd5, 0xe4, 0x9f,
        0x4b, 0x30, 0x7d, 0x06, 0xd2, 0xa9, 0x1e, 0x65, 0xb1, 0xca, 0x87, 0xfc, 0x28, 0x53, 0x8e,
        0xf5, 0x21, 0x5a, 0x17, 0x6c, 0xb8, 0xc3, 0x74, 0x0f, 0xdb, 0xa0, 0xed, 0x96, 0x42, 0x39,
        0x08, 0x73, 0xa7, 0xdc, 0x91, 0xea, 0x3e, 0x45, 0xf2, 0x89, 0x5d, 0x26, 0x6b, 0x10, 0xc4,
        0xbf,
    ];

    const KARN_TAL1TAB: [u8; 256] = [
        0x00, 0xcc, 0xac, 0x60, 0x79, 0xb5, 0xd5, 0x19, 0xf0, 0x3c, 0x5c, 0x90, 0x89, 0x45, 0x25,
        0xe9, 0xfd, 0x31, 0x51, 0x9d, 0x84, 0x48, 0x28, 0xe4, 0x0d, 0xc1, 0xa1, 0x6d, 0x74, 0xb8,
        0xd8, 0x14, 0x2e, 0xe2, 0x82, 0x4e, 0x57, 0x9b, 0xfb, 0x37, 0xde, 0x12, 0x72, 0xbe, 0xa7,
        0x6b, 0x0b, 0xc7, 0xd3, 0x1f, 0x7f, 0xb3, 0xaa, 0x66, 0x06, 0xca, 0x23, 0xef, 0x8f, 0x43,
        0x5a, 0x96, 0xf6, 0x3a, 0x42, 0x8e, 0xee, 0x22, 0x3b, 0xf7, 0x97, 0x5b, 0xb2, 0x7e, 0x1e,
        0xd2, 0xcb, 0x07, 0x67, 0xab, 0xbf, 0x73, 0x13, 0xdf, 0xc6, 0x0a, 0x6a, 0xa6, 0x4f, 0x83,
        0xe3, 0x2f, 0x36, 0xfa, 0x9a, 0x56, 0x6c, 0xa0, 0xc0, 0x0c, 0x15, 0xd9, 0xb9, 0x75, 0x9c,
        0x50, 0x30, 0xfc, 0xe5, 0x29, 0x49, 0x85, 0x91, 0x5d, 0x3d, 0xf1, 0xe8, 0x24, 0x44, 0x88,
        0x61, 0xad, 0xcd, 0x01, 0x18, 0xd4, 0xb4, 0x78, 0xc5, 0x09, 0x69, 0xa5, 0xbc, 0x70, 0x10,
        0xdc, 0x35, 0xf9, 0x99, 0x55, 0x4c, 0x80, 0xe0, 0x2c, 0x38, 0xf4, 0x94, 0x58, 0x41, 0x8d,
        0xed, 0x21, 0xc8, 0x04, 0x64, 0xa8, 0xb1, 0x7d, 0x1d, 0xd1, 0xeb, 0x27, 0x47, 0x8b, 0x92,
        0x5e, 0x3e, 0xf2, 0x1b, 0xd7, 0xb7, 0x7b, 0x62, 0xae, 0xce, 0x02, 0x16, 0xda, 0xba, 0x76,
        0x6f, 0xa3, 0xc3, 0x0f, 0xe6, 0x2a, 0x4a, 0x86, 0x9f, 0x53, 0x33, 0xff, 0x87, 0x4b, 0x2b,
        0xe7, 0xfe, 0x32, 0x52, 0x9e, 0x77, 0xbb, 0xdb, 0x17, 0x0e, 0xc2, 0xa2, 0x6e, 0x7a, 0xb6,
        0xd6, 0x1a, 0x03, 0xcf, 0xaf, 0x63, 0x8a, 0x46, 0x26, 0xea, 0xf3, 0x3f, 0x5f, 0x93, 0xa9,
        0x65, 0x05, 0xc9, 0xd0, 0x1c, 0x7c, 0xb0, 0x59, 0x95, 0xf5, 0x39, 0x20, 0xec, 0x8c, 0x40,
        0x54, 0x98, 0xf8, 0x34, 0x2d, 0xe1, 0x81, 0x4d, 0xa4, 0x68, 0x08, 0xc4, 0xdd, 0x11, 0x71,
        0xbd,
    ];

    const KARN_MESSAGE: &str = "dc0465aa1fad1d5adae5ac1b1e5f1370796cfd10ff19af601d04acb41d022b46\
        78733af2df5faeb70859d1ee3910cb4895b5cc892911ff06b6622edf3cf935fd\
        4b9428ca097c44b3025e965fb3ea6dacd42d816e69afe0e6874c9c04e7d2365d\
        2c60c9eaf479f686a0eb9326e46212d50dcbb377156a6a3a68ba8edb7408469e\
        f3ceb30af8d0dd68bbf85ffa24f2d2fc1887fb5c87bab43832a59b1b3d107cf7\
        78d67fe26df81191297e9395cb12c557ce5af1d41618d719bc045b7e9965f1a2\
        9471c42aac6aa938c475c7ad3238021f053b2c991afceb15decf68bae07cbc";

    const KARN_PARITY: &str = "a52b7b78c3a32a8315f0c1bd3b4aeb32e68728b5944bcc51fd18dad18e19ea4e";

    const KARN_CORRUPTED: &str = "230565aa1fad1ddadae5ac1b1e5f1370796cfd10ff19af601d04acb41d022b1c\
        78733af2df5faeb70859d1ee3910cb4895b5cc892911ff06b6622edf3cf935fd\
        ee9428ca097c44b3025e965fb3ea6dacd42d816e69afe0e6874c9c04e7d2365d\
        2c60c9d6f479f686a0eb9326e46212d50dcbb377156a6a3a68ba8edb7408469e\
        30ceb30af8d0dd68bbf85ffa24f2d2fc1887fb5c87babb3832a59b1b3d107cf7\
        78d67fe26df81191297e9395cb12c557ce5af1d41618d719bc045b7e9965f1a2\
        9471c42aac6aa9c8c475c7ad3238021f053b2c991afceb15decf68bae07cc242\
        337b78c3a32a0215f0c1bd3b4aeb32e6a328b5944bcc51fd18da938e19ead7";

    /// The 1103515245/12345 linear congruential generator, the same one that produced the
    /// libfec vectors above. A test needs repeatable bytes and the workspace has no `rand`.
    struct Lcg(u32);

    impl Lcg {
        fn next_byte(&mut self) -> u8 {
            self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (self.0 >> 16) as u8
        }

        fn message(&mut self, length: usize) -> Vec<u8> {
            (0..length).map(|_| self.next_byte()).collect()
        }

        /// A mask that always changes the symbol it is applied to. A test that corrupts by
        /// assigning a random byte corrupts one position in 256 not at all, and then the
        /// "exactly E errors" assertion is flaky one run in sixteen.
        fn next_mask(&mut self) -> u8 {
            let mask = self.next_byte();
            if mask == 0 { 0xFF } else { mask }
        }
    }

    fn from_hex(text: &str) -> Vec<u8> {
        let digits: Vec<u32> = text
            .chars()
            .filter(|character| !character.is_whitespace())
            .map(|character| character.to_digit(16).expect("hex digit"))
            .collect();
        digits
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[high, low]| ((high << 4) | low) as u8)
            .collect()
    }

    fn codec(parity_symbols: usize, interleave: usize) -> ReedSolomon {
        ReedSolomon::ccsds(parity_symbols, interleave).expect("a pair CCSDS defines")
    }

    /// An encoded block of pseudo-random data, and the data that went into it.
    fn encoded(code: &ReedSolomon, seed: u32) -> (Vec<u8>, Vec<u8>) {
        let mut random = Lcg(seed);
        let data = random.message(code.data_length());
        let mut block = Vec::new();
        code.encode(&data, &mut block).expect("the right length");
        assert_eq!(block.len(), code.block_length());
        (data, block)
    }

    /// Asserts that a block `decode` returned `Ok` for really is a codeword.
    ///
    /// This is what a success from a Reed-Solomon decoder claims and the only part of the
    /// claim a test can check: re-encode the data half of what came back and the parity must
    /// come back identical. Asserting instead that the result differs from the transmitted
    /// block is a tautology — `Ok` bounds the changes by `E`, the received word was `E + 1`
    /// away, so the triangle inequality already settles it — and a decoder that dropped its
    /// final syndrome re-check would pass that assertion while handing back rubbish.
    ///
    /// It cannot see a received word that landed inside *another* codeword's decoding sphere;
    /// nothing can. That is the case the caller's `E + 1` doc describes.
    fn assert_is_a_codeword(code: &ReedSolomon, block: &[u8]) {
        let mut reencoded = Vec::new();
        code.encode(&block[..code.data_length()], &mut reencoded)
            .expect("the decoder returned a block of the length it was given");
        assert_eq!(
            reencoded.as_slice(),
            block,
            "decode reported success on a block whose parity is not its data's"
        );
    }

    #[test]
    fn the_dual_basis_table_matches_phil_karns_taltab() {
        assert_eq!(TO_DUAL, KARN_TALTAB);
    }

    #[test]
    fn the_inverse_dual_basis_table_matches_phil_karns_tal1tab() {
        assert_eq!(FROM_DUAL, KARN_TAL1TAB);
    }

    /// The rows of `T` and of `T^-1` as CCSDS 131.0-B-5 §4.3.9.3 and annex F2 print them,
    /// most significant bit first. `T` is [`super::DUAL_BASIS`]; `T^-1` is printed separately
    /// in the Blue Book and is not derived from `T` anywhere in this crate, so rebuilding
    /// [`from_dual_basis`] out of it is an independent check of the inversion.
    const BLUE_BOOK_INVERSE: [u8; 8] = [0xc5, 0x42, 0x2e, 0xfd, 0xf0, 0x79, 0xac, 0xcc];

    #[test]
    fn the_inverse_transform_is_the_blue_books_own_inverse_matrix() {
        for symbol in 0..=u8::MAX {
            // [u7..u0] = [z0..z7] T^-1: bit k of the dual octet selects row 7-k.
            let mut image = 0u8;
            for bit in 0..8u8 {
                if symbol & (1 << bit) != 0 {
                    image ^= BLUE_BOOK_INVERSE[7 - bit as usize];
                }
            }
            assert_eq!(from_dual_basis(symbol), image, "for {symbol:#04x}");
        }
    }

    /// Both directions, because a table built backwards passes a one-directional test: it is
    /// still an involution pair, just the wrong one.
    #[test]
    fn the_dual_basis_transforms_undo_each_other_in_both_directions() {
        for symbol in 0..=u8::MAX {
            assert_eq!(to_dual_basis(from_dual_basis(symbol)), symbol);
            assert_eq!(from_dual_basis(to_dual_basis(symbol)), symbol);
        }
    }

    /// The transform is linear and invertible over GF(2)^8, so it permutes the 256 octets. A
    /// table with a repeated entry would still round-trip for the values it happened to cover.
    #[test]
    fn the_dual_basis_transform_is_a_permutation_of_every_byte() {
        let mut seen = [false; 256];
        for symbol in 0..=u8::MAX {
            seen[to_dual_basis(symbol) as usize] = true;
        }
        assert!(seen.iter().all(|hit| *hit));
        assert_eq!(to_dual_basis(0), 0, "a linear map fixes zero");
    }

    /// CCSDS 131.0-B-5 §4.3.4 writes the root range around 128 — `j` from `128-E` to
    /// `127+E` — and annex F2 then calls the result self-reciprocal in its own words. So this
    /// is a structural check on arithmetic the Blue Book already decided, not a substitute for
    /// reading it. It earns its place on the `E = 8` code, which libfec has no CCSDS codec for:
    /// reusing 112 as the first root for both codes builds a perfectly good RS(255, 239) that
    /// fails this, and the byte-for-byte comparison below cannot see it.
    #[test]
    fn the_generators_are_self_reciprocal_as_the_root_range_makes_them() {
        assert_eq!(first_root(32), 112);
        assert_eq!(first_root(16), 120);
        for (parity_symbols, generator) in [(16usize, &GENERATOR_E8), (32, &GENERATOR_E16)] {
            for offset in 0..=parity_symbols {
                assert_eq!(
                    generator[offset],
                    generator[parity_symbols - offset],
                    "g(x) for E = {} is not self-reciprocal at {offset}",
                    parity_symbols / 2
                );
            }
        }
    }

    /// The encoder reads the generator in index form and adds, which silently multiplies by
    /// one where a coefficient is zero. No generator with consecutive roots has a zero
    /// coefficient, and this is where that assumption is written down.
    #[test]
    fn no_generator_coefficient_is_zero() {
        for (parity_symbols, generator) in [(16usize, &GENERATOR_E8), (32, &GENERATOR_E16)] {
            assert!(
                generator[..=parity_symbols]
                    .iter()
                    .all(|coefficient| *coefficient != LOG_ZERO)
            );
        }
    }

    #[test]
    fn eleven_has_an_inverse_modulo_two_hundred_and_fifty_five_and_it_is_one_hundred_and_sixteen() {
        assert_eq!(ROOT_SPACING * INVERSE_ROOT_SPACING % CODEWORD_SYMBOLS, 1);
    }

    /// `F(x) = 0x187` has to be primitive or the shift register visits fewer than 255 values
    /// and the log table has holes in it.
    #[test]
    fn the_field_tables_invert_each_other_over_the_whole_field() {
        assert_eq!(ALPHA_TO[0], 1);
        assert_eq!(ALPHA_TO[LOG_ZERO as usize], 0);
        assert_eq!(INDEX_OF[0], LOG_ZERO);
        for power in 0..CODEWORD_SYMBOLS {
            assert_eq!(INDEX_OF[ALPHA_TO[power] as usize] as usize, power);
        }
        let mut seen = [false; 256];
        for power in 0..CODEWORD_SYMBOLS {
            seen[ALPHA_TO[power] as usize] = true;
        }
        assert_eq!(seen.iter().filter(|hit| **hit).count(), 255);
        assert!(!seen[0], "zero is not a power of alpha");
    }

    /// The whole of the encoder in one assertion: field, generator, root range, the LFSR, the
    /// basis transform on the parity and the absence of one on the data. A table comparison
    /// alone would not catch a wrong first root.
    #[test]
    fn a_codeword_matches_phil_karns_encoder_byte_for_byte() {
        let code = codec(32, 1);
        let message = from_hex(KARN_MESSAGE);
        let parity = from_hex(KARN_PARITY);
        assert_eq!(message.len(), 223);
        assert_eq!(parity.len(), 32);

        let mut block = Vec::new();
        code.encode(&message, &mut block).expect("223 bytes");
        assert_eq!(&block[..223], &message[..], "the data is not transformed");
        assert_eq!(&block[223..], &parity[..]);
    }

    /// libfec's own `decode_rs_ccsds` returns 16 on this block and recovers the codeword.
    #[test]
    fn phil_karns_corrupted_codeword_decodes_back_to_his() {
        let code = codec(32, 1);
        let message = from_hex(KARN_MESSAGE);
        let parity = from_hex(KARN_PARITY);
        let mut block = from_hex(KARN_CORRUPTED);
        assert_eq!(block.len(), 255);

        assert_eq!(code.decode(&mut block), Ok(16));
        assert_eq!(&block[..223], &message[..]);
        assert_eq!(&block[223..], &parity[..]);
    }

    #[test]
    fn the_parity_of_an_all_zero_message_is_all_zeros() {
        for parity_symbols in [16, 32] {
            for interleave in INTERLEAVE_DEPTHS {
                let code = codec(parity_symbols, interleave);
                let mut block = Vec::new();
                code.encode(&vec![0u8; code.data_length()], &mut block)
                    .expect("the right length");
                assert!(
                    block.iter().all(|symbol| *symbol == 0),
                    "E = {}, I = {interleave}",
                    parity_symbols / 2
                );
            }
        }
    }

    #[test]
    fn an_untouched_block_is_reported_as_needing_no_corrections() {
        for parity_symbols in [16, 32] {
            for interleave in INTERLEAVE_DEPTHS {
                let code = codec(parity_symbols, interleave);
                let (data, mut block) = encoded(&code, 7);
                assert_eq!(code.decode(&mut block), Ok(0));
                assert_eq!(&block[..code.data_length()], &data[..]);
            }
        }
    }

    /// Every position, because the Chien search maps a root to a position through the inverse
    /// of the root spacing and a decoder that uses the wrong step corrects position `11 * i`
    /// instead of `i`. On a single error at position 0 the two agree.
    #[test]
    fn every_single_symbol_position_in_a_block_is_correctable() {
        let code = codec(32, 1);
        let (_, clean) = encoded(&code, 11);
        for position in 0..code.block_length() {
            let mut block = clean.clone();
            block[position] ^= 0xA5;
            assert_eq!(code.decode(&mut block), Ok(1), "at position {position}");
            assert_eq!(block, clean, "at position {position}");
        }
    }

    #[test]
    fn corrupting_exactly_the_capacity_gives_the_message_back() {
        for parity_symbols in [16, 32] {
            for interleave in [1, 3, 8] {
                let code = codec(parity_symbols, interleave);
                let (data, clean) = encoded(&code, 1234 + parity_symbols as u32);
                let mut block = clean.clone();
                let mut random = Lcg(99);
                // Exactly E errors in every codeword: the capacity, and not one less.
                let capacity = code.correction_capacity();
                for lane in 0..interleave {
                    for error in 0..capacity {
                        // Spread them over the codeword, parity symbols included. The stride
                        // is 255/E, so the last position is (E-1) * (255/E) < 255 and no two
                        // errors land on one symbol — which is what makes the count below
                        // exactly `E * I` rather than "at most".
                        let symbol = error * (CODEWORD_SYMBOLS / capacity);
                        block[symbol * interleave + lane] ^= random.next_mask();
                    }
                }
                assert_eq!(
                    code.decode(&mut block),
                    Ok(capacity * interleave),
                    "E = {capacity}, I = {interleave}"
                );
                assert_eq!(&block[..code.data_length()], &data[..]);
                assert_eq!(block, clean);
            }
        }
    }

    /// One symbol past the capacity, over many messages.
    ///
    /// `Uncorrectable` is asserted as *usually*, not always, and that is not a hedge: a
    /// received word that is `E + 1` symbols from the transmitted codeword can be `E` or fewer
    /// from a different one, and then the decoder lands on that codeword, the syndromes of the
    /// result really are zero, and no check inside a Reed-Solomon decoder can tell. What must
    /// hold every time is the weaker statement below — the decoder either refuses, or does not
    /// hand back something it claims is the transmitted frame when it is not. For RS(255, 223)
    /// the fraction of received words that fall inside another sphere is about `1 / 16!`, so
    /// in practice every trial here refuses; the loop asserts three quarters so that the day
    /// it does happen the suite reports it rather than flapping.
    #[test]
    fn one_error_past_the_capacity_is_usually_refused_and_is_never_silently_wrong() {
        let code = codec(32, 1);
        let trials = 64;
        let mut refused = 0;
        for trial in 0..trials {
            let (_, clean) = encoded(&code, 500 + trial);
            let mut block = clean.clone();
            let mut random = Lcg(trial * 31 + 1);
            let mut corrupted = 0;
            let mut position = 0;
            while corrupted <= code.correction_capacity() {
                position =
                    (position + 7 + usize::from(random.next_byte()) % 5) % code.block_length();
                if block[position] == clean[position] {
                    block[position] ^= random.next_mask();
                    corrupted += 1;
                }
            }
            match code.decode(&mut block) {
                Err(RsError::Uncorrectable) => refused += 1,
                Err(other) => panic!("the length was right: {other:?}"),
                // No trial in this loop has ever entered this arm — random weight-`E + 1`
                // patterns land inside another sphere far too rarely for 64 of them to find
                // one, and `a_constructed_near_miss_...` below is the deterministic case. The
                // arm stays because the day a trial does enter it, what has to hold is that
                // the block handed back is a codeword, not merely that it differs from the
                // transmitted one, which `Ok` already guarantees.
                Ok(_) => assert_is_a_codeword(&code, &block),
            }
        }
        assert!(
            refused * 4 >= trials * 3,
            "only {refused} of {trials} blocks past the capacity were refused"
        );
    }

    /// The entire reason interleaving exists, in one test.
    ///
    /// Symbol `i` of the block belongs to codeword `i mod I`, so `E * I` consecutive symbols
    /// hit every residue class exactly `E` times: every codeword gets exactly its capacity and
    /// the burst comes out. `(E + 1) * I` consecutive symbols hit every class `E + 1` times
    /// and every codeword is over. Both hold wherever the burst starts, which is why the step
    /// past the capacity is `+ I` and not `+ 1`.
    #[test]
    fn a_burst_of_the_capacity_times_the_depth_is_corrected_and_one_depth_more_is_not() {
        let code = codec(32, 5);
        let capacity = code.correction_capacity();
        let (data, clean) = encoded(&code, 2024);

        // Inside the data, and straddling the boundary into the parity.
        for start in [100, code.data_length() - 40] {
            let mut block = clean.clone();
            let mut random = Lcg(17);
            for offset in 0..capacity * code.interleave() {
                block[start + offset] ^= random.next_mask();
            }
            assert_eq!(
                code.decode(&mut block),
                Ok(capacity * code.interleave()),
                "a burst of {} from {start}",
                capacity * code.interleave()
            );
            assert_eq!(&block[..code.data_length()], &data[..]);

            let mut block = clean.clone();
            let mut random = Lcg(17);
            for offset in 0..(capacity + 1) * code.interleave() {
                block[start + offset] ^= random.next_mask();
            }
            // Usually `Uncorrectable`, for the reason the previous test spells out.
            match code.decode(&mut block) {
                Err(RsError::Uncorrectable) => {}
                Err(other) => panic!("the length was right: {other:?}"),
                Ok(_) => assert_is_a_codeword(&code, &block),
            }
        }
    }

    /// The mis-correction the two loops above only assert is *not silently wrong*, built on
    /// purpose so that it happens every run.
    ///
    /// Two codewords whose messages differ in one symbol differ in exactly `2E + 1` places:
    /// at least that, because `2E + 1` is the minimum distance of an MDS code, and at most
    /// that, because only the one message symbol and the `2E` check symbols can move. Move a
    /// received word `E + 1` of those steps and it sits `E` from the *other* codeword, so a
    /// decoder that is working correctly repairs it into that one, reports `Ok(E)`, and hands
    /// on a frame that was never transmitted. Nothing inside a Reed-Solomon decoder can see
    /// this — the result really is a codeword, which is what [`assert_is_a_codeword`] checks
    /// and all a success can ever claim. It is the module documentation's reason for counting
    /// corrections instead of trusting them.
    #[test]
    fn a_constructed_near_miss_is_corrected_into_the_wrong_codeword_and_called_success() {
        let code = codec(32, 1);
        let capacity = code.correction_capacity();
        let (data, clean) = encoded(&code, 7);

        // A second message one symbol away, and the codeword it encodes to.
        let mut other_data = data.clone();
        other_data[100] ^= 0x5A;
        let mut other = Vec::new();
        code.encode(&other_data, &mut other)
            .expect("the same length");
        let differences: Vec<usize> = (0..code.block_length())
            .filter(|position| clean[*position] != other[*position])
            .collect();
        assert_eq!(
            differences.len(),
            2 * capacity + 1,
            "one message symbol and every check symbol, which is the minimum distance"
        );

        // `E + 1` steps from `clean` is `E` steps from `other`.
        let mut block = clean.clone();
        for position in &differences[..=capacity] {
            block[*position] = other[*position];
        }

        assert_eq!(code.decode(&mut block), Ok(capacity));
        assert_is_a_codeword(&code, &block);
        assert_eq!(block, other, "the decoder landed on the near codeword");
        assert_ne!(
            &block[..code.data_length()],
            &data[..],
            "and the frame it handed on is not the one that was sent"
        );
    }

    #[test]
    fn only_the_pairs_ccsds_defines_are_accepted() {
        for parity_symbols in [16, 32] {
            for interleave in INTERLEAVE_DEPTHS {
                assert!(ReedSolomon::ccsds(parity_symbols, interleave).is_some());
            }
            // Six and seven are not CCSDS depths, whatever the arithmetic would do with them.
            for interleave in [0, 6, 7, 9, usize::MAX] {
                assert!(ReedSolomon::ccsds(parity_symbols, interleave).is_none());
            }
        }
        for parity_symbols in [0, 1, 8, 17, 24, 31, 33, 64, usize::MAX] {
            assert!(ReedSolomon::ccsds(parity_symbols, 1).is_none());
        }
    }

    #[test]
    fn a_block_of_the_wrong_length_is_refused_and_says_both_numbers() {
        let code = codec(32, 4);
        let expected = code.block_length();
        for found in [0, 1, expected - 1, expected + 1, expected / 2, 65_535] {
            let mut block = vec![0u8; found];
            assert_eq!(
                code.decode(&mut block),
                Err(RsError::BadLength { expected, found }),
                "a {found}-byte block"
            );
        }
    }

    /// A short frame silently padded to a codeword decodes cleanly to bytes nobody sent, so
    /// the encoder refuses rather than pads.
    #[test]
    fn a_message_of_the_wrong_length_is_refused() {
        let code = codec(16, 2);
        let expected = code.data_length();
        for found in [0, expected - 1, expected + 1] {
            let mut out = Vec::new();
            assert_eq!(
                code.encode(&vec![0u8; found], &mut out),
                Err(RsError::BadLength { expected, found })
            );
            assert!(out.is_empty(), "nothing is appended to a refused encode");
        }
    }

    #[test]
    fn the_lengths_follow_from_the_parameters() {
        let rs223 = codec(32, 1);
        assert_eq!(rs223.correction_capacity(), 16);
        assert_eq!(rs223.block_length(), 255);
        assert_eq!(rs223.data_length(), 223);

        let rs239 = codec(16, 1);
        assert_eq!(rs239.correction_capacity(), 8);
        assert_eq!(rs239.block_length(), 255);
        assert_eq!(rs239.data_length(), 239);

        let deep = codec(32, 5);
        assert_eq!(deep.block_length(), 1275);
        assert_eq!(deep.data_length(), 1115);
        assert_eq!(deep.parity_symbols(), 32);
        assert_eq!(deep.interleave(), 5);
    }

    /// One error in the last codeword must not leave the first `I - 1` repaired: the caller
    /// takes the first `data_length` bytes as a frame and would parse a half-corrected one.
    #[test]
    fn an_uncorrectable_codeword_leaves_the_whole_block_as_it_was_found() {
        let code = codec(16, 3);
        let (_, clean) = encoded(&code, 4242);
        let mask = Lcg(5).next_mask();

        // Codeword 0, symbol 0: one error, and `decode` reaches codeword 0 first. Asserted on
        // its own so that the refusal below is known to happen *after* a correction was
        // computed, rather than the test passing because nothing was repairable.
        let mut block = clean.clone();
        block[0] ^= mask;
        assert_eq!(code.decode(&mut block), Ok(1));
        assert_eq!(block, clean);

        // The same error, plus one more than codeword 2 can carry.
        let mut block = clean.clone();
        block[0] ^= mask;
        for error in 0..=code.correction_capacity() {
            // Position 27e + 2 is symbol 9e of codeword 2.
            block[error * 9 * 3 + 2] ^= mask;
        }
        let damaged = block.clone();
        assert_eq!(code.decode(&mut block), Err(RsError::Uncorrectable));
        assert_eq!(block, damaged, "a refused block is not partly repaired");
    }

    #[test]
    fn encoding_and_decoding_round_trips_at_every_interleaving_depth() {
        for parity_symbols in [16, 32] {
            for interleave in INTERLEAVE_DEPTHS {
                let code = codec(parity_symbols, interleave);
                let (data, clean) = encoded(&code, 808 + interleave as u32);
                let mut block = clean.clone();
                let mut random = Lcg(interleave as u32 + 3);
                // One error per codeword, at a different depth in each.
                for lane in 0..interleave {
                    let symbol = (lane * 37 + 5) % CODEWORD_SYMBOLS;
                    block[symbol * interleave + lane] ^= random.next_mask();
                }
                assert_eq!(code.decode(&mut block), Ok(interleave));
                assert_eq!(&block[..code.data_length()], &data[..]);
            }
        }
    }
}
