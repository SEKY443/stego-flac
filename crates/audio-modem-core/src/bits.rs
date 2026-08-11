//! Byte <-> symbol packing.
//!
//! Phase 1 constrains `bits_per_symbol` to a divisor of 8, so the mapping is a
//! total bijection between `n` bytes and `n · symbols_per_byte` symbols with no
//! padding and no state carried across byte boundaries. Symbols are emitted
//! most-significant-field first, so the on-air symbol order matches the way the
//! byte is written down in hex.
//!
//! For the default 4 bit/symbol plan this is literally nibble order: `0xA7`
//! becomes `[0xA, 0x7]`.
//!
//! Once framing exists and carries an explicit payload length, this module
//! grows a general MSB-first bitstream packer and the divisor restriction in
//! [`ModemConfig::validate`](crate::ModemConfig::validate) can be dropped.

/// Split `payload` into symbols of `bits_per_symbol` bits each, MSB-first.
///
/// `bits_per_symbol` must divide 8; callers get that guarantee from
/// [`ModemConfig::validate`](crate::ModemConfig::validate).
pub fn bytes_to_symbols(payload: &[u8], bits_per_symbol: u32) -> Vec<u8> {
    debug_assert!(bits_per_symbol > 0 && 8 % bits_per_symbol == 0);

    let symbols_per_byte = (8 / bits_per_symbol) as usize;
    let mask = mask_for(bits_per_symbol);
    let mut symbols = Vec::with_capacity(payload.len() * symbols_per_byte);

    for &byte in payload {
        for field in (0..symbols_per_byte).rev() {
            let shift = field as u32 * bits_per_symbol;
            symbols.push((byte >> shift) & mask);
        }
    }

    symbols
}

/// Reassemble bytes from MSB-first `symbols`.
///
/// Returns `None` if `symbols.len()` is not a whole number of bytes; the caller
/// turns that into a typed [`DemodError`](crate::DemodError) since it has the
/// context to describe it.
pub fn symbols_to_bytes(symbols: &[u8], bits_per_symbol: u32) -> Option<Vec<u8>> {
    debug_assert!(bits_per_symbol > 0 && 8 % bits_per_symbol == 0);

    let symbols_per_byte = (8 / bits_per_symbol) as usize;
    if !symbols.len().is_multiple_of(symbols_per_byte) {
        return None;
    }

    let mask = mask_for(bits_per_symbol);
    let bytes = symbols
        .chunks_exact(symbols_per_byte)
        .map(|group| {
            group.iter().fold(0u8, |acc, &symbol| {
                (acc << bits_per_symbol) | (symbol & mask)
            })
        })
        .collect();

    Some(bytes)
}

#[inline]
fn mask_for(bits_per_symbol: u32) -> u8 {
    // `bits_per_symbol` is at most 8 here, so the shift is done in u16 to keep
    // the `1 << 8` case from overflowing.
    ((1u16 << bits_per_symbol) - 1) as u8
}

/// MSB-first bit packer, for symbol sizes that do not divide 8.
///
/// OFDM carries `active_bins × bits_per_bin` bits per symbol, which is only a
/// whole number of bytes for particular combinations. The frame header declares
/// every length explicitly, so trailing padding bits are unambiguous and this
/// can pad freely.
pub struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    bits: u32,
}

impl BitWriter {
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            out: Vec::with_capacity(bytes),
            acc: 0,
            bits: 0,
        }
    }

    /// Append the low `count` bits of `value`, most significant first.
    pub fn write(&mut self, value: u32, count: u32) {
        debug_assert!(count <= 32);
        self.acc = (self.acc << count) | u64::from(value & mask32(count));
        self.bits += count;
        while self.bits >= 8 {
            self.bits -= 8;
            self.out.push((self.acc >> self.bits) as u8);
        }
    }

    /// Flush, zero-padding the final byte.
    pub fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            let shift = 8 - self.bits;
            self.out.push(((self.acc << shift) & 0xff) as u8);
        }
        self.out
    }
}

/// MSB-first bit reader. Reads past the end yield zero bits.
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Start reading at an absolute bit offset.
    ///
    /// Lets each OFDM symbol be built independently: symbol `i` begins at
    /// `i * bits_per_symbol`, so the modulator can fan out across symbols
    /// instead of walking one shared cursor.
    pub fn new_at(data: &'a [u8], bit_offset: usize) -> Self {
        Self {
            data,
            pos: bit_offset,
        }
    }

    /// Total bits available.
    pub fn total_bits(&self) -> usize {
        self.data.len() * 8
    }

    /// Read `count` bits, most significant first.
    pub fn read(&mut self, count: u32) -> u32 {
        let mut value = 0u32;
        for _ in 0..count {
            let byte = self.pos >> 3;
            let bit = if byte < self.data.len() {
                (self.data[byte] >> (7 - (self.pos & 7))) & 1
            } else {
                0
            };
            value = (value << 1) | u32::from(bit);
            self.pos += 1;
        }
        value
    }
}

#[inline]
fn mask32(count: u32) -> u32 {
    if count >= 32 {
        u32::MAX
    } else {
        (1u32 << count) - 1
    }
}
