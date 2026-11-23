//! XML customization layer: inject a built-in SmartCamera category (the
//! 0x2000_0000 custom window as GenApi nodes) plus an optional
//! operator-supplied node fragment into the camera's GenICam XML, and
//! serve the result as the virtual device's XML blob. Fetching the raw
//! camera XML (First URL, `local:` blob read, transparent zip) is the
//! back adapter's job ([`crate::back::CameraBack::fetch_xml`]) — this type
//! starts from the raw bytes.

use gentl_core::{GentlError, GentlResult};
use gentl_gvcp::registers as reg;

use crate::XML_BASE_V;

/// Injected device description served by the virtual device.
pub struct XmlMerger {
    blob: Vec<u8>,
    /// `local:vgige-<content hash>.xml;<XML_BASE_V hex>;<len hex>` — never
    /// longer than the 512-byte First URL slot.
    url: String,
    /// Feature nodes in the built-in SmartCamera fragment (ready-line stats).
    node_count: usize,
}

impl XmlMerger {
    /// Build the injected blob from the raw camera XML and the optional
    /// operator fragment (a file's content, inserted verbatim).
    pub fn new(camera_xml: &[u8], ext: Option<&[u8]>) -> GentlResult<Self> {
        Self::build(camera_xml, ext, None)
    }

    /// Like [`new`](Self::new), but rewrites every `Endianess` declaration
    /// to the virtual front's native byte order. The gateway converts
    /// pass-through register blocks to that same order, so a GigE front
    /// (big-endian wire) and a U3V front (little-endian GenCP) can both
    /// serve a little-endian null backend correctly.
    pub fn new_with_endian(
        camera_xml: &[u8],
        ext: Option<&[u8]>,
        big_endian: bool,
    ) -> GentlResult<Self> {
        Self::build(camera_xml, ext, Some(big_endian))
    }

    fn build(
        camera_xml: &[u8],
        ext: Option<&[u8]>,
        target_big_endian: Option<bool>,
    ) -> GentlResult<Self> {
        let camera_xml = String::from_utf8(camera_xml.to_vec())
            .map_err(|_| GentlError::Io("camera XML is not UTF-8".into()))?;
        let (mut blob, mut node_count) = build_injected_xml(&camera_xml, ext)?;
        // A GigE personality must expose the standard transport features
        // (§37.5.4 and SFNC GevSCP*/GevSCD*), even when the downstream
        // camera speaks U3V or is the null source and therefore has no
        // GigE nodes. A real GigE camera XML already carries them: only the
        // missing ones are injected, so the document never has duplicates.
        if target_big_endian == Some(true) {
            let (with_transport, transport_nodes) = inject_gige_transport(blob);
            blob = with_transport;
            node_count += transport_nodes;
        }
        if let Some(big) = target_big_endian {
            blob = rewrite_endian(blob, big);
            if big {
                blob = rewrite_gige_identity(blob);
            }
        }
        // NI-IMAQdx and GenApi-based consumers cache the downloaded XML by
        // URL (NI: `Data\XML\<name>.xml`; GenICam: the GENICAM_CACHE entry
        // keyed by XMLVersion plus device identity). A device whose XML
        // changes but keeps the same URL/version hands the host a stale
        // feature tree: measured 2026-11-20, NI MAX validated FrameRate
        // against a cached Min/Max 0..240 while the live device allowed
        // 1..1e6. Hash the content into the file name so any XML document
        // change becomes a new cache entry.
        let revision = content_revision(blob.as_bytes());
        let url = format!(
            "local:vgige-{revision:08x}.xml;{XML_BASE_V:x};{:x}",
            blob.len()
        );
        if url.len() > 512 {
            return Err(GentlError::Io("injected XML URL exceeds the 512-byte First URL slot".into()));
        }
        Ok(Self {
            blob: blob.into_bytes(),
            url,
            node_count,
        })
    }

    /// Injected XML bytes served at [`XML_BASE_V`].
    pub fn blob(&self) -> &[u8] {
        &self.blob
    }

    /// Base address of the blob on the virtual device.
    pub fn base(&self) -> u32 {
        XML_BASE_V
    }

    /// Virtual First URL content (without NUL padding).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Number of feature nodes in the built-in SmartCamera fragment.
    pub fn node_count(&self) -> usize {
        self.node_count
    }
}

