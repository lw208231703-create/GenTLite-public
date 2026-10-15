//! Minimal GenICam device description (GenApi Schema 1.1) for the sim.
//!
//! Conventions follow the SFNC 2.7 reference XML and the GenApi Schema 1.1
//! XSD (docs/GenICam_Package_2025.10): node element order matches the XSD
//! sequences, register-backed features go through `*Reg` helper nodes in the
//! Custom namespace referenced by `<pValue>`/`<pPort>`, and the sim's user
//! register area addresses come from `crate::regmap`.

use std::fmt::Write as _;

use gentl_gvcp::registers as reg;

use crate::regmap::{REG_EXPOSURE_TIME, REG_HEIGHT, REG_PIXEL_FORMAT, REG_WIDTH};

// Placeholder-style GUIDs in the same spirit as the SFNC reference file;
// they only need to be syntactically valid GUID_t values.
const PRODUCT_GUID: &str = "5a1f3c8e-2b7d-4e9a-9c4d-1e2f3a4b5c6d";
const VERSION_GUID: &str = "6b2e4d9f-3c8a-5f1b-8d5e-2f3a4b5c6d7e";

/// Render the sim's GenICam XML. `width`/`height` are the stream
/// dimensions, published as the Width/Height feature maxima so the XML
/// always agrees with the emitted fake stream.
pub(crate) fn build_xml(width: u32, height: u32) -> String {
    let mut x = String::with_capacity(6 * 1024);
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        x,
        "<RegisterDescription xmlns=\"http://www.genicam.org/GenApi/Version_1_1\" \
xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
xsi:schemaLocation=\"http://www.genicam.org/GenApi/Version_1_1 \
http://www.genicam.org/GenApi/GenApiSchema_Version_1_1.xsd\" \
ModelName=\"SimCam\" VendorName=\"GenTLite\" \
ToolTip=\"GenTLite software GigE Vision camera\" StandardNameSpace=\"None\" \
SchemaMajorVersion=\"1\" SchemaMinorVersion=\"1\" SchemaSubMinorVersion=\"0\" \
MajorVersion=\"1\" MinorVersion=\"0\" SubMinorVersion=\"0\" \
ProductGuid=\"{PRODUCT_GUID}\" VersionGuid=\"{VERSION_GUID}\">"
    );

    x.push_str(
        "  <Group Comment=\"Categories\">\n\
         \x20   <Category Name=\"Root\" NameSpace=\"Standard\">\n\
         \x20     <ToolTip>Provides the Root of the GenICam features tree.</ToolTip>\n\
         \x20     <Description>Provides the Root of the GenICam features tree.</Description>\n\
         \x20     <DisplayName>Root</DisplayName>\n\
         \x20     <Visibility>Beginner</Visibility>\n\
         \x20     <ImposedAccessMode>RO</ImposedAccessMode>\n\
         \x20     <pFeature>DeviceControl</pFeature>\n\
         \x20     <pFeature>ImageFormatControl</pFeature>\n\
         \x20     <pFeature>AcquisitionControl</pFeature>\n\
         \x20   </Category>\n\
         \x20   <Category Name=\"DeviceControl\" NameSpace=\"Standard\">\n\
         \x20     <ToolTip>Device information and control.</ToolTip>\n\
         \x20     <Description>Device information and control.</Description>\n\
         \x20     <DisplayName>Device Control</DisplayName>\n\
         \x20     <Visibility>Beginner</Visibility>\n\
         \x20     <ImposedAccessMode>RO</ImposedAccessMode>\n\
         \x20     <pFeature>DeviceVendorName</pFeature>\n\
         \x20     <pFeature>DeviceModelName</pFeature>\n\
         \x20     <pFeature>DeviceVersion</pFeature>\n\
         \x20     <pFeature>DeviceSerialNumber</pFeature>\n\
         \x20     <pFeature>DeviceUserID</pFeature>\n\
         \x20   </Category>\n\
         \x20   <Category Name=\"ImageFormatControl\" NameSpace=\"Standard\">\n\
         \x20     <ToolTip>Image format control.</ToolTip>\n\
         \x20     <Description>Image format control.</Description>\n\
         \x20     <DisplayName>Image Format Control</DisplayName>\n\
         \x20     <Visibility>Beginner</Visibility>\n\
         \x20     <ImposedAccessMode>RO</ImposedAccessMode>\n\
         \x20     <pFeature>Width</pFeature>\n\
         \x20     <pFeature>Height</pFeature>\n\
         \x20     <pFeature>PixelFormat</pFeature>\n\
         \x20   </Category>\n\
         \x20   <Category Name=\"AcquisitionControl\" NameSpace=\"Standard\">\n\
         \x20     <ToolTip>Acquisition control.</ToolTip>\n\
         \x20     <Description>Acquisition control.</Description>\n\
         \x20     <DisplayName>Acquisition Control</DisplayName>\n\
         \x20     <Visibility>Beginner</Visibility>\n\
         \x20     <ImposedAccessMode>RO</ImposedAccessMode>\n\
         \x20     <pFeature>ExposureTime</pFeature>\n\
         \x20   </Category>\n\
         \x20 </Group>\n",
    );

    x.push_str(
        "  <Group Comment=\"Port\">\n\
         \x20   <Port Name=\"Device\" NameSpace=\"Standard\">\n\
         \x20     <ToolTip>GenTL Port module of this device.</ToolTip>\n\
         \x20     <Description>Register port through which the GenTL consumer accesses the device.</Description>\n\
         \x20     <DisplayName>Device Port</DisplayName>\n\
         \x20     <Visibility>Invisible</Visibility>\n\
         \x20   </Port>\n\
         \x20 </Group>\n",
    );

    // Identity strings, backed by the bootstrap string registers.
    string_reg(&mut x, "DeviceVendorName", "Device Vendor Name", "Name of the manufacturer of the device.", reg::MANUFACTURER_NAME, 32, "RO");
    string_reg(&mut x, "DeviceModelName", "Device Model Name", "Model of the device.", reg::MODEL_NAME, 32, "RO");
    string_reg(&mut x, "DeviceVersion", "Device Version", "Version of the device.", reg::DEVICE_VERSION, 32, "RO");
    string_reg(&mut x, "DeviceSerialNumber", "Device Serial Number", "Unique serial number of the device.", reg::SERIAL_NUMBER, 16, "RO");
    string_reg(&mut x, "DeviceUserID", "Device User ID", "User-programmable device identifier.", reg::USER_DEFINED_NAME, 16, "RW");

    x.push_str("  <Group Comment=\"ImageFormat\">\n");
    int_feature(&mut x, "Width", "Width", "Width of the image provided by the device (in pixels).", "WidthReg", 1, width);
    int_reg(&mut x, "WidthReg", REG_WIDTH);
    int_feature(&mut x, "Height", "Height", "Height of the image provided by the device (in pixels).", "HeightReg", 1, height);
    int_reg(&mut x, "HeightReg", REG_HEIGHT);

    x.push_str(
        "    <Enumeration Name=\"PixelFormat\" NameSpace=\"Standard\">\n\
         \x20     <ToolTip>Format of the pixels provided by the device.</ToolTip>\n\
         \x20     <Description>Format of the pixels provided by the device.</Description>\n\
         \x20     <DisplayName>Pixel Format</DisplayName>\n\
         \x20     <Visibility>Beginner</Visibility>\n",
    );
    for (name, value) in [
        ("Mono8", 0x0108_0001u32),
        ("Mono10", 0x0110_0003),
        ("Mono12", 0x0110_0005),
        ("Mono16", 0x0110_0007),
    ] {
        let _ = writeln!(
            x,
            "      <EnumEntry Name=\"{name}\" NameSpace=\"Standard\">\n\
             \x20       <DisplayName>{name}</DisplayName>\n\
             \x20       <Value>0x{value:08X}</Value>\n\
             \x20     </EnumEntry>"
        );
    }
    x.push_str(
        "      <pValue>PixelFormatReg</pValue>\n\
         \x20   </Enumeration>\n",
    );
    int_reg(&mut x, "PixelFormatReg", REG_PIXEL_FORMAT);
    x.push_str("  </Group>\n");

    let _ = writeln!(
        x,
        "  <Group Comment=\"Acquisition\">\n\
         \x20   <Float Name=\"ExposureTime\" NameSpace=\"Standard\">\n\
         \x20     <ToolTip>Sets the exposure time in microseconds.</ToolTip>\n\
         \x20     <Description>Sets the exposure time in microseconds.</Description>\n\
         \x20     <DisplayName>Exposure Time</DisplayName>\n\
         \x20     <Visibility>Beginner</Visibility>\n\
         \x20     <pValue>ExposureTimeReg</pValue>\n\
         \x20     <Min>10.0</Min>\n\
         \x20     <Max>100000.0</Max>\n\
         \x20     <Unit>us</Unit>\n\
         \x20   </Float>\n\
         \x20   <FloatReg Name=\"ExposureTimeReg\" NameSpace=\"Custom\">\n\
         \x20     <Address>0x{REG_EXPOSURE_TIME:08X}</Address>\n\
         \x20     <Length>4</Length>\n\
         \x20     <AccessMode>RW</AccessMode>\n\
         \x20     <pPort>Device</pPort>\n\
         \x20     <Endianess>BigEndian</Endianess>\n\
         \x20     <Unit>us</Unit>\n\
         \x20   </FloatReg>\n\
         \x20 </Group>"
    );

    x.push_str("</RegisterDescription>\n");
    x
}

