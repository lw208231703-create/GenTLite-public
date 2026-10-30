//! XML customization layer: inject a built-in SmartCamera category (the
//! 0x2000_0000 custom window as GenApi nodes) plus an optional
//! operator-supplied node fragment into the camera's GenICam XML, and
//! serve the result as the virtual device's XML blob. Fetching the raw
//! camera XML (First URL, `local:` blob read, transparent zip) is the
//! back adapter's job ([`crate::back::CameraBack::fetch_xml`]) — this type
//! starts from the raw bytes.

use gentl_core::{GentlError, GentlResult};

use crate::XML_BASE_V;

/// Injected device description served by the virtual device.
pub struct XmlMerger {
    blob: Vec<u8>,
    /// `local:vgige.xml;<XML_BASE_V hex>;<len hex>` — never longer than the
    /// 512-byte First URL slot.
    url: String,
    /// Feature nodes in the built-in SmartCamera fragment (ready-line stats).
    node_count: usize,
}

impl XmlMerger {
    /// Build the injected blob from the raw camera XML and the optional
    /// operator fragment (a file's content, inserted verbatim).
    pub fn new(camera_xml: &[u8], ext: Option<&[u8]>) -> GentlResult<Self> {
        let camera_xml = String::from_utf8(camera_xml.to_vec())
            .map_err(|_| GentlError::Io("camera XML is not UTF-8".into()))?;
        let (blob, node_count) = build_injected_xml(&camera_xml, ext)?;
        let url = format!("local:vgige.xml;{XML_BASE_V:x};{:x}", blob.len());
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
    let smart = smart_camera_fragment();
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
    Ok((out, node_count))
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
        // Virtual URL points at the blob.
        assert!(m.url().starts_with("local:vgige.xml;"));
        assert!(m.url().ends_with(&format!("{:x}", m.blob().len())));
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
    fn missing_closing_tag_is_a_clean_error() {
        assert!(XmlMerger::new(b"<RegisterDescription>", None).is_err());
    }
}
