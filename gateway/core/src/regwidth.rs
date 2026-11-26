//! Register-width map extracted from the GenICam XML a front serves, and
//! width-aware endian block conversion.
//!
//! The gateway relays a camera's register space between transports with
//! opposite byte order (U3V little-endian <-> GigE big-endian). A raw block
//! carries no type information, and the eight-byte case is ambiguous:
//!
//! * two adjacent 32-bit registers must be swapped word by word and keep
//!   their order (the bug a reviewer reproduced: an 8-byte READMEM of
//!   Width+Height came back Height,Width);
//! * one 64-bit register (`SI Required Payload Size`, timestamp, ...) must
//!   be reversed as one value.
//!
//! The served GenICam XML is the only register-type source available at this
//! boundary, so this module extracts the explicit `<Length>` of every
//! `IntReg` / `MaskedIntReg` / `Register` element (resolving one-level
//! `<pAddress>` -> `Integer <Value>` indirection) and converts blocks per
//! register. Addresses the XML does not describe fall back to the 4-byte
//! register assumption (the pre-existing behavior).
//!
//! Limitations (documented, not silent): registers whose address cannot be
//! resolved (deep `pAddress` chains to non-constant nodes) stay unknown and
//! use the 4-byte fallback; unconvertible unaligned tails are left untouched
//! rather than guessed.

use std::collections::HashMap;

/// Widths extracted from one served XML document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegisterWidths {
    /// Sorted by address. Only widths other than 4 are stored: four bytes is
    /// the fallback assumption, so storing it would add no information.
    entries: Vec<(u32, u8)>,
}

impl RegisterWidths {
    /// Extract the register widths from a served GenICam XML document.
    pub fn from_xml(xml: &[u8]) -> Self {
        let text = String::from_utf8_lossy(xml);

        // Pass 1: `Integer` constants, including `pValue` chains, which are
        // what `<pAddress>` usually points at in camera XMLs.
        let mut const_value: HashMap<String, u64> = HashMap::new();
        let mut const_ref: HashMap<String, String> = HashMap::new();
        for (attrs, body) in element_bodies(&text, "Integer") {
            let Some(name) = attribute(attrs, "Name") else { continue };
            if let Some(value) = child_text(body, "Value").and_then(parse_int) {
                const_value.insert(name.to_owned(), value);
            } else if let Some(target) = child_text(body, "pValue") {
                const_ref.insert(name.to_owned(), target.trim().to_owned());
            }
        }
        let resolve_const = |name: &str| -> Option<u32> {
            let mut current = name.to_owned();
            for _ in 0..8 {
                if let Some(v) = const_value.get(&current) {
                    return u32::try_from(*v).ok();
                }
                current = const_ref.get(&current)?.clone();
            }
            None
        };

        // Pass 2: register descriptions (address + explicit length).
        struct Def {
            length: u8,
            address: Option<u32>,
            indirect: Option<String>,
        }
        let mut defs: HashMap<String, Def> = HashMap::new();
        for tag in ["IntReg", "MaskedIntReg", "Register"] {
            for (attrs, body) in element_bodies(&text, tag) {
                let Some(length) = child_text(body, "Length").and_then(parse_int) else {
                    continue;
                };
                let Ok(length) = u8::try_from(length) else { continue };
                let address = child_text(body, "Address").and_then(parse_int);
                let indirect = child_text(body, "pAddress").map(|s| s.trim().to_owned());
                let address = address.and_then(|a| u32::try_from(a).ok());
                if address.is_none() && indirect.is_none() {
                    continue;
                }
                let name = attribute(attrs, "Name").map(str::to_owned);
                if let Some(name) = name {
                    defs.insert(
                        name,
                        Def {
                            length,
                            address,
                            indirect,
                        },
                    );
                }
            }
        }

        // Resolve addresses: direct first, then `pAddress` through constants
        // or another register definition.
        fn resolve(
            name: &str,
            defs: &HashMap<String, Def>,
            resolve_const: &dyn Fn(&str) -> Option<u32>,
            depth: u8,
        ) -> Option<u32> {
            if depth == 0 {
                return None;
            }
            let def = defs.get(name)?;
            if let Some(address) = def.address {
                return Some(address);
            }
            let target = def.indirect.as_deref()?;
            resolve_const(target).or_else(|| resolve(target, defs, resolve_const, depth - 1))
        }

        let mut entries: Vec<(u32, u8)> = Vec::new();
        for (name, def) in &defs {
            let Some(address) = resolve(name, &defs, &resolve_const, 8) else {
                continue;
            };
            if def.length == 0 || def.length > 8 || def.length == 4 {
                continue;
            }
            entries.push((address, def.length));
        }
        entries.sort_unstable();
        entries.dedup();
        Self { entries }
    }

