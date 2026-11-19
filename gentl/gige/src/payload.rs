//! Pre-stream `PayloadSize` resolution for `STREAM_INFO_PAYLOAD_SIZE`.
//!
//! GigE Vision 3.0 §37.5.3 makes `PayloadSize` a mandatory read-only GenICam
//! feature: the maximum payload bytes of one block (IP/UDP/GVSP headers and
//! leader/trailer excluded). A GenTL producer needs that value before
//! `DSStartAcquisition` because consumers use `STREAM_INFO_PAYLOAD_SIZE`
//! (GenTL 1.6 §6.4.4) to size their buffers; the first GVSP leader arrives
//! too late for that check.
//!
//! This producer has no full GenApi engine, so this module follows the
//! feature through the device's own XML: `Integer Name="PayloadSize"` ->
//! `<pValue>` chain -> the backing `IntReg`/`MaskedIntReg` -> one GVCP read.
//! Nothing is estimated from Width/Height and nothing is hardcoded: an XML
//! the resolver cannot follow leaves the value unknown (0) until the first
//! GVSP leader, which is the pre-existing behavior.
//!
//! Supported First URL schemes are the ones the deployment uses:
//! `local:<name>;<addr hex>;<len hex>` (GigE Vision §14.1.1), with
//! transparent zip decompression (camera XML is commonly served as a zip
//! manifest). `http:`/`file:` URLs are not fetched; those devices keep the
//! first-leader fallback.

use std::io::Read as _;

/// Upper bound for a camera XML blob, plain or decompressed. Real device
/// descriptions are well below 1 MiB; the cap keeps a malformed First URL or
/// a zip bomb from allocating unbounded memory in the producer.
const MAX_XML_BYTES: usize = 64 * 1024 * 1024;

/// Address and shape of the register backing the `PayloadSize` feature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadReg {
    pub address: u32,
    pub length: usize,
    /// `Endianess` as declared by the device XML (GigE registers are
    /// normally big-endian; the field is not assumed).
    pub big_endian: bool,
    /// Inclusive bit range of a `MaskedIntReg` (`LSB`/`MSB` or `Bit`).
    pub mask: Option<(u8, u8)>,
}

/// Parse `local:<name>;<addr hex>;<len hex>` (§14.1.1) and read the blob.
/// Returns `None` for any other scheme or a failed read.
pub fn fetch_xml<F>(url: &str, read_mem: F) -> Option<Vec<u8>>
where
    F: Fn(u32, &mut [u8]) -> gentl_core::GentlResult<()>,
{
    let rest = url
        .strip_prefix("local:")
        .or_else(|| url.strip_prefix("Local:"))?;
    let mut parts = rest.split(';');
    let name = parts.next()?.trim();
    let address = parse_hex_u32(parts.next()?)?;
    let length = parse_hex_u32(parts.next()?)?;
    if length == 0 || length as usize > MAX_XML_BYTES {
        return None;
    }
    let mut data = vec![0u8; length as usize];
    read_mem(address, &mut data).ok()?;
    if name.to_ascii_lowercase().ends_with(".zip") || data.starts_with(b"PK\x03\x04") {
        unzip_single_xml(&data)
    } else {
        Some(data)
    }
}

/// Resolve `PayloadSize` from the device XML. `None` means the document has
/// no followable feature (or no `Address` behind it).
pub fn parse_payload_reg(xml: &str) -> Option<PayloadReg> {
    let mut name = "PayloadSize";
    // Bounded chain: a malformed XML must not spin (pValue cycles).
    for _ in 0..8 {
        let inner = find_element(xml, "Integer", name)
            .or_else(|| find_element(xml, "IntReg", name))
            .or_else(|| find_element(xml, "MaskedIntReg", name))?;
        if let Some(address) = child_text(inner, "Address") {
            let address = parse_u32(address)?;
            let length = child_text(inner, "Length")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(4);
            if length == 0 || length > 8 {
                return None;
            }
            let big_endian = !matches!(
                child_text(inner, "Endianess").map(str::trim),
                Some("LittleEndian")
            );
            return Some(PayloadReg {
                address,
                length,
                big_endian,
                mask: parse_mask(inner, length, big_endian),
            });
        }
        name = child_text(inner, "pValue")?.trim();
    }
    None
}

