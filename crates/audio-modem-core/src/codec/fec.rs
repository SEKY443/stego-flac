//! RaptorQ (RFC 6330) forward error correction.
//!
//! # What this actually buys on a lossless channel
//!
//! Nothing, when the FLAC file arrives intact — and that is worth stating
//! plainly rather than hiding behind the acronym. A fountain code earns its
//! keep on an *erasure* channel, where symbols go missing and the receiver
//! cannot ask for them again. A bit-exact file has no erasures.
//!
//! It is still here, and still on by default at a low rate, because the
//! erasure it protects against is real for this format: a **truncated file**.
//! Audio gets clipped by a failed upload, an interrupted download, a chat app's
//! duration cap, or a user trimming the tail in an editor. Without FEC, losing
//! the last 1% of the carrier loses the whole payload. With 5% repair overhead,
//! RaptorQ reconstructs the object from *any* sufficiently large subset of
//! packets, so a clipped tail is survivable up to the overhead.
//!
//! That is the honest case for it. Set `repair_overhead_percent` to 0 for pure
//! archival transport where the file is known to survive intact, and the layer
//! reduces to a ~1.6% framing cost.
//!
//! # Why RaptorQ rather than Reed–Solomon
//!
//! Reed–Solomon is optimal (MDS: any `k` of `n` symbols reconstruct) but its
//! decoding cost is quadratic and its block length is capped by the field size
//! — 255 symbols in GF(2^8), which forces awkward interleaving across a
//! multi-megabyte object. RaptorQ decodes in linear time, handles objects up to
//! 2^40 bytes in one logical block, and needs only `k + ε` symbols with ε
//! typically 0 and overwhelmingly ≤ 2. It gives up strict optimality for a
//! failure probability around 10^-6 at two extra symbols, which is far below
//! the probability of anything else in this pipeline going wrong.

use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation};

use crate::error::FecError;

/// Serialised length of RaptorQ's Object Transmission Information.
pub const OTI_LEN: usize = 12;
/// Per-packet RaptorQ `PayloadId` overhead (source block number + symbol id).
pub const PAYLOAD_ID_LEN: usize = 4;

/// Default symbol size in bytes.
///
/// 256 balances two opposing costs. Larger symbols amortise the 4-byte
/// `PayloadId` better (1.6% here) but coarsen the granularity of protection —
/// with 1024-byte symbols a small payload becomes a handful of symbols, and
/// losing one loses a large fraction. Smaller symbols protect finely but the
/// framing overhead grows: at 64 bytes it would be 6.3%.
pub const DEFAULT_SYMBOL_SIZE: u16 = 256;

/// Default repair overhead as a percentage of source symbols.
pub const DEFAULT_REPAIR_PERCENT: u8 = 5;

/// FEC configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecParams {
    /// RaptorQ symbol size in bytes.
    pub symbol_size: u16,
    /// Repair symbols to generate, as a percentage of source symbols per block.
    /// Zero disables repair and emits source symbols only.
    pub repair_overhead_percent: u8,
}

impl Default for FecParams {
    fn default() -> Self {
        Self {
            symbol_size: DEFAULT_SYMBOL_SIZE,
            repair_overhead_percent: DEFAULT_REPAIR_PERCENT,
        }
    }
}

/// The result of FEC encoding.
pub struct FecEncoded {
    /// Serialised Object Transmission Information; the decoder cannot start
    /// without it, so it lives in the frame header.
    pub oti: [u8; OTI_LEN],
    /// Concatenated packets, each exactly `PAYLOAD_ID_LEN + symbol_size` bytes.
    pub packets: Vec<u8>,
    /// Number of packets in `packets`.
    pub packet_count: usize,
}

/// Wire size of one packet for a given symbol size.
#[inline]
pub fn packet_size(symbol_size: u16) -> usize {
    PAYLOAD_ID_LEN + symbol_size as usize
}

/// Encode `data` into a flat run of fixed-size RaptorQ packets.
///
/// Packets are concatenated with no length prefixes: RaptorQ pads every symbol,
/// including the last one of a block, to exactly `symbol_size`, so the packet
/// stride is constant and the decoder can split the region arithmetically. That
/// is what makes a truncated tail recoverable — a self-delimiting encoding
/// would lose framing at the cut point, whereas fixed stride means every intact
/// prefix is still a whole number of valid packets.
pub fn encode(data: &[u8], params: FecParams) -> Result<FecEncoded, FecError> {
    if params.symbol_size == 0 {
        return Err(FecError::ZeroSymbolSize);
    }

    let encoder = Encoder::with_defaults(data, params.symbol_size);
    let config = encoder.get_config();

    // Repair symbols are requested per source block, so the percentage is
    // applied to the per-block symbol count rather than the object total.
    let source_blocks = config.source_blocks().max(1) as usize;
    let total_symbols = data.len().div_ceil(params.symbol_size as usize).max(1);
    let symbols_per_block = total_symbols.div_ceil(source_blocks);
    let repair_per_block = if params.repair_overhead_percent == 0 {
        0
    } else {
        (symbols_per_block * params.repair_overhead_percent as usize)
            .div_ceil(100)
            .max(1) as u32
    };

    let encoded = encoder.get_encoded_packets(repair_per_block);
    let stride = packet_size(params.symbol_size);

    let mut packets = Vec::with_capacity(encoded.len() * stride);
    for packet in &encoded {
        packets.extend_from_slice(&packet.serialize());
    }

    Ok(FecEncoded {
        oti: config.serialize(),
        packets,
        packet_count: encoded.len(),
    })
}

/// Reconstruct the object from whatever packets survived.
///
/// `expected_len` is the header's record of the pre-FEC length; RaptorQ also
/// carries a transfer length in the OTI, and both are checked. Extra packets
/// beyond what is needed are simply redundant — the decoder returns as soon as
/// it has enough, which is why a truncated `packets` region can still succeed.
pub fn decode(oti: &[u8; OTI_LEN], packets: &[u8], expected_len: u64) -> Result<Vec<u8>, FecError> {
    let config = ObjectTransmissionInformation::deserialize(oti);
    let symbol_size = config.symbol_size();
    if symbol_size == 0 {
        return Err(FecError::ZeroSymbolSize);
    }

    let stride = packet_size(symbol_size);
    let remainder = packets.len() % stride;
    if remainder != 0 {
        return Err(FecError::RaggedPacketRegion {
            len: packets.len(),
            packet_size: stride,
            remainder,
        });
    }

    let packet_count = packets.len() / stride;
    let mut decoder = Decoder::new(config);
    let mut recovered = None;

    for chunk in packets.chunks_exact(stride) {
        if let Some(data) = decoder.decode(EncodingPacket::deserialize(chunk)) {
            recovered = Some(data);
            break;
        }
    }

    let data = recovered.ok_or(FecError::Unrecoverable {
        packets: packet_count,
    })?;

    if data.len() as u64 != expected_len {
        return Err(FecError::LengthMismatch {
            expected: expected_len,
            got: data.len(),
        });
    }

    Ok(data)
}