    /// Declared width of the register starting exactly at `addr`.
    pub fn width_at(&self, addr: u32) -> Option<u8> {
        self.entries
            .binary_search_by_key(&addr, |(a, _)| *a)
            .ok()
            .map(|i| self.entries[i].1)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Reverse each register in `data` between two transport byte orders.
///
/// `addr` is the register-space address of `data[0]`. Registers covered by
/// `widths` are reversed as one value (2, 8, ... bytes); everything else is
/// treated as a 32-bit register. A tail that cannot be aligned to a whole
/// register is left untouched instead of guessed.
pub fn swap_block(data: &mut [u8], addr: u32, widths: &RegisterWidths) {
    let mut pos = 0usize;
    while pos < data.len() {
        let current = addr.wrapping_add(pos as u32);
        if let Some(width) = widths.width_at(current) {
            let width = usize::from(width);
            if width >= 2 && pos + width <= data.len() {
                data[pos..pos + width].reverse();
                pos += width;
                continue;
            }
        }
        if data.len() - pos >= 4 && current % 4 == 0 {
            data[pos..pos + 4].reverse();
            pos += 4;
        } else {
            break;
        }
    }
}

// -- small XML scanning helpers (no XML crate in this workspace) ------------

/// `(open-tag attribute text, inner text)` for every non-self-closing
/// element named `tag`. Nested elements of the same name are not expected in
/// GenICam register descriptions.
fn element_bodies<'a>(xml: &'a str, tag: &str) -> Vec<(&'a str, &'a str)> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let delimited = after
            .chars()
            .next()
            .map(|c| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(false);
        if !delimited {
            rest = after;
            continue;
        }
        let Some(tag_end) = after.find('>') else { break };
        let attrs = &after[..tag_end];
        if attrs.trim_end().ends_with('/') {
            rest = &after[tag_end + 1..];
            continue;
        }
        let inner_start = tag_end + 1;
        let Some(inner_end) = after[inner_start..].find(&close) else {
            break;
        };
        out.push((attrs, &after[inner_start..inner_start + inner_end]));
        rest = &after[inner_start + inner_end + close.len()..];
    }
    out
}

fn attribute<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let needle = format!("{name}={quote}");
        if let Some(start) = attrs.find(&needle) {
            let value = &attrs[start + needle.len()..];
            if let Some(end) = value.find(quote) {
                return Some(&value[..end]);
            }
        }
    }
    None
}

fn child_text<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(&body[start..end])
}