/// Decode one register read into a payload size. Returns `None` for a zero
/// or non-positive value: §37.5.3 requires `PayloadSize` to be positive
/// (CR-517st), and 0 is also this producer's "unknown" sentinel.
pub fn decode_payload_value(reg: &PayloadReg, bytes: &[u8]) -> Option<usize> {
    if bytes.len() < reg.length {
        return None;
    }
    let mut value = 0u64;
    if reg.big_endian {
        for &byte in &bytes[..reg.length] {
            value = (value << 8) | u64::from(byte);
        }
    } else {
        for (i, &byte) in bytes[..reg.length].iter().enumerate() {
            value |= u64::from(byte) << (8 * i);
        }
    }
    if let Some((lsb, msb)) = reg.mask {
        if msb >= 64 || lsb > msb {
            return None;
        }
        let width = u32::from(msb - lsb) + 1;
        let mask = if width == 64 {
            u64::MAX
        } else {
            ((1u64 << width) - 1) << lsb
        };
        value = (value & mask) >> lsb;
    }
    usize::try_from(value).ok().filter(|size| *size > 0)
}

/// Stream defaults exposed by the device XML. The registers are kept as
/// [`PayloadReg`] so the caller can read them through the same GVCP path
/// used for `PayloadSize`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamDefaults {
    pub packet_size: Option<PayloadReg>,
    pub packet_size_min: Option<PayloadReg>,
    pub packet_size_max: Option<PayloadReg>,
}

/// Resolve the packet-size registers from `GevSCPSPacketSize`.
///
/// Typical chain: `Integer GevSCPSPacketSize` -> `IntConverter
/// GevSCPSPacketSizeVal_Reg` -> `IntReg GevSCPSPacketSizeVal_INT`
/// (address `0xd04`), with `pMin`/`pMax` pointing at
/// `GevSCPSMin_Reg`/`GevSCPSMax_Reg`. A missing feature or a feature
/// without an address yields `None`; the caller then keeps the factory
/// SCPSx value.
pub fn parse_stream_defaults(xml: &str) -> StreamDefaults {
    let feature = find_element(xml, "Integer", "GevSCPSPacketSize");
    let resolve = |name: &str| resolve_int_reg(xml, name.trim());
    StreamDefaults {
        packet_size: feature
            .and_then(|f| child_text(f, "pValue"))
            .and_then(resolve),
        packet_size_min: feature
            .and_then(|f| child_text(f, "pMin"))
            .and_then(resolve)
            .or_else(|| resolve_int_reg(xml, "GevSCPSMin_Reg")),
        packet_size_max: feature
            .and_then(|f| child_text(f, "pMax"))
            .and_then(resolve)
            .or_else(|| resolve_int_reg(xml, "GevSCPSMax_Reg")),
    }
}

/// Follow one `pValue` chain to an `IntReg`/`MaskedIntReg`. Supports the
/// converter/knife nodes used by real camera XMLs between the feature and
/// the backing register.
fn resolve_int_reg<'a>(xml: &'a str, mut name: &'a str) -> Option<PayloadReg> {
    for _ in 0..8 {
        let inner = [
            "Integer",
            "IntReg",
            "MaskedIntReg",
            "IntConverter",
            "Converter",
            "SwissKnife",
            "Boolean",
        ]
        .iter()
        .find_map(|tag| find_element(xml, tag, name))?;
        if let Some(address) = child_text(inner, "Address") {
            let address = parse_u32(address)?;
            let length = child_text(inner, "Length")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(4);
            if length == 0 || length > 8 {
                return None;
            }
            let big_endian = !matches!(
                child_text(inner, "Endianess").map(str::trim),
                Some("LittleEndian")
            );
            return Some(PayloadReg {
                address,
                length,
                big_endian,
                mask: parse_mask(inner, length, big_endian),
            });
        }
        name = child_text(inner, "pValue")?.trim();
    }
    None
}