fn string_reg(x: &mut String, name: &str, display: &str, tooltip: &str, addr: u32, len: u32, access: &str) {
    let _ = writeln!(
        x,
        "  <Group Comment=\"{name}\">\n\
         \x20   <StringReg Name=\"{name}\" NameSpace=\"Standard\">\n\
         \x20     <ToolTip>{tooltip}</ToolTip>\n\
         \x20     <Description>{tooltip}</Description>\n\
         \x20     <DisplayName>{display}</DisplayName>\n\
         \x20     <Visibility>Beginner</Visibility>\n\
         \x20     <Address>0x{addr:08X}</Address>\n\
         \x20     <Length>{len}</Length>\n\
         \x20     <AccessMode>{access}</AccessMode>\n\
         \x20     <pPort>Device</pPort>\n\
         \x20   </StringReg>\n\
         \x20 </Group>"
    );
}

fn int_feature(x: &mut String, name: &str, display: &str, tooltip: &str, pvalue: &str, min: u32, max: u32) {
    let _ = writeln!(
        x,
        "    <Integer Name=\"{name}\" NameSpace=\"Standard\">\n\
         \x20     <ToolTip>{tooltip}</ToolTip>\n\
         \x20     <Description>{tooltip}</Description>\n\
         \x20     <DisplayName>{display}</DisplayName>\n\
         \x20     <Visibility>Beginner</Visibility>\n\
         \x20     <pValue>{pvalue}</pValue>\n\
         \x20     <Min>{min}</Min>\n\
         \x20     <Max>{max}</Max>\n\
         \x20   </Integer>"
    );
}