fn parse_int(text: &str) -> Option<u64> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        text.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0"?>
<RegisterDescription>
  <Integer Name="PayloadSizeAddr"><Value>0x00081000</Value></Integer>
  <Integer Name="PayloadSizeAddrAlias"><pValue>PayloadSizeAddr</pValue></Integer>
  <IntReg Name="WidthReg">
    <Address>0x00080000</Address>
    <Length>4</Length>
    <AccessMode>RW</AccessMode>
  </IntReg>
  <IntReg Name="HeightReg">
    <Address>0x00080004</Address>
    <Length>4</Length>
  </IntReg>
  <MaskedIntReg Name="BitReg">
    <Address>0x00080008</Address>
    <Length>4</Length>
    <LSB>0</LSB><MSB>0</MSB>
  </MaskedIntReg>
  <IntReg Name="PayloadSizeReg">
    <pAddress>PayloadSizeAddrAlias</pAddress>
    <Length>8</Length>
  </IntReg>
  <IntReg Name="ShortReg">
    <Address>0x00080010</Address>
    <Length>2</Length>
  </IntReg>
  <IntReg Name="Unresolvable">
    <pAddress>SomeSwissKnife</pAddress>
    <Length>8</Length>
  </IntReg>
</RegisterDescription>"#;

    #[test]
    fn widths_come_from_direct_and_indirect_registers() {
        let widths = RegisterWidths::from_xml(XML.as_bytes());
        // Four-byte registers need no entry (fallback covers them).
        assert_eq!(widths.width_at(0x0008_0000), None);
        assert_eq!(widths.width_at(0x0008_0004), None);
        // Eight-byte register resolved through an Integer pValue chain.
        assert_eq!(widths.width_at(0x0008_1000), Some(8));
        // Two-byte register: needs explicit handling, a word swap would move
        // it to the wrong offset.
        assert_eq!(widths.width_at(0x0008_0010), Some(2));
        // Unresolvable indirect address is not guessed.
        assert_eq!(widths.width_at(0), None);
        assert_eq!(widths.len(), 2);
    }

    /// The reviewer's reproduction: an eight-byte READMEM covering Width and
    /// Height (two 32-bit registers, little-endian backend) must come back as
    /// Width,Height in big-endian register-space order.
    #[test]
    fn two_32_bit_registers_keep_their_order() {
        let widths = RegisterWidths::from_xml(XML.as_bytes());
        // Width=1280=0x500, Height=1024=0x400, little-endian words.
        let mut block = [0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00];
        swap_block(&mut block, 0x0008_0000, &widths);
        assert_eq!(block, [0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x00]);
        assert_eq!(
            u32::from_be_bytes(block[0..4].try_into().unwrap()),
            1280
        );
        assert_eq!(
            u32::from_be_bytes(block[4..8].try_into().unwrap()),
            1024
        );
    }

    /// A declared 64-bit register at the same eight-byte width is reversed as
    /// one value (PayloadSize semantics).
    #[test]
    fn declared_64_bit_register_is_reversed_as_one_value() {
        let widths = RegisterWidths::from_xml(XML.as_bytes());
        let mut block = 0x0026_2140u64.to_le_bytes(); // 2,621,440
        swap_block(&mut block, 0x0008_1000, &widths);
        assert_eq!(u64::from_be_bytes(block), 0x0026_2140);
    }

    /// Sixteen-bit registers are swapped as two bytes, not as a 32-bit word.
    #[test]
    fn declared_16_bit_register_is_swapped_in_place() {
        let widths = RegisterWidths::from_xml(XML.as_bytes());
        let mut block = [0x34, 0x12, 0xAA, 0xBB];
        swap_block(&mut block, 0x0008_0010, &widths);
        assert_eq!(block, [0x12, 0x34, 0xAA, 0xBB]);
    }

    #[test]
    fn unknown_addresses_fall_back_to_32_bit_words() {
        let widths = RegisterWidths::from_xml(XML.as_bytes());
        let mut block = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        swap_block(&mut block, 0x0009_0000, &widths);
        assert_eq!(block, [4, 3, 2, 1, 8, 7, 6, 5, 12, 11, 10, 9]);
    }

    #[test]
    fn empty_or_unparseable_xml_yields_no_widths() {
        assert!(RegisterWidths::from_xml(b"").is_empty());
        assert!(RegisterWidths::from_xml(b"<RegisterDescription/>").is_empty());
        assert!(RegisterWidths::from_xml(b"not xml at all").is_empty());
    }
}
