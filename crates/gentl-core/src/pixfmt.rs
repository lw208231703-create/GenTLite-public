//! PFNC pixel format constants and payload size math.
//!
//! The full PFNC enumeration lives in `pixfmt_table` (generated from
//! `include/PFNC.h` by `scripts/generate_pixfmt.py` — explicit table, no
//! derivation heuristic, per project decision 2026-10-14). The named
//! constants below are the commonly used subset; size math goes through
//! the table for every known format.
//!
//! GVSP carries unpacked 10/12-bit pixels in 16-bit containers (2 bytes on
//! the wire); the packed variants (`Mono10p`/`Mono12p`) bit-pack the pixels
//! (1.25 / 1.5 bytes per pixel).

/// Monochrome 8-bit (PFNC.h: `PFNC_Mono8`).
pub const PFNC_MONO8: u32 = 0x0108_0001;
/// Monochrome 10-bit unpacked (PFNC.h: `PFNC_Mono10`).
pub const PFNC_MONO10: u32 = 0x0110_0003;
/// Monochrome 12-bit unpacked (PFNC.h: `PFNC_Mono12`).
pub const PFNC_MONO12: u32 = 0x0110_0005;
/// Monochrome 16-bit (PFNC.h: `PFNC_Mono16`).
pub const PFNC_MONO16: u32 = 0x0110_0007;
/// Monochrome 10-bit packed (PFNC.h: `PFNC_Mono10p`).
pub const PFNC_MONO10P: u32 = 0x010A_0046;
/// Monochrome 12-bit packed (PFNC.h: `PFNC_Mono12p`).
pub const PFNC_MONO12P: u32 = 0x010C_0047;

/// Bayer Green-Red 8-bit (PFNC.h: `PFNC_BayerGR8`).
pub const PFNC_BAYER_GR8: u32 = 0x0108_0008;
/// Bayer Red-Green 8-bit (PFNC.h: `PFNC_BayerRG8`).
pub const PFNC_BAYER_RG8: u32 = 0x0108_0009;
/// Bayer Green-Blue 8-bit (PFNC.h: `PFNC_BayerGB8`).
pub const PFNC_BAYER_GB8: u32 = 0x0108_000A;
/// Bayer Blue-Green 8-bit (PFNC.h: `PFNC_BayerBG8`).
pub const PFNC_BAYER_BG8: u32 = 0x0108_000B;
/// Red-Green-Blue 8-bit (PFNC.h: `PFNC_RGB8`).
pub const PFNC_RGB8: u32 = 0x0218_0014;
/// Blue-Green-Red 8-bit (PFNC.h: `PFNC_BGR8`).
pub const PFNC_BGR8: u32 = 0x0218_0015;
/// YUV 4:2:2 8-bit (PFNC.h: `PFNC_YUV422_8`).
pub const PFNC_YUV422_8: u32 = 0x0210_0032;

/// All PFNC formats with a computable single-buffer payload size: the
/// entire generated table. (Planar entries size as one total payload here;
/// their GenTL *presentation* needs parts, v1.5, not implemented.)
pub fn is_supported(pfnc: u32) -> bool {
    crate::pixfmt_table::FORMATS.iter().any(|(_, v, _, _)| *v == pfnc)
}

/// Bytes per pixel on the wire. Packed formats return fractional values
/// (`Mono10p` = 1.25, `Mono12p` = 1.5). `None` if the format is not
/// supported by this producer.
pub fn bytes_per_pixel(pfnc: u32) -> Option<f32> {
    let (_, _, bits, _) = crate::pixfmt_table::FORMATS.iter().find(|(_, v, _, _)| *v == pfnc)?;
    Some(f32::from(*bits) / 8.0)
}

/// One image row in bytes (no padding), integer-exact for any width.
fn stride_bytes(pfnc: u32, width: u32) -> Option<usize> {
    let (_, _, bits, _) =
        crate::pixfmt_table::FORMATS.iter().find(|(_, v, _, _)| *v == pfnc)?;
    let w = u64::from(width);
    // Pixels are bit-packed within a row; each row starts on a byte
    // boundary (SFNC), so per-row size is ceil(w * bits / 8).
    let stride = w.checked_mul(u64::from(*bits))?.checked_add(7)? / 8;
    usize::try_from(stride).ok()
}