fn parse_mask(inner: &str, length: usize, big_endian: bool) -> Option<(u8, u8)> {
    let bits = length.checked_mul(8)?;
    if bits > 64 {
        return None;
    }
    let bits = bits as u8;
    if let Some(bit) = child_text(inner, "Bit") {
        let bit = bit.trim().parse::<u8>().ok()?;
        if bit >= bits {
            return None;
        }
        // GenICam numbers big-endian register bits MSB-first (bit 0 is the
        // MSB); normalize the mask to ordinary little-endian bit positions.
        let normal = if big_endian { bits - 1 - bit } else { bit };
        return Some((normal, normal));
    }
    let raw_lsb = child_text(inner, "LSB")?.trim().parse::<u8>().ok()?;
    let raw_msb = child_text(inner, "MSB")?.trim().parse::<u8>().ok()?;
    if raw_lsb >= bits || raw_msb >= bits {
        return None;
    }
    if big_endian {
        // The reference implementation requires LSB >= MSB for BigEndian.
        if raw_lsb < raw_msb {
            return None;
        }
        Some((bits - 1 - raw_lsb, bits - 1 - raw_msb))
    } else {
        if raw_msb < raw_lsb {
            return None;
        }
        Some((raw_lsb, raw_msb))
    }
}

/// Find an element by tag and `Name` attribute, returning its inner text.
fn find_element<'a>(xml: &'a str, tag: &str, name: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut search = 0usize;
    while let Some(rel) = xml[search..].find(&open) {
        let start = search + rel;
        let after = &xml[start + open.len()..];
        let boundary = after
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            search = start + open.len();
            continue;
        }
        let tag_end = xml[start..].find('>')? + start + 1;
        let head = &xml[start..tag_end];
        if attr_value(head, "Name") == Some(name) {
            if head.trim_end().ends_with("/>") {
                return Some("");
            }
            let close_rel = xml[tag_end..].find(&close)?;
            return Some(&xml[tag_end..tag_end + close_rel]);
        }
        search = tag_end;
    }
    None
}

/// Text content of a direct child element (`<tag>text</tag>`).
fn child_text<'a>(inner: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let start = inner.find(&open)?;
    let after = &inner[start + open.len()..];
    let boundary = after
        .chars()
        .next()
        .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
    if !boundary {
        return None;
    }
    if after.trim_start().starts_with("/>") {
        return None;
    }
    let tag_end = inner[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let close_rel = inner[tag_end..].find(&close)?;
    Some(&inner[tag_end..tag_end + close_rel])
}

/// Attribute value with a tag-name boundary (`Name` must not match
/// `NameSpace`).
fn attr_value<'a>(head: &'a str, attr: &str) -> Option<&'a str> {
    let mut search = 0usize;
    while let Some(rel) = head[search..].find(attr) {
        let start = search + rel;
        let before_ok = start == 0
            || head[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || c == '<');
        let after = &head[start + attr.len()..];
        let after_trim = after.trim_start();
        if before_ok {
            if let Some(rest) = after_trim.strip_prefix('=') {
                let rest = rest.trim_start();
                let quote = rest.chars().next()?;
                if quote == '"' || quote == '\'' {
                    let end = rest[1..].find(quote)? + 1;
                    return Some(&rest[1..end]);
                }
            }
        }
        search = start + attr.len();
    }
    None
}

fn parse_u32(text: &str) -> Option<u32> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        text.parse::<u32>().ok()
    }
}

fn parse_hex_u32(text: &str) -> Option<u32> {
    let text = text.trim();
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    u32::from_str_radix(text, 16).ok()
}