/// FNV-1a 32-bit content revision embedded in the virtual XML URL. Not a
/// security hash: it only has to change whenever the document changes, so a
/// host's URL-keyed XML cache cannot serve a stale feature tree.
fn content_revision(blob: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in blob {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Rewrite every int-register endianness declaration to `big` (GigE wire)
/// or little (GenCP/U3V wire). The gateway's register boundary performs the
/// matching byte conversion.
fn rewrite_endian(mut xml: String, big: bool) -> String {
    if big {
        xml = xml.replace(
            "<Endianess>LittleEndian</Endianess>",
            "<Endianess>BigEndian</Endianess>",
        );
    } else {
        xml = xml.replace(
            "<Endianess>BigEndian</Endianess>",
            "<Endianess>LittleEndian</Endianess>",
        );
    }
    xml
}

/// Give the GigE personality its own GenICam identity. NI-IMAQdx caches
/// downloaded XML by vendor/model/serial; U3V and GigE otherwise share
/// `GenTLite/Virtual/NULL-0001` and the first download (little-endian U3V)
/// would be reused for the big-endian GigE device, producing
/// `AcquisitionFrameRate=503316480` again.
fn rewrite_gige_identity(blob: String) -> String {
    blob.replace(
        "<String Name=\"DeviceModelName\" NameSpace=\"Standard\"><Value>Virtual</Value></String>",
        "<String Name=\"DeviceModelName\" NameSpace=\"Standard\"><Value>Virtual GigE</Value></String>",
    )
    .replace(
        "<String Name=\"DeviceSerialNumber\" NameSpace=\"Standard\"><Value>NULL-0001</Value></String>",
        "<String Name=\"DeviceSerialNumber\" NameSpace=\"Standard\"><Value>NULL-0001-GE</Value></String>",
    )
}

/// Insert the SmartCamera fragment and the optional operator fragment before
/// `</RegisterDescription>`. String surgery only — no XML parser dependency.
/// Returns the injected document and the built-in fragment's feature count.
fn build_injected_xml(
    camera_xml: &str,
    ext: Option<&[u8]>,
) -> GentlResult<(String, usize)> {
    let closing = "</RegisterDescription>";
    let cut = camera_xml
        .rfind(closing)
        .ok_or_else(|| GentlError::Io("camera XML has no </RegisterDescription>".into()))?;
    // Idempotency: a downstream camera may itself be one of our virtual
    // devices (e.g. the U3V gadget front), whose served XML already carries
    // this built-in fragment. Injecting it a second time produces duplicate
    // GenApi feature names and hosts reject the whole document (NI-IMAQdx:
    // "Unable to load camera's XML file"; XSD: duplicate FeatureKey).
    let smart = if has_node(camera_xml, "SmartCamera") {
        String::new()
    } else {
        smart_camera_fragment()
    };
    let node_count = smart.matches("<pFeature>").count();
    let mut out = String::with_capacity(camera_xml.len() + smart.len() + ext.map_or(0, <[u8]>::len));
    out.push_str(&camera_xml[..cut]);
    out.push_str(&smart);
    if let Some(ext) = ext {
        let ext = std::str::from_utf8(ext)
            .map_err(|_| GentlError::Io("ext nodes fragment is not UTF-8".into()))?;
        out.push_str(ext);
        if !ext.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&camera_xml[cut..]);
    // The injected category is only reachable through the GenApi feature
    // tree when Root references it: a camera XML that never had a
    // SmartCamera node needs the `<pFeature>` link or NI/pylon show the
    // fragment as an orphan (the null XML already carries the reference, so
    // this is a no-op there).
    let out = attach_to_root(out, "SmartCamera");
    Ok((out, node_count))
}

/// Insert `feature` into the `Root` category's `pFeature` list unless the
/// document already references it. GenApi only exposes categories reachable
/// from Root; an injected `<Category>` without the link is invisible in host
/// attribute trees (and in NI's `.icd` paths).
fn attach_to_root(xml: String, feature: &str) -> String {
    let reference = format!("<pFeature>{feature}</pFeature>");
    if xml.contains(&reference) {
        return xml;
    }
    let Some(pos) = category_close(&xml, "Root") else {
        return xml;
    };
    insert_at(xml, pos, &format!("    {reference}\n"))
}

/// Byte offset of the `</Category>` closing the first
/// `<Category Name="{name}"` in the document. String surgery only: the
/// producer/consumer contract is "well-formed camera XML", and Root's direct
/// children are `<pFeature>` references (no nested categories), so the first
/// close tag after the opening one is the matching one.
fn category_close(xml: &str, name: &str) -> Option<usize> {
    let open = format!("<Category Name=\"{name}\"");
    let start = xml.find(&open)?;
    xml[start..].find("</Category>").map(|p| start + p)
}

fn insert_at(mut xml: String, pos: usize, text: &str) -> String {
    xml.insert_str(pos, text);
    xml
}

/// Insert the standard GigE Vision transport features for stream channel 0
/// when the camera XML does not already declare them, into a
/// `TransportLayerControl` category that Root references (§37.5: that
/// category is the standard home of the `Gev*` features, and GenApi only
/// exposes features reachable from Root). The addresses are the spec's fixed
/// stream-channel map (§36.57–§36.60); the virtual front routes them to its
/// local channel, so they never reach the downstream camera.
/// Returns the document and the number of injected features (4 when the
/// camera had no GigE transport nodes at all).
fn inject_gige_transport(xml: String) -> (String, usize) {
    let mut nodes = String::new();
    let mut features: Vec<&str> = Vec::new();

    if !has_node(&xml, "GevSCPSPacketSize") {
        gige_int_feature(
            &mut nodes,
            "GevSCPSPacketSize",
            "Packet Size",
            "Total GVSP packet size in bytes, including IP, UDP and GVSP headers (GigE Vision §36.58).",
            "GevSCPSPacketSize_Reg",
            "576",
            "65535",
        );
        // packet_size is the low 16 bits; the FIRE/DF flags above it are
        // not part of the SFNC packet-size feature. GenICam numbers the bits
        // of a big-endian register MSB-first (bit 0 is the MSB), so the low
        // 16 bits are printed bits 16..31: LSB=31, MSB=16. The reference
        // GenApi implementation rejects LSB < MSB for BigEndian registers.
        gige_masked_reg(&mut nodes, "GevSCPSPacketSize_Reg", reg::SCPS0, 31, 16);
        features.push("GevSCPSPacketSize");
    }
    if !has_node(&xml, "GevSCPHostPort") {
        gige_int_feature(
            &mut nodes,
            "GevSCPHostPort",
            "Host Port",
            "Host UDP port of the stream channel; writing 0 closes it (GigE Vision §36.57).",
            "GevSCPHostPort_Reg",
            "0",
            "65535",
        );
        gige_masked_reg(&mut nodes, "GevSCPHostPort_Reg", reg::SCP0, 31, 16);
        features.push("GevSCPHostPort");
    }
    if !has_node(&xml, "GevSCDA") {
        gige_int_feature(
            &mut nodes,
            "GevSCDA",
            "Destination Address",
            "Stream channel destination IPv4 address (GigE Vision §36.60).",
            "GevSCDA_Reg",
            "0",
            "4294967295",
        );
        gige_int_reg(&mut nodes, "GevSCDA_Reg", reg::SCDA0);
        features.push("GevSCDA");
    }
    if !has_node(&xml, "GevSCPD") {
        gige_int_feature(
            &mut nodes,
            "GevSCPD",
            "Packet Delay",
            "Minimal inter-packet delay in timestamp ticks (GigE Vision §36.59).",
            "GevSCPD_Reg",
            "0",
            "4294967295",
        );
        gige_int_reg(&mut nodes, "GevSCPD_Reg", reg::SCPD0);
        features.push("GevSCPD");
    }

    if features.is_empty() {
        return (xml, 0);
    }
    let count = features.len();
    let Some(cut) = xml.rfind("</RegisterDescription>") else {
        return (xml, 0);
    };
    let mut out = String::with_capacity(xml.len() + nodes.len() + 1024);
    out.push_str(&xml[..cut]);
    out.push_str(&nodes);
    out.push_str(&xml[cut..]);

    let pfeatures: String = features
        .iter()
        .map(|f| format!("    <pFeature>{f}</pFeature>\n"))
        .collect();
    let out = match category_close(&out, "TransportLayerControl") {
        // Real cameras already have the standard transport category: add the
        // injected features to it instead of creating a duplicate.
        Some(pos) => insert_at(out, pos, &pfeatures),
        None => {
            let category = format!(
                "  <Category Name=\"TransportLayerControl\" NameSpace=\"Standard\">\n\
                 \x20     <ToolTip>Transport layer control (GigE Vision stream channel 0).</ToolTip>\n\
                 \x20     <Description>Transport layer control.</Description>\n\
                 \x20     <DisplayName>Transport Layer Control</DisplayName>\n\
                 \x20     <Visibility>Beginner</Visibility>\n\
                 {pfeatures}  </Category>\n"
            );
            let out = insert_at(out, cut, &category);
            attach_to_root(out, "TransportLayerControl")
        }
    };
    (out, count)
}

fn has_node(xml: &str, name: &str) -> bool {
    xml.contains(&format!("Name=\"{name}\"")) || xml.contains(&format!("Name='{name}'"))
}

fn gige_int_feature(
    x: &mut String,
    name: &str,
    display: &str,
    tooltip: &str,
    pvalue: &str,
    min: &str,
    max: &str,
) {
    x.push_str(&format!(
        "    <Integer Name=\"{name}\" NameSpace=\"Standard\">\n\
         \x20     <ToolTip>{tooltip}</ToolTip>\n\
         \x20     <Description>{tooltip}</Description>\n\
         \x20     <DisplayName>{display}</DisplayName>\n\
         \x20     <Visibility>Beginner</Visibility>\n\
         \x20     <pValue>{pvalue}</pValue>\n\
         \x20     <Min>{min}</Min>\n\
         \x20     <Max>{max}</Max>\n\
         \x20     <Inc>1</Inc>\n\
         \x20   </Integer>\n"
    ));
}

fn gige_masked_reg(x: &mut String, name: &str, addr: u32, lsb: u8, msb: u8) {
    x.push_str(&format!(
        "    <MaskedIntReg Name=\"{name}\" NameSpace=\"Custom\">\n\
         \x20     <Address>0x{addr:08X}</Address>\n\
         \x20     <Length>4</Length>\n\
         \x20     <AccessMode>RW</AccessMode>\n\
         \x20     <pPort>Device</pPort>\n\
         \x20     <LSB>{lsb}</LSB>\n\
         \x20     <MSB>{msb}</MSB>\n\
         \x20     <Sign>Unsigned</Sign>\n\
         \x20     <Endianess>BigEndian</Endianess>\n\
         \x20   </MaskedIntReg>\n"
    ));
}

fn gige_int_reg(x: &mut String, name: &str, addr: u32) {
    x.push_str(&format!(
        "    <IntReg Name=\"{name}\" NameSpace=\"Custom\">\n\
         \x20     <Address>0x{addr:08X}</Address>\n\
         \x20     <Length>4</Length>\n\
         \x20     <AccessMode>RW</AccessMode>\n\
         \x20     <pPort>Device</pPort>\n\
         \x20     <Sign>Unsigned</Sign>\n\
         \x20     <Endianess>BigEndian</Endianess>\n\
         \x20   </IntReg>\n"
    ));
}

/// Built-in SmartCamera category mapping the custom window
/// (0x2000_0000..) as GenApi nodes: one `<Integer>` feature per slot,
/// backed by an `<IntReg>` pointing straight at the slot address (the same
/// `pValue` chain shape the simulator's XML uses).
fn smart_camera_fragment() -> String {
    const B: u32 = crate::CUSTOM_BASE;
    let mut x = String::with_capacity(3 * 1024);
    x.push_str("  <Group Comment=\"SmartCamera\">\n");
    x.push_str(
        "    <Category Name=\"SmartCamera\" NameSpace=\"Standard\">\n\
         \x20     <ToolTip>vgige smart-camera gateway features.</ToolTip>\n\
         \x20     <Description>Inspection results and relay statistics served locally by the vgige gateway (register window 0x20000000); reads and writes never reach the camera.</Description>\n\
         \x20     <DisplayName>Smart Camera</DisplayName>\n\
         \x20     <Visibility>Expert</Visibility>\n\
         \x20     <ImposedAccessMode>RO</ImposedAccessMode>\n\
         \x20     <pFeature>VgigeVersion</pFeature>\n\
         \x20     <pFeature>AIResult</pFeature>\n\
         \x20     <pFeature>AIScore</pFeature>\n\
         \x20     <pFeature>FrameCountRelayed</pFeature>\n\
         \x20     <pFeature>UserCommand</pFeature>\n\
         \x20   </Category>\n",
    );
    int_feature(&mut x, "VgigeVersion", "vgige Version", "Gateway semantic version (major&lt;&lt;16 | minor&lt;&lt;8 | patch).", "VgigeVersionReg");
    int_reg(&mut x, "VgigeVersionReg", B, "RO");
    int_feature(&mut x, "AIResult", "AI Result", "Inspection result: 0 = not inspected, 1 = OK, 2 = NG.", "AIResultReg");
    int_reg(&mut x, "AIResultReg", B + 4, "RO");
    int_feature(&mut x, "AIScore", "AI Score", "Anomaly score multiplied by 1000.", "AIScoreReg");
    int_reg(&mut x, "AIScoreReg", B + 8, "RO");
    int_feature(&mut x, "FrameCountRelayed", "Frames Relayed", "Number of GVSP blocks relayed to the remote client since the channel opened.", "FrameCountRelayedReg");
    int_reg(&mut x, "FrameCountRelayedReg", B + 12, "RO");
    int_feature(&mut x, "UserCommand", "User Command", "Write-only business hook; the gateway logs every write.", "UserCommandReg");
    int_reg(&mut x, "UserCommandReg", B + 16, "WO");
    x.push_str("  </Group>\n");
    x
}

fn int_feature(x: &mut String, name: &str, display: &str, tooltip: &str, pvalue: &str) {
    x.push_str(&format!(
        "    <Integer Name=\"{name}\" NameSpace=\"Custom\">\n\
         \x20     <ToolTip>{tooltip}</ToolTip>\n\
         \x20     <Description>{tooltip}</Description>\n\
         \x20     <DisplayName>{display}</DisplayName>\n\
         \x20     <Visibility>Expert</Visibility>\n\
         \x20     <pValue>{pvalue}</pValue>\n\
         \x20     <Min>0</Min>\n\
         \x20     <Max>4294967295</Max>\n\
         \x20   </Integer>\n"
    ));
}

fn int_reg(x: &mut String, name: &str, addr: u32, access: &str) {
    x.push_str(&format!(
        "    <IntReg Name=\"{name}\" NameSpace=\"Custom\">\n\
         \x20     <Address>0x{addr:08X}</Address>\n\
         \x20     <Length>4</Length>\n\
         \x20     <AccessMode>{access}</AccessMode>\n\
         \x20     <pPort>Device</pPort>\n\
         \x20     <Sign>Unsigned</Sign>\n\
         \x20     <Endianess>BigEndian</Endianess>\n\
         \x20   </IntReg>\n"
    ));
}

#[cfg(test)]
mod tests {

    /// A downstream camera that already serves our SmartCamera fragment
    /// (e.g. the U3V gadget front, whose XML this gateway then wraps) must
    /// not get a second copy: duplicate GenApi feature names make every
    /// host reject the document (NI: "Unable to load camera's XML file").
    #[test]
    fn injected_smart_camera_fragment_is_idempotent() {
        let camera_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<RegisterDescription xmlns="http://www.genicam.org/GenApi/Version_1_1">
  <Category Name="Root" NameSpace="Standard">
    <pFeature>SmartCamera</pFeature>
    <pFeature>DeviceControl</pFeature>
  </Category>
  <Category Name="SmartCamera" NameSpace="Custom">
    <pFeature>VgigeVersion</pFeature>
  </Category>
  <Integer Name="VgigeVersion" NameSpace="Custom">
    <pValue>VgigeVersionReg</pValue>
  </Integer>
  <IntReg Name="VgigeVersionReg" NameSpace="Custom">
    <Address>536870912</Address>
    <Length>4</Length>
    <AccessMode>RO</AccessMode>
    <pPort>Device</pPort>
  </IntReg>
  <Category Name="DeviceControl" NameSpace="Standard">
    <pFeature>DeviceReset</pFeature>
  </Category>
</RegisterDescription>
"#;
        let merged = XmlMerger::new(camera_xml.as_bytes(), None).expect("merge");
        let text = String::from_utf8_lossy(merged.blob());
        assert_eq!(
            text.matches("Name=\"SmartCamera\"").count(),
            1,
            "SmartCamera category stays unique"
        );
        assert_eq!(
            text.matches("Name=\"VgigeVersion\"").count(),
            1,
            "feature stays unique"
        );
        assert_eq!(
            text.matches("<pFeature>SmartCamera</pFeature>").count(),
            1,
            "Root keeps a single reference"
        );
    }
    use super::*;

    /// Minimal camera document in the simulator's shape.
    fn camera_xml() -> &'static str {
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <RegisterDescription xmlns=\"http://www.genicam.org/GenApi/Version_1_1\" ModelName=\"SimCam\" VendorName=\"GenTLite\">\n\
         \x20 <Group Comment=\"Categories\">\n\
         \x20   <Category Name=\"Root\" NameSpace=\"Standard\">\n\
         \x20     <pFeature>Width</pFeature>\n\
         \x20   </Category>\n\
         \x20 </Group>\n\
         \x20 <Group Comment=\"ImageFormat\">\n\
         \x20   <Integer Name=\"Width\" NameSpace=\"Standard\">\n\
         \x20     <pValue>WidthReg</pValue>\n\
         \x20   </Integer>\n\
         \x20   <IntReg Name=\"WidthReg\" NameSpace=\"Custom\">\n\
         \x20     <Address>0x00010000</Address>\n\
         \x20   </IntReg>\n\
         \x20 </Group>\n\
         </RegisterDescription>\n"
    }

    /// Hand-rolled tag-balance check: scan for element names (a name starts
    /// with a letter/underscore after '<'), then for each require that
    /// non-self-closing opens equal "</name>" closings. No XML crate on
    /// purpose (v1 discipline).
    fn assert_tag_balance(xml: &str) {
        let bytes = xml.as_bytes();
        let mut names: Vec<String> = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'<' && i + 1 < bytes.len() && bytes[i + 1] != b'/' && bytes[i + 1] != b'?' && bytes[i + 1] != b'!' {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                // A real element name starts with a letter or underscore
                // (text content between tags can start with digits).
                if end > start && (bytes[start].is_ascii_alphabetic() || bytes[start] == b'_') {
                    let name = &xml[start..end];
                    if !names.iter().any(|n| n == name) {
                        names.push(name.to_owned());
                    }
                }
            }
            i += 1;
        }
        for name in names {
            let needle = format!("<{name}");
            let mut opens = 0usize;
            let mut rest = xml;
            while let Some(p) = rest.find(&needle) {
                let after = &rest[p + needle.len()..];
                let boundary = after
                    .chars()
                    .next()
                    .map_or(true, |c| !(c.is_ascii_alphanumeric() || c == '_'));
                if boundary {
                    // Exclude self-closing tags ("<X ... />").
                    let head = after.split('>').next().unwrap_or(after);
                    if !head.trim_end().ends_with('/') {
                        opens += 1;
                    }
                }
                rest = &rest[p + needle.len()..];
            }
            let closes = xml.matches(&format!("</{name}>")).count();
            assert_eq!(opens, closes, "tag <{name}> not balanced");
        }
    }

    #[test]
    fn injection_is_balanced_and_keeps_camera_nodes() {
        let m = XmlMerger::new(camera_xml().as_bytes(), None).unwrap();
        let out = std::str::from_utf8(m.blob()).unwrap();
        assert_tag_balance(out);
        // Camera's own nodes survive verbatim.
        assert!(out.contains("<Integer Name=\"Width\" NameSpace=\"Standard\">"));
        assert!(out.contains("<IntReg Name=\"WidthReg\" NameSpace=\"Custom\">"));
        // Injected fragment is inside the document, before the closing tag.
        let cut = out.rfind("</RegisterDescription>").unwrap();
        let smart = out.find("<Category Name=\"SmartCamera\"").unwrap();
        assert!(smart < cut);
        assert!(out[cut..].trim_start().starts_with("</RegisterDescription>"));
        // Feature count and register wiring of the built-in nodes.
        assert_eq!(m.node_count(), 5);
        for (addr, reg) in [
            (0x2000_0000u32, "VgigeVersionReg"),
            (0x2000_0004, "AIResultReg"),
            (0x2000_0008, "AIScoreReg"),
            (0x2000_000C, "FrameCountRelayedReg"),
            (0x2000_0010, "UserCommandReg"),
        ] {
            let needle = format!("<IntReg Name=\"{reg}\"");
            let at = out.find(&needle).unwrap();
            assert!(out[at..at + 220].contains(&format!("Address>0x{addr:08X}</Address>")));
        }
        assert!(out.contains("<AccessMode>WO</AccessMode>"));
        // The injected category is reachable from Root (NI/pylon only show
        // categories referenced by Root's pFeature list).
        assert!(out.contains("<pFeature>SmartCamera</pFeature>"));
        // Virtual URL points at the blob and carries the content revision.
        assert!(m.url().starts_with("local:vgige-"));
        assert!(m.url().contains(".xml;"));
        assert!(m.url().ends_with(&format!("{:x}", m.blob().len())));
    }

    #[test]
    fn xml_url_tracks_content_revision() {
        // Same document -> same URL (host caches stay valid).
        let a = XmlMerger::new(camera_xml().as_bytes(), None).unwrap();
        let b = XmlMerger::new(camera_xml().as_bytes(), None).unwrap();
        assert_eq!(a.url(), b.url());
        // Content change -> new URL, so an URL-keyed host cache cannot serve
        // the stale feature tree (2026-11-20 NI MAX FrameRate incident).
        let changed = camera_xml().replace("ModelName=\"SimCam\"", "ModelName=\"SimCam2\"");
        let c = XmlMerger::new(changed.as_bytes(), None).unwrap();
        assert_ne!(a.url(), c.url());
        assert!(!changed.is_empty());
    }

    #[test]
    fn ext_nodes_inserted_verbatim() {
        let ext = b"  <Group Comment=\"Operator\"><Integer Name=\"MyNode\" NameSpace=\"Custom\"><pValue>MyNodeReg</pValue></Integer></Group>\n";
        let m = XmlMerger::new(camera_xml().as_bytes(), Some(ext)).unwrap();
        let out = std::str::from_utf8(m.blob()).unwrap();
        assert_tag_balance(out);
        assert!(out.contains("<Integer Name=\"MyNode\" NameSpace=\"Custom\">"));
        let cut = out.rfind("</RegisterDescription>").unwrap();
        assert!(out.find("<Category Name=\"SmartCamera\"").unwrap() < cut);
    }

    #[test]
    fn gige_personality_injects_standard_transport_nodes() {
        let m = XmlMerger::new_with_endian(camera_xml().as_bytes(), None, true).unwrap();
        let out = std::str::from_utf8(m.blob()).unwrap();
        assert_tag_balance(out);
        for (node, addr) in [
            ("GevSCPSPacketSize", 0x0D04u32),
            ("GevSCPHostPort", 0x0D00),
            ("GevSCDA", 0x0D18),
            ("GevSCPD", 0x0D08),
        ] {
            assert!(
                out.contains(&format!("Name=\"{node}\"")),
                "missing feature {node}"
            );
            let reg_name = format!("{node}_Reg");
            let at = out
                .find(&format!("Name=\"{reg_name}\""))
                .unwrap_or_else(|| panic!("missing register {reg_name}"));
            assert!(
                out[at..at + 240].contains(&format!("Address>0x{addr:08X}</Address>")),
                "{reg_name} does not point at 0x{addr:08X}"
            );
        }
        // The packet-size/host-port fields are masked to their 16-bit fields
        // (the surrounding SCP/SCPS flag bits are not part of the feature).
        // Big-endian GenICam numbering is MSB-first: LSB=31, MSB=16.
        assert!(out.contains("<MaskedIntReg Name=\"GevSCPSPacketSize_Reg\""));
        assert!(out.contains("<MaskedIntReg Name=\"GevSCPHostPort_Reg\""));
        assert_eq!(out.matches("<LSB>31</LSB>").count(), 2);
        assert_eq!(out.matches("<MSB>16</MSB>").count(), 2);
        // §37.5: the injected features live in TransportLayerControl, which
        // Root references (otherwise NI/pylon treat them as orphans).
        assert!(out.contains("<Category Name=\"TransportLayerControl\""));
        assert!(out.contains("<pFeature>TransportLayerControl</pFeature>"));
        assert_eq!(
            out.matches("Name=\"TransportLayerControl\"").count(),
            1,
            "exactly one transport category"
        );
        assert_eq!(m.node_count(), 9, "5 built-in + 4 transport nodes");
    }

    #[test]
    fn injected_features_join_an_existing_transport_category() {
        // Real cameras already declare TransportLayerControl: injected
        // features must join it instead of creating a duplicate category.
        let camera = camera_xml().replace(
            "  </Group>\n</RegisterDescription>",
            "  </Group>\n  <Category Name=\"TransportLayerControl\" NameSpace=\"Standard\">\n    <pFeature>CameraOnlyTransportNode</pFeature>\n  </Category>\n</RegisterDescription>",
        );
        let m = XmlMerger::new_with_endian(camera.as_bytes(), None, true).unwrap();
        let out = std::str::from_utf8(m.blob()).unwrap();
        assert_tag_balance(out);
        assert_eq!(out.matches("<Category Name=\"TransportLayerControl\"").count(), 1);
        let transport = out.find("<Category Name=\"TransportLayerControl\"").unwrap();
        let close = out[transport..].find("</Category>").unwrap() + transport;
        let block = &out[transport..close];
        for node in [
            "CameraOnlyTransportNode",
            "GevSCPSPacketSize",
            "GevSCPHostPort",
            "GevSCDA",
            "GevSCPD",
        ] {
            assert!(
                block.contains(&format!("<pFeature>{node}</pFeature>")),
                "{node} missing from the existing TransportLayerControl category"
            );
        }
    }

    #[test]
    fn u3v_personality_does_not_get_gige_transport_nodes() {
        let m = XmlMerger::new_with_endian(camera_xml().as_bytes(), None, false).unwrap();
        let out = std::str::from_utf8(m.blob()).unwrap();
        assert!(!out.contains("GevSCPSPacketSize"));
        assert!(!out.contains("GevSCDA"));
        assert_eq!(m.node_count(), 5);
    }

    #[test]
    fn existing_gige_transport_nodes_are_not_duplicated() {
        let camera = camera_xml().replace(
            "</RegisterDescription>",
            "<Integer Name=\"GevSCPSPacketSize\"><pValue>X</pValue></Integer></RegisterDescription>",
        );
        let m = XmlMerger::new_with_endian(camera.as_bytes(), None, true).unwrap();
        let out = std::str::from_utf8(m.blob()).unwrap();
        assert_eq!(out.matches("Name=\"GevSCPSPacketSize\"").count(), 1);
        assert!(out.contains("Name=\"GevSCPHostPort\""));
        assert_eq!(
            m.node_count(),
            8,
            "5 built-in + the 3 missing transport nodes"
        );
    }

    #[test]
    fn missing_closing_tag_is_a_clean_error() {
        assert!(XmlMerger::new(b"<RegisterDescription>", None).is_err());
    }
}

#[cfg(test)]
mod endian_tests {
    use super::{rewrite_endian, rewrite_gige_identity};

    #[test]
    fn rewrites_declarations_in_both_directions() {
        let little = "<A><Endianess>LittleEndian</Endianess></A>";
        let big = rewrite_endian(little.to_owned(), true);
        assert_eq!(big, "<A><Endianess>BigEndian</Endianess></A>");
        let back = rewrite_endian(big, false);
        assert_eq!(back, little);
    }

    #[test]
    fn gige_identity_gets_a_distinct_cache_key() {
        let xml = concat!(
            "<String Name=\"DeviceModelName\" NameSpace=\"Standard\"><Value>Virtual</Value></String>",
            "<String Name=\"DeviceSerialNumber\" NameSpace=\"Standard\"><Value>NULL-0001</Value></String>",
        );
        let out = rewrite_gige_identity(xml.to_owned());
        assert!(out.contains("<Value>Virtual GigE</Value>"));
        assert!(out.contains("<Value>NULL-0001-GE</Value>"));
    }
}
