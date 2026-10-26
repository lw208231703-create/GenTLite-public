//! USB descriptor construction (U3V 1.2 §3.2 / USB 2.0 §9.6): the
//! gadget layer feeds these blobs to FunctionFS ep0. Layout mirrors a
//! standard U3V camera (the Hikrobot class of devices): one IAD
//! grouping control + event + streaming interfaces, class-specific
//! Device Info descriptor on the control interface, one bulk IN per
//! interface family (§3.2 Tables 3-1/3-3/3-6/3-8).
//!
//! All builders are pure byte constructors — the byte-level tests lock
//! the §3.2 identity triples, and the Device Info round-trips through
//! gentl-u3v's own parser (the host-side view of the same descriptor).

use gentl_u3v::descriptors as d;

use crate::DeviceConfig;

/// Endpoint addresses and MPS of the gadget (QoI — dwc3 superSpeed,
/// 1024-byte bulk MPS; Q39).
pub const EP_CONTROL_OUT: u8 = 0x01;
pub const EP_CONTROL_IN: u8 = 0x81;
pub const EP_EVENT_IN: u8 = 0x82;
pub const EP_STREAM_IN: u8 = 0x83;
pub const BULK_MPS: u16 = 1024;

/// String table indexes (referenced by the Device Info descriptor).
pub const STR_MANUFACTURER: u8 = 1;
pub const STR_PRODUCT: u8 = 2;
pub const STR_SERIAL: u8 = 3;
pub const STR_GUID: u8 = 4;

/// The full descriptor set the gadget writes to FunctionFS.
pub struct DescriptorSet {
    /// USB device descriptor (18 bytes).
    pub device: Vec<u8>,
    /// Configuration descriptor blob (config + IAD + interfaces +
    /// endpoints + the U3V Device Info CS descriptor).
    pub config: Vec<u8>,
    /// FunctionFS strings record (language + string descriptors).
    pub strings: Vec<u8>,
}