fn int_reg(x: &mut String, name: &str, addr: u32) {
    let _ = writeln!(
        x,
        "    <IntReg Name=\"{name}\" NameSpace=\"Custom\">\n\
         \x20     <Address>0x{addr:08X}</Address>\n\
         \x20     <Length>4</Length>\n\
         \x20     <AccessMode>RW</AccessMode>\n\
         \x20     <pPort>Device</pPort>\n\
         \x20     <Sign>Unsigned</Sign>\n\
         \x20     <Endianess>BigEndian</Endianess>\n\
         \x20   </IntReg>"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_is_schema_1_1_register_description() {
        let x = build_xml(1024, 768);
        assert!(x.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(x.contains("xmlns=\"http://www.genicam.org/GenApi/Version_1_1\""));
        assert!(x.contains("SchemaMajorVersion=\"1\" SchemaMinorVersion=\"1\""));
        assert!(x.contains("StandardNameSpace=\"None\""));
        assert!(x.trim_end().ends_with("</RegisterDescription>"));
    }

    #[test]
    fn xml_references_user_registers_and_port() {
        let x = build_xml(1024, 768);
        // Register-backed helpers point at the sim's user register area.
        for addr in [REG_WIDTH, REG_HEIGHT, REG_PIXEL_FORMAT, REG_EXPOSURE_TIME] {
            assert!(
                x.contains(&format!("Address>0x{addr:08X}</Address>")),
                "missing address 0x{addr:08X}"
            );
        }
        assert!(x.contains("<pPort>Device</pPort>"));
        assert!(x.contains("<Port Name=\"Device\" NameSpace=\"Standard\">"));
        // Feature surface: 4 EnumEntries, the min/max constraints, identity
        // StringRegs bound to the bootstrap string slots.
        assert_eq!(x.matches("<EnumEntry ").count(), 4);
        assert!(x.contains("<Min>10.0</Min>") && x.contains("<Max>100000.0</Max>"));
        assert!(x.contains("<StringReg Name=\"DeviceVendorName\""));
        assert!(x.contains(&format!(
            "Address>0x{:08X}</Address>",
            reg::MANUFACTURER_NAME
        )));
    }

    #[test]
    fn xml_feature_maxima_match_stream_dimensions() {
        let x = build_xml(640, 480);
        assert!(x.contains("<Max>640</Max>"));
        assert!(x.contains("<Max>480</Max>"));
        assert!(!x.contains("<Max>1024</Max>"));
    }
}