/// Unpadded image payload size in bytes: `stride * height`.
pub fn payload_size(pfnc: u32, width: u32, height: u32) -> Option<usize> {
    let stride = stride_bytes(pfnc, width)? as u64;
    let total = stride.checked_mul(u64::from(height))?;
    usize::try_from(total).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_match_pfnc_header() {
        // Cross-checked against include/PFNC.h on 2026-10-09.
        assert_eq!(PFNC_MONO8, 0x0108_0001);
        assert_eq!(PFNC_MONO10, 0x0110_0003);
        assert_eq!(PFNC_MONO12, 0x0110_0005);
        assert_eq!(PFNC_MONO16, 0x0110_0007);
        assert_eq!(PFNC_MONO10P, 0x010A_0046);
        assert_eq!(PFNC_MONO12P, 0x010C_0047);
        // Bayer/RGB/YUV group, verified against include/PFNC.h on 2026-10-10.
        assert_eq!(PFNC_BAYER_GR8, 0x0108_0008);
        assert_eq!(PFNC_BAYER_RG8, 0x0108_0009);
        assert_eq!(PFNC_BAYER_GB8, 0x0108_000A);
        assert_eq!(PFNC_BAYER_BG8, 0x0108_000B);
        assert_eq!(PFNC_RGB8, 0x0218_0014);
        assert_eq!(PFNC_BGR8, 0x0218_0015);
        assert_eq!(PFNC_YUV422_8, 0x0210_0032);
        assert!(is_supported(PFNC_MONO8) && is_supported(PFNC_YUV422_8));
    }

    #[test]
    fn bytes_per_pixel_values() {
        assert_eq!(bytes_per_pixel(PFNC_MONO8), Some(1.0));
        assert_eq!(bytes_per_pixel(PFNC_MONO10), Some(2.0));
        assert_eq!(bytes_per_pixel(PFNC_MONO12), Some(2.0));
        assert_eq!(bytes_per_pixel(PFNC_MONO16), Some(2.0));
        assert_eq!(bytes_per_pixel(PFNC_MONO10P), Some(1.25));
        assert_eq!(bytes_per_pixel(PFNC_MONO12P), Some(1.5));
        assert_eq!(bytes_per_pixel(PFNC_BAYER_GR8), Some(1.0));
        assert_eq!(bytes_per_pixel(PFNC_BAYER_RG8), Some(1.0));
        assert_eq!(bytes_per_pixel(PFNC_BAYER_GB8), Some(1.0));
        assert_eq!(bytes_per_pixel(PFNC_BAYER_BG8), Some(1.0));
        assert_eq!(bytes_per_pixel(PFNC_RGB8), Some(3.0));
        assert_eq!(bytes_per_pixel(PFNC_BGR8), Some(3.0));
        assert_eq!(bytes_per_pixel(PFNC_YUV422_8), Some(2.0));
        // YUV422_8_UYVY (0x0210_001F): now in the full PFNC table.
        assert_eq!(bytes_per_pixel(0x0210_001F), Some(2.0));
        assert!(!is_supported(0));
        assert!(!is_supported(0x8000_0000)); // PFNC_CUSTOM sentinel
        assert!(is_supported(PFNC_MONO12P));
        assert!(is_supported(PFNC_BAYER_GR8));
        assert!(is_supported(PFNC_RGB8));
        assert!(is_supported(PFNC_YUV422_8));
    }

    #[test]
    fn payload_size_rounding() {
        // Unpacked: plain multiples.
        assert_eq!(payload_size(PFNC_MONO8, 640, 480), Some(640 * 480));
        assert_eq!(payload_size(PFNC_MONO16, 100, 10), Some(2000));
        // Bayer8: same stride as Mono8.
        assert_eq!(payload_size(PFNC_BAYER_GR8, 640, 480), Some(640 * 480));
        // RGB8/BGR8: 3 bytes per pixel.
        assert_eq!(payload_size(PFNC_RGB8, 100, 10), Some(3000));
        assert_eq!(payload_size(PFNC_BGR8, 3, 2), Some(18));
        // YUV422_8: 2 bytes per pixel.
        assert_eq!(payload_size(PFNC_YUV422_8, 100, 10), Some(2000));
        // Packed Mono10p: ceil(width * 10 / 8) per row.
        assert_eq!(payload_size(PFNC_MONO10P, 4, 1), Some(5)); // 40 bits = 5 B
        assert_eq!(payload_size(PFNC_MONO10P, 1, 1), Some(2)); // 10 bits -> 2 B
        assert_eq!(payload_size(PFNC_MONO10P, 7, 3), Some(9 * 3)); // 70 bits -> 9 B/row
        // Packed Mono12p: ceil(width * 12 / 8) per row.
        assert_eq!(payload_size(PFNC_MONO12P, 2, 1), Some(3)); // 24 bits = 3 B
        assert_eq!(payload_size(PFNC_MONO12P, 3, 2), Some(5 * 2)); // 36 bits -> 5 B/row
        assert_eq!(payload_size(0xDEAD_BEEF, 3, 2), None);
        // 2 bytes/px * u32::MAX width overflows u64 when multiplied by the height.
        assert_eq!(payload_size(PFNC_MONO16, u32::MAX, u32::MAX), None);
    }
}

#[cfg(test)]
mod table_tests {
    use super::*;
    use crate::pixfmt_table::FORMATS;

    #[test]
    fn table_matches_pfnc_header_semantics() {
        // Every entry's size field must match the PFNC value's own
        // bits 16-23 (self-consistency of the generator extraction).
        for (name, value, bits, _) in FORMATS {
            assert_eq!(
                u8::try_from((value >> 16) & 0xFF).unwrap(),
                *bits,
                "{name} size field mismatch"
            );
        }
    }

    #[test]
    fn table_covers_full_pfnc_range() {
        assert!(FORMATS.len() >= 250, "PFNC 2.4 defines ~250 formats, got {}", FORMATS.len());
    }

    #[test]
    fn spot_check_sizes_across_families() {
        // Mono packed
        assert_eq!(payload_size(PFNC_MONO10P, 4, 1), Some(5));
        // RGB/BGR
        assert_eq!(payload_size(0x0218_0015, 3, 2), Some(18)); // BGR8
        // 10-bit RGB packed (RGB10p = 30 bits/px)
        assert_eq!(payload_size(0x021E_005C, 4, 1), Some(15)); // RGB10p, ceil(4*30/8)
        // Coord3D ABC 32f (96 bits/px total across 3 planes)
        assert_eq!(payload_size(0x0260_00C0, 10, 2), Some(240));
        // YCbCr420 semiplanar (12 bits/px total)
        assert_eq!(payload_size(0x020C_0112, 4, 2), Some(12)); // 2 rows * ceil(4*12/8)
    }

    #[test]
    fn planar_formats_flagged() {
        let planar: Vec<_> = FORMATS.iter().filter(|(_, _, _, p)| *p).collect();
        assert!(planar.len() >= 15, "expected planar entries (Coord3D_*/YCbCr*_Semiplanar)");
        assert!(planar.iter().all(|(n, _, _, _)| n.contains("Planar") || n.contains("Semiplanar")));
    }
}