/// One ZIP manifest entry contains a single GenICam XML file (GenCP
/// §5.5.1.2 Table 34): the first `.xml` member wins.
fn unzip_single_xml(zip_bytes: &[u8]) -> Option<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).ok()?;
    for i in 0..archive.len() {
        let file = archive.by_index(i).ok()?;
        if file.name().to_ascii_lowercase().ends_with(".xml") {
            if file.size() as usize > MAX_XML_BYTES {
                return None;
            }
            let mut raw = Vec::with_capacity(file.size() as usize);
            file.take(MAX_XML_BYTES as u64 + 1)
                .read_to_end(&mut raw)
                .ok()?;
            if raw.len() > MAX_XML_BYTES {
                return None;
            }
            return Some(raw);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_real_camera_dump_shape() {
        // Same shape as the LeadingOE dump: Integer -> plain IntReg, 4-byte
        // big-endian register.
        let xml = r#"
            <Integer Name="PayloadSize" NameSpace="Standard">
              <pValue>PayloadSize_CtrlValueFao</pValue>
            </Integer>
            <IntReg Name="PayloadSize_CtrlValueFao" NameSpace="Custom">
              <Address>0x10000050</Address>
              <Length>4</Length>
              <AccessMode>RO</AccessMode>
              <pPort>Device</pPort>
              <Sign>Unsigned</Sign>
              <Endianess>BigEndian</Endianess>
            </IntReg>
        "#;
        let reg = parse_payload_reg(xml).expect("PayloadSize resolved");
        assert_eq!(
            reg,
            PayloadReg {
                address: 0x1000_0050,
                length: 4,
                big_endian: true,
                mask: None,
            }
        );
        let bytes = 2_621_440u32.to_be_bytes();
        assert_eq!(decode_payload_value(&reg, &bytes), Some(2_621_440));
    }

    #[test]
    fn follows_chained_features_and_little_endian_64() {
        let xml = r#"
            <Integer Name="PayloadSize">
              <pValue>PayloadSizeAlias</pValue>
            </Integer>
            <Integer Name="PayloadSizeAlias">
              <pValue>PayloadSize_Reg</pValue>
            </Integer>
            <IntReg Name="PayloadSize_Reg">
              <Address>0x40008</Address>
              <Length>8</Length>
              <Endianess>LittleEndian</Endianess>
            </IntReg>
        "#;
        let reg = parse_payload_reg(xml).expect("PayloadSize resolved");
        assert_eq!(reg.length, 8);
        assert!(!reg.big_endian);
        assert_eq!(
            decode_payload_value(&reg, &1_310_720u64.to_le_bytes()),
            Some(1_310_720)
        );
    }

    #[test]
    fn masked_register_extracts_the_declared_bits() {
        let xml = r#"
            <Integer Name="PayloadSize"><pValue>PayloadReg</pValue></Integer>
            <MaskedIntReg Name="PayloadReg">
              <Address>0x1234</Address>
              <Length>4</Length>
              <LSB>4</LSB>
              <MSB>19</MSB>
              <Endianess>LittleEndian</Endianess>
            </MaskedIntReg>
        "#;
        let reg = parse_payload_reg(xml).expect("PayloadSize resolved");
        assert_eq!(reg.mask, Some((4, 19)));
        let raw = (12345u32 << 4).to_le_bytes();
        assert_eq!(decode_payload_value(&reg, &raw), Some(12345));
    }

    #[test]
    fn masked_big_endian_uses_msb_first_numbering() {
        // Low 16 bits of a big-endian 32-bit register are printed bits
        // 16..31: LSB=31, MSB=16 (the ordering the GenApi reference
        // implementation requires). The high flags must be ignored.
        let xml = r#"
            <Integer Name="PayloadSize"><pValue>PayloadReg</pValue></Integer>
            <MaskedIntReg Name="PayloadReg">
              <Address>0x1234</Address>
              <Length>4</Length>
              <LSB>31</LSB>
              <MSB>16</MSB>
              <Endianess>BigEndian</Endianess>
            </MaskedIntReg>
        "#;
        let reg = parse_payload_reg(xml).expect("PayloadSize resolved");
        assert_eq!(reg.mask, Some((0, 15)));
        let raw = 0x4000_3039u32.to_be_bytes();
        assert_eq!(decode_payload_value(&reg, &raw), Some(0x3039));
    }

    #[test]
    fn missing_or_unfollowable_feature_is_none() {
        assert!(parse_payload_reg("<Integer Name=\"Width\"/>").is_none());
        assert!(
            parse_payload_reg("<Integer Name=\"PayloadSize\"><pValue>Nope</pValue></Integer>")
                .is_none()
        );
    }

    #[test]
    fn local_url_parser_uses_hex_address_and_length() {
        let xml = b"<Integer Name=\"PayloadSize\"><pValue>R</pValue></Integer>\
                    <IntReg Name=\"R\"><Address>0x20</Address><Length>4</Length></IntReg>";
        let data = fetch_xml("local:cam.xml;100000;2c", |addr, buf| {
            assert_eq!(addr, 0x0010_0000);
            assert_eq!(buf.len(), 0x2c);
            buf.copy_from_slice(&xml[..0x2c]);
            Ok(())
        })
        .expect("local blob");
        assert_eq!(data, &xml[..0x2c]);
    }

    #[test]
    fn unsupported_scheme_has_no_payload_source() {
        assert!(fetch_xml("http://192.0.2.1/cam.xml", |_, _| Ok(())).is_none());
        assert!(fetch_xml("local:cam.xml;zz;10", |_, _| Ok(())).is_none());
        assert!(fetch_xml("local:cam.xml;100;0", |_, _| Ok(())).is_none());
    }

    #[test]
    fn stream_defaults_follow_packet_size_converter_chain() {
        let xml = r#"
            <Integer Name="GevSCPSPacketSize">
              <pValue>GevSCPSPacketSizeVal_Reg</pValue>
              <pMin>GevSCPSMin_Reg</pMin>
              <pMax>GevSCPSMax_Reg</pMax>
            </Integer>
            <IntConverter Name="GevSCPSPacketSizeVal_Reg">
              <pValue>GevSCPSPacketSizeVal_INT</pValue>
            </IntConverter>
            <IntReg Name="GevSCPSPacketSizeVal_INT">
              <Address>0xd04</Address>
              <Length>4</Length>
              <Endianess>BigEndian</Endianess>
            </IntReg>
            <IntReg Name="GevSCPSMin_Reg">
              <Address>0x10000064</Address>
              <Length>4</Length>
              <Endianess>BigEndian</Endianess>
            </IntReg>
            <IntReg Name="GevSCPSMax_Reg">
              <Address>0x10000068</Address>
              <Length>4</Length>
              <Endianess>BigEndian</Endianess>
            </IntReg>
        "#;
        let d = parse_stream_defaults(xml);
        assert_eq!(d.packet_size.unwrap().address, 0xd04);
        assert_eq!(d.packet_size_min.unwrap().address, 0x1000_0064);
        assert_eq!(d.packet_size_max.unwrap().address, 0x1000_0068);
    }

    #[test]
    fn missing_stream_defaults_are_none() {
        let d = parse_stream_defaults("<Integer Name=\"Width\"/>");
        assert!(d.packet_size.is_none());
        assert!(d.packet_size_min.is_none());
        assert!(d.packet_size_max.is_none());
    }

    #[test]
    fn zero_and_oversized_values_are_rejected() {
        let reg = PayloadReg {
            address: 0,
            length: 4,
            big_endian: true,
            mask: None,
        };
        assert_eq!(decode_payload_value(&reg, &0u32.to_be_bytes()), None);
        let short = [0u8; 2];
        assert_eq!(decode_payload_value(&reg, &short), None);
    }
}
