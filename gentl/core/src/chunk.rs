//! GenTL chunk-data parsing shared by the transports (GenTL 1.6 §6.3.5.13,
//! `SINGLE_CHUNK_DATA`).
//!
//! A chunk is `data[K]` followed by a trailing tag carrying the chunk ID and
//! the data length. The tag byte order is transport dependent:
//!
//! - GigE Vision 3.0 §24.4 / CR-276s: chunk tags are big-endian.
//! - USB3 Vision 1.2 §5.5.6 / CR-91st: chunk tags are little-endian.
//!
//! Both specs require decoding from the END of the chunk block: the length
//! field of the last chunk is the only way to find where it starts. The
//! block carried into [`parse_chunks`] therefore must not contain the image
//! chunk (`payload[0..image_len]` in the GenTL buffer is the image itself;
//! its tag is the first 8 bytes of the chunk area and is skipped by the
//! caller before parsing the metadata chunks).
//!
//! GenTL reports chunks with offsets relative to the buffer base, so the
//! caller adds its own base offset to [`ChunkSpan::offset`].

/// One chunk as reported to the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSpan {
    /// Chunk identifier from the tag (`SINGLE_CHUNK_DATA.ChunkID`, widened
    /// from the 32-bit on-wire field).
    pub id: u64,
    /// Data offset relative to the parsed chunk block (the caller adds the
    /// buffer offset).
    pub offset: usize,
    /// Chunk data length in bytes.
    pub len: usize,
}

/// The chunk block does not decode: a tag ran past the block start, or a
/// length is not a multiple of 4 (GigE §24.4 / U3V §5.5.6 require padded,
/// 4-byte-multiple chunk data so the tags stay 32-bit aligned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkError;

/// Parse every chunk of `block` (the chunk area without the image chunk).
///
/// `big_endian` selects the tag byte order (GigE = true, U3V = false).
/// Returns the chunks in buffer order; an empty block is a well-formed
/// empty list.
pub fn parse_chunks(block: &[u8], big_endian: bool) -> Result<Vec<ChunkSpan>, ChunkError> {
    let read_u32 = |bytes: &[u8]| -> u32 {
        let b: [u8; 4] = bytes.try_into().expect("4-byte field");
        if big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        }
    };

    let mut chunks = Vec::new();
    let mut end = block.len();
    while end > 0 {
        // A chunk is at least the 8-byte tag.
        if end < 8 {
            return Err(ChunkError);
        }
        let id = read_u32(&block[end - 8..end - 4]);
        let len = read_u32(&block[end - 4..end]) as usize;
        // The data length must be 4-byte aligned and stay inside the block.
        if len % 4 != 0 || len > end - 8 {
            return Err(ChunkError);
        }
        let start = end - 8 - len;
        chunks.push(ChunkSpan {
            id: u64::from(id),
            offset: start,
            len,
        });
        end = start;
    }
    chunks.reverse();
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `data + id + len` chunks back to back with the requested tag
    /// byte order.
    fn block(chunks: &[(u32, &[u8])], big_endian: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for &(id, data) in chunks {
            assert_eq!(data.len() % 4, 0);
            out.extend_from_slice(data);
            if big_endian {
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            } else {
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn parses_chunks_in_buffer_order() {
        let second = [0x5Au8; 8];
        let third = [0xA5u8; 4];
        let bytes = block(&[(2, &second), (3, &third)], true);
        let parsed = parse_chunks(&bytes, true).expect("well-formed chunk block");
        assert_eq!(
            parsed,
            vec![
                ChunkSpan { id: 2, offset: 0, len: 8 },
                ChunkSpan { id: 3, offset: 16, len: 4 },
            ]
        );
    }

    #[test]
    fn little_endian_tags_are_supported() {
        let data = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let bytes = block(&[(0x0102_0304, &data)], false);
        let parsed = parse_chunks(&bytes, false).expect("well-formed chunk block");
        assert_eq!(
            parsed,
            vec![ChunkSpan {
                id: 0x0102_0304,
                offset: 0,
                len: 8
            }]
        );
    }

    #[test]
    fn empty_block_has_no_chunks() {
        assert_eq!(parse_chunks(&[], true).unwrap(), Vec::new());
    }

    #[test]
    fn malformed_blocks_are_rejected() {
        // Tag length pointing past the block start.
        let mut bytes = vec![0u8; 8];
        bytes[4..8].copy_from_slice(&8u32.to_be_bytes());
        assert_eq!(parse_chunks(&bytes, true), Err(ChunkError));

        // Unaligned data length.
        let mut bytes = vec![0u8; 12];
        bytes[8..12].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(parse_chunks(&bytes, true), Err(ChunkError));

        // Dangling bytes shorter than a tag.
        assert_eq!(parse_chunks(&[0u8; 4], true), Err(ChunkError));
    }
}