fn le16(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

impl DescriptorSet {
    pub fn build(config: &DeviceConfig, vid: u16, pid: u16) -> Self {
        Self {
            device: device_descriptor(vid, pid),
            config: config_descriptor(),
            strings: strings_record(config),
        }
    }
}

/// USB device descriptor (USB 2.0 §9.6.1): the composite-IF U3V shape —
/// bDeviceClass 0xEF (misc), bDeviceSubClass 0x02 (U3V), protocol 0x01
/// (IAD present), bcdUSB 3.2 for the superSpeed gadget (Table 3-1).
fn device_descriptor(vid: u16, pid: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(18);
    b.push(18); // bLength
    b.push(0x01); // bDescriptorType DEVICE
    b.extend_from_slice(&le16(0x0320)); // bcdUSB 3.2
    b.push(d::USB_CLASS_MISC); // 0xEF
    b.push(d::USB_DEVICE_SUBCLASS_U3V); // 0x02
    b.push(0x01); // bDeviceProtocol: IAD present
    b.push(9); // bMaxPacketSize0 (SS: 2^9)
    b.extend_from_slice(&le16(vid));
    b.extend_from_slice(&le16(pid));
    b.extend_from_slice(&le16(0x0100)); // bcdDevice 1.0
    b.push(STR_MANUFACTURER);
    b.push(STR_PRODUCT);
    b.push(STR_SERIAL);
    b.push(1); // bNumConfigurations
    b
}

/// Configuration blob: config(9) + IAD(8) + control IF(9) + ep OUT(7) +
/// ep IN(7) + devinfo CS(20) + event IF(9) + ep IN(7) + streaming IF(9)
/// + ep IN(7). The same shape the gentl-u3v descriptor parser's tests
/// consume, so host and device agree byte-for-byte on the layout.
fn config_descriptor() -> Vec<u8> {
    let mut b = Vec::new();
    let total_len_pos = b.len() + 2;
    // Configuration header (USB 2.0 §9.6.3).
    b.extend_from_slice(&[9, d::DESC_CONFIGURATION, 0, 0, 1, 0, 0, 0, 0xE0]);

    // Interface Association Descriptor (USB 2.0 §9.6.4): control +
    // event + streaming under one U3V function (Tables 3-3/3-6/3-8).
    b.extend_from_slice(&[
        8,
        d::DESC_INTERFACE_ASSOCIATION,
        0, // bFirstInterface
        3, // bInterfaceCount
        d::USB_CLASS_MISC,
        d::USB_INTERFACE_SUBCLASS_U3V,
        d::USB_PROTOCOL_CONTROL,
        0, // iFunction
    ]);

    // Control interface (Table 3-3): EF/05/00, two bulk endpoints.
    b.extend_from_slice(&[9, d::DESC_INTERFACE, 0, 0, 2, d::USB_CLASS_MISC, d::USB_INTERFACE_SUBCLASS_U3V, d::USB_PROTOCOL_CONTROL, 0]);
    b.extend_from_slice(&endpoint_descriptor(EP_CONTROL_OUT, false));
    b.extend_from_slice(&endpoint_descriptor(EP_CONTROL_IN, true));
    // U3V class-specific Device Info (§3.2.5 Table 3-5).
    b.extend_from_slice(&device_info_cs());

    // Event interface (Table 3-6): EF/05/01, one bulk IN. The gateway
    // emits no events in v1; the endpoint exists so the function shape
    // matches a standard camera (UQ7 follow-up would serve it).
    b.extend_from_slice(&[9, d::DESC_INTERFACE, 1, 0, 1, d::USB_CLASS_MISC, d::USB_INTERFACE_SUBCLASS_U3V, d::USB_PROTOCOL_EVENT, 0]);
    b.extend_from_slice(&endpoint_descriptor(EP_EVENT_IN, true));

    // Streaming interface (Table 3-8): EF/05/02, one bulk IN.
    b.extend_from_slice(&[9, d::DESC_INTERFACE, 2, 0, 1, d::USB_CLASS_MISC, d::USB_INTERFACE_SUBCLASS_U3V, d::USB_PROTOCOL_STREAMING, 0]);
    b.extend_from_slice(&endpoint_descriptor(EP_STREAM_IN, true));

    let total = b.len() as u16;
    b[total_len_pos] = total as u8;
    b[total_len_pos + 1] = (total >> 8) as u8;
    b
}

/// Bulk endpoint descriptor (USB 2.0 §9.6.6), superSpeed MPS.
fn endpoint_descriptor(address: u8, _in: bool) -> [u8; 7] {
    [
        7,
        0x05, // ENDPOINT
        address,
        0x02, // bmAttributes: bulk
        BULK_MPS as u8,
        (BULK_MPS >> 8) as u8,
        0, // bInterval: N/A for bulk
    ]
}

/// U3V Device Info class-specific descriptor (§3.2.5 Table 3-5):
/// GenCP/U3V versions + the string indexes + speed mask.
fn device_info_cs() -> [u8; d::U3V_DEVICEINFO_LEN] {
    let mut b = [0u8; d::U3V_DEVICEINFO_LEN];
    b[0] = d::U3V_DEVICEINFO_LEN as u8;
    b[1] = d::DESC_CS_INTERFACE;
    b[2] = d::U3V_DEVICEINFO_SUBTYPE;
    b[3..7].copy_from_slice(&0x0001_0003u32.to_le_bytes()); // GenCP 1.3
    b[7..11].copy_from_slice(&0x0001_0002u32.to_le_bytes()); // U3V 1.2
    b[11] = STR_GUID;
    b[12] = STR_MANUFACTURER;
    b[13] = STR_PRODUCT;
    b[14] = 0; // family name: none
    b[15] = 0; // device version: none
    b[16] = 0; // manufacturer info: none
    b[17] = STR_SERIAL;
    b[18] = 0; // user-defined name: none
    b[19] = 0x08; // bmSpeedSupport: SuperSpeed
    b
}

/// FunctionFS strings record: language id header + UTF-16LE string
/// descriptors (USB 2.0 §9.6.7; FunctionFS appends them after the
/// descriptor blob).
fn strings_record(config: &DeviceConfig) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[4, 0x03, 0x09, 0x04]); // LANGID en-US
    let push_str = |b: &mut Vec<u8>, index: u8, s: &str| {
        let _ = index;
        let utf16: Vec<u16> = s.encode_utf16().collect();
        let len = 2 + utf16.len() * 2;
        b.push(len as u8);
        b.push(0x03);
        for w in utf16 {
            b.extend_from_slice(&w.to_le_bytes());
        }
    };
    push_str(&mut b, STR_MANUFACTURER, &config.manufacturer);
    push_str(&mut b, STR_PRODUCT, &config.model);
    push_str(&mut b, STR_SERIAL, &config.serial_number);
    push_str(&mut b, STR_GUID, &guid_string(&config.guid));
    b
}

/// The canonical 8-4-4-4-12 GUID text form (R-1cd's string view).
pub fn guid_string(guid: &[u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        guid[0], guid[1], guid[2], guid[3], guid[4], guid[5], guid[6], guid[7],
        guid[8], guid[9], guid[10], guid[11], guid[12], guid[13], guid[14], guid[15],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §3.2 identity triples, byte-level: device EF/02/IAD-present, the
    /// three interfaces EF/05/{00,01,02}, endpoint addresses and MPS.
    #[test]
    fn descriptor_identity_triples() {
        let set = DescriptorSet::build(
            &DeviceConfig {
                manufacturer: "M".into(),
                model: "P".into(),
                device_version: "1".into(),
                serial_number: "S".into(),
                xml: Vec::new(),
                payload_size: 0,
                guid: [0xAB; 16],
            },
            0x1234,
            0x0001,
        );
        // Device descriptor (USB 2.0 §9.6.1 field offsets).
        assert_eq!(set.device.len(), 18);
        assert_eq!(set.device[4], 0xEF);
        assert_eq!(set.device[5], 0x02);
        assert_eq!(set.device[6], 0x01);
        assert_eq!(&set.device[8..12], &[0x34, 0x12, 0x01, 0x00]);

        // Host-side parse of OUR config blob: the same parser a real
        // host (gentl-u3v producer) runs over this wire shape.
        let cfg = gentl_u3v::descriptors::parse_config_descriptor(&set.config).expect("parse");
        assert_eq!(cfg.iads.len(), 1);
        assert_eq!(cfg.interfaces.len(), 3);
        for (i, proto) in [d::USB_PROTOCOL_CONTROL, d::USB_PROTOCOL_EVENT, d::USB_PROTOCOL_STREAMING]
            .iter()
            .enumerate()
        {
            assert_eq!(cfg.interfaces[i].class, 0xEF);
            assert_eq!(cfg.interfaces[i].sub_class, 0x05);
            assert_eq!(cfg.interfaces[i].protocol, *proto);
        }
        // Endpoint descriptors carry our MPS and addresses.
        for needle in [EP_CONTROL_OUT, EP_CONTROL_IN, EP_EVENT_IN, EP_STREAM_IN] {
            let pos = set
                .config
                .windows(7)
                .position(|w| w[1] == 0x05 && w[2] == needle)
                .unwrap_or_else(|| panic!("endpoint 0x{needle:02x}"));
            assert_eq!(&set.config[pos + 4..pos + 6], &BULK_MPS.to_le_bytes());
        }
    }

    /// The class-specific Device Info descriptor round-trips through
    /// gentl-u3v's own parser (host view of the same bytes).
    #[test]
    fn device_info_roundtrips_through_host_parser() {
        let cfg = config_descriptor();
        let info = d::device_info_descriptor(&cfg).expect("devinfo");
        assert_eq!(info.gen_cp_version, 0x0001_0003);
        assert_eq!(info.u3v_version, 0x0001_0002);
        assert_eq!(info.i_device_guid, STR_GUID);
        assert_eq!(info.i_vendor_name, STR_MANUFACTURER);
        assert_eq!(info.i_model_name, STR_PRODUCT);
        assert_eq!(info.i_serial_number, STR_SERIAL);
    }

    #[test]
    fn guid_text_form() {
        let g: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D,
            0x0E, 0x0F, 0x10,
        ];
        assert_eq!(guid_string(&g), "01020304-0506-0708-090A-0B0C0D0E0F10");
    }

    #[test]
    fn synthesize_guid_is_stable_and_valid() {
        let a = crate::synthesize_guid("FU3V-0001");
        let b = crate::synthesize_guid("FU3V-0001");
        let c = crate::synthesize_guid("FU3V-0002");
        assert_eq!(a, b, "deterministic");
        assert_ne!(a, c);
        assert_eq!(a[6] >> 4, 5, "version nibble");
        assert_eq!(a[8] >> 6, 0b10, "RFC 4122 variant");
    }
}
