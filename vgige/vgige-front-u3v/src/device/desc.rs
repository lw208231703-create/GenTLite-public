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

/// Endpoint addresses of the gadget (fixed hints; FunctionFS autoconfig
/// may renumber, dwc3 has 15 endpoints to pick from).
pub const EP_CONTROL_OUT: u8 = 0x01;
pub const EP_CONTROL_IN: u8 = 0x81;
pub const EP_EVENT_IN: u8 = 0x82;
pub const EP_STREAM_IN: u8 = 0x83;
/// Bulk MPS per speed (USB 2.0 §9.6.6 wMaxPacketSize limits; USB 3.2
/// §9.6.6 for the super-speed profile). The bring-up gadget runs
/// high-speed (dwc3 with the usb2 phy only — Q39), so the host-facing
/// descriptors carry the HS values; the SS set lands with the
/// super-speed front-end.
pub const FS_BULK_MPS: u16 = 64;
pub const HS_BULK_MPS: u16 = 512;
pub const BULK_MPS: u16 = 1024;

/// String table indexes (referenced by the Device Info descriptor).
pub const STR_MANUFACTURER: u8 = 1;
pub const STR_PRODUCT: u8 = 2;
pub const STR_SERIAL: u8 = 3;
pub const STR_GUID: u8 = 4;

/// The full descriptor set the gadget serves over ep0 / writes to
/// FunctionFS.
pub struct DescriptorSet {
    /// USB device descriptor (18 bytes) for GET_DESCRIPTOR(DEVICE) —
    /// high-speed shape (USB 2.0 §9.6.1: bcdUSB 2.1, MPS0 64). The U3V
    /// spec Table 3-1 is the super-speed profile; the SS device
    /// descriptor lands with the SS descriptor set (Q39).
    pub device: Vec<u8>,
    /// Configuration blob (config + IAD + interfaces + endpoints + the
    /// U3V Device Info CS descriptor) for GET_DESCRIPTOR(CONFIGURATION)
    /// — the high-speed shape (512-byte bulk MPS).
    pub config: Vec<u8>,
    /// FunctionFS full-speed interface set (interfaces + endpoints, bulk
    /// MPS 64): NO config header, NO IAD — f_fs.c's grammar takes
    /// interface-level descriptors only (the composite layer composes
    /// the config header and the IAD), and NO class-specific
    /// descriptors — f_fs.c rejects CS_INTERFACE (0x24) for classes it
    /// does not know (only HID/CCID/DFU are whitelisted), so the U3V
    /// Device Info CS descriptor is served via the ep0
    /// GET_DESCRIPTOR(CONFIGURATION) answer instead (our `config`
    /// blob still carries it).
    pub ffs_fs: Vec<u8>,
    /// FunctionFS high-speed interface set (bulk MPS 512); endpoint
    /// addresses must match `ffs_fs` (f_fs.c requires identical
    /// addresses across speed sets).
    pub ffs_hs: Vec<u8>,
    /// FunctionFS super-speed interface set: interfaces + 1024-MPS
    /// endpoints, each SS endpoint descriptor followed by its 6-byte
    /// companion (USB 3.2 §9.6.7). No CS descriptors (f_fs.c whitelist).
    pub ffs_ss: Vec<u8>,
    /// Super-speed device descriptor for GET_DESCRIPTOR(DEVICE) when the
    /// link negotiated SuperSpeed (USB 3.2 §9.6.1: bcdUSB 3.2, MPS0 2^9).
    pub device_ss: Vec<u8>,
    /// Super-speed configuration blob (same shape as `config`, 1024-MPS
    /// endpoints with companions) for GET_DESCRIPTOR(CONFIGURATION) at
    /// SuperSpeed.
    pub config_ss: Vec<u8>,
    /// BOS (Binary Device Object Store, USB 3.2 §9.6.2) with a USB 2.0
    /// Extension capability and a SuperSpeed USB Device Capability —
    /// required for a host to keep a SuperSpeed link (answered for
    /// GET_DESCRIPTOR(BOS); bcdUSB 2.1 in the HS device descriptor makes
    /// hosts ask).
    pub bos: Vec<u8>,
    /// FunctionFS strings record payload (v2 format, uapi functionfs.h:
    /// str_count/lang_count then per-language NUL-terminated UTF-8
    /// strings) — for the kernel; the host reads the ep0 answers.
    pub ffs_strings: Vec<u8>,
    /// USB string descriptors (language id + UTF-16LE descriptors) for
    /// the ep0 GET_DESCRIPTOR(STRING) answer.
    pub usb_strings: Vec<u8>,
}

fn le16(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

impl DescriptorSet {
    pub fn build(config: &DeviceConfig, vid: u16, pid: u16) -> Self {
        Self {
            device: device_descriptor(vid, pid),
            config: config_descriptor(),
            ffs_fs: interfaces_and_eps(FS_BULK_MPS, false),
            ffs_hs: interfaces_and_eps(HS_BULK_MPS, false),
            ffs_ss: interfaces_and_eps_ss(false),
            device_ss: device_descriptor_ss(vid, pid),
            config_ss: config_descriptor_ss(),
            bos: bos_descriptor(),
            ffs_strings: ffs_strings_record(config),
            usb_strings: usb_strings_record(config),
        }
    }
}

/// USB device descriptor (USB 2.0 §9.6.1): the composite-IF U3V shape —
/// bDeviceClass 0xEF (misc), bDeviceSubClass 0x02 (U3V), protocol 0x01
/// (IAD present). High-speed bring-up profile: bcdUSB 2.1, MPS0 64
/// (Table 3-1's bcdUSB 3.2 / MPS0 2^9 is the super-speed profile — Q39).
fn device_descriptor(vid: u16, pid: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(18);
    b.push(18); // bLength
    b.push(0x01); // bDescriptorType DEVICE
    b.extend_from_slice(&le16(0x0210)); // bcdUSB 2.1
    b.push(d::USB_CLASS_MISC); // 0xEF
    b.push(d::USB_DEVICE_SUBCLASS_U3V); // 0x02
    b.push(0x01); // bDeviceProtocol: IAD present
    b.push(64); // bMaxPacketSize0 (high-speed)
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
/// + ep IN(7) — the high-speed shape. The same shape the gentl-u3v
/// descriptor parser's tests consume, so host and device agree
/// byte-for-byte on the layout.
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

    b.extend_from_slice(&interfaces_and_eps(HS_BULK_MPS, true));

    let total = b.len() as u16;
    b[total_len_pos] = total as u8;
    b[total_len_pos + 1] = (total >> 8) as u8;
    b
}

/// Interface-level descriptors shared by the host-facing config blob and
/// the FunctionFS speed sets: control IF + ep OUT + ep IN [+ devinfo CS]
/// + event IF + ep IN + streaming IF + ep IN. `mps` is the bulk
/// wMaxPacketSize for the speed (64 full / 512 high / 1024 super);
/// `with_devinfo` adds the U3V class-specific Device Info descriptor —
/// host-facing descriptors only, f_fs.c rejects it in the record.
fn interfaces_and_eps(mps: u16, with_devinfo: bool) -> Vec<u8> {
    let mut b = Vec::new();

    // Control interface (Table 3-3): EF/05/00, two bulk endpoints.
    b.extend_from_slice(&[9, d::DESC_INTERFACE, 0, 0, 2, d::USB_CLASS_MISC, d::USB_INTERFACE_SUBCLASS_U3V, d::USB_PROTOCOL_CONTROL, 0]);
    b.extend_from_slice(&endpoint_descriptor(EP_CONTROL_OUT, mps));
    b.extend_from_slice(&endpoint_descriptor(EP_CONTROL_IN, mps));
    if with_devinfo {
        // U3V class-specific Device Info (§3.2.5 Table 3-5).
        b.extend_from_slice(&device_info_cs());
    }

    // Event interface (Table 3-6): EF/05/01, one bulk IN. The gateway
    // emits no events in v1; the endpoint exists so the function shape
    // matches a standard camera (UQ7 follow-up would serve it).
    b.extend_from_slice(&[9, d::DESC_INTERFACE, 1, 0, 1, d::USB_CLASS_MISC, d::USB_INTERFACE_SUBCLASS_U3V, d::USB_PROTOCOL_EVENT, 0]);
    b.extend_from_slice(&endpoint_descriptor(EP_EVENT_IN, mps));

    // Streaming interface (Table 3-8): EF/05/02, one bulk IN.
    b.extend_from_slice(&[9, d::DESC_INTERFACE, 2, 0, 1, d::USB_CLASS_MISC, d::USB_INTERFACE_SUBCLASS_U3V, d::USB_PROTOCOL_STREAMING, 0]);
    b.extend_from_slice(&endpoint_descriptor(EP_STREAM_IN, mps));

    b
}

/// Bulk endpoint descriptor (USB 2.0 §9.6.6), per-speed MPS.
fn endpoint_descriptor(address: u8, mps: u16) -> [u8; 7] {
    [
        7,
        0x05, // ENDPOINT
        address,
        0x02, // bmAttributes: bulk
        mps as u8,
        (mps >> 8) as u8,
        0, // bInterval: N/A for bulk
    ]
}

/// Super-speed variant of [`interfaces_and_eps`]: 1024-byte bulk MPS and
/// a 6-byte SS endpoint companion descriptor after every endpoint
/// (USB 3.2 §9.6.7 — mandatory in SS configuration descriptor chains).
fn interfaces_and_eps_ss(with_devinfo: bool) -> Vec<u8> {
    let mut b = Vec::new();

    b.extend_from_slice(&[9, d::DESC_INTERFACE, 0, 0, 2, d::USB_CLASS_MISC, d::USB_INTERFACE_SUBCLASS_U3V, d::USB_PROTOCOL_CONTROL, 0]);
    b.extend_from_slice(&endpoint_descriptor(EP_CONTROL_OUT, BULK_MPS));
    b.extend_from_slice(&ss_endpoint_companion());
    b.extend_from_slice(&endpoint_descriptor(EP_CONTROL_IN, BULK_MPS));
    b.extend_from_slice(&ss_endpoint_companion());
    if with_devinfo {
        b.extend_from_slice(&device_info_cs());
    }

    b.extend_from_slice(&[9, d::DESC_INTERFACE, 1, 0, 1, d::USB_CLASS_MISC, d::USB_INTERFACE_SUBCLASS_U3V, d::USB_PROTOCOL_EVENT, 0]);
    b.extend_from_slice(&endpoint_descriptor(EP_EVENT_IN, BULK_MPS));
    b.extend_from_slice(&ss_endpoint_companion());

    b.extend_from_slice(&[9, d::DESC_INTERFACE, 2, 0, 1, d::USB_CLASS_MISC, d::USB_INTERFACE_SUBCLASS_U3V, d::USB_PROTOCOL_STREAMING, 0]);
    b.extend_from_slice(&endpoint_descriptor(EP_STREAM_IN, BULK_MPS));
    b.extend_from_slice(&ss_endpoint_companion());

    b
}

/// SS endpoint companion descriptor (USB 3.2 §9.6.7): no bursting
/// (bMaxBurst 0), bulk (no attributes), no periodic bytes.
fn ss_endpoint_companion() -> [u8; 6] {
    [6, 0x30, 0, 0, 0, 0]
}

/// Super-speed device descriptor (USB 3.2 §9.6.1, Table 3-1's profile):
/// bcdUSB 3.2, bMaxPacketSize0 = 2^9 (512).
fn device_descriptor_ss(vid: u16, pid: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(18);
    b.push(18);
    b.push(0x01);
    b.extend_from_slice(&le16(0x0320)); // bcdUSB 3.2
    b.push(d::USB_CLASS_MISC);
    b.push(d::USB_DEVICE_SUBCLASS_U3V);
    b.push(0x01);
    b.push(9); // bMaxPacketSize0 (2^9, super-speed)
    b.extend_from_slice(&le16(vid));
    b.extend_from_slice(&le16(pid));
    b.extend_from_slice(&le16(0x0100));
    b.push(STR_MANUFACTURER);
    b.push(STR_PRODUCT);
    b.push(STR_SERIAL);
    b.push(1);
    b
}

/// Super-speed configuration blob: same shape as [`config_descriptor`]
/// with the SS interface set (1024-MPS endpoints + companions).
fn config_descriptor_ss() -> Vec<u8> {
    let mut b = Vec::new();
    let total_len_pos = b.len() + 2;
    b.extend_from_slice(&[9, d::DESC_CONFIGURATION, 0, 0, 1, 0, 0, 0, 0xE0]);
    b.extend_from_slice(&[
        8,
        d::DESC_INTERFACE_ASSOCIATION,
        0,
        3,
        d::USB_CLASS_MISC,
        d::USB_INTERFACE_SUBCLASS_U3V,
        d::USB_PROTOCOL_CONTROL,
        0,
    ]);
    b.extend_from_slice(&interfaces_and_eps_ss(true));
    let total = b.len() as u16;
    b[total_len_pos] = total as u8;
    b[total_len_pos + 1] = (total >> 8) as u8;
    b
}

/// BOS (USB 3.2 §9.6.2): USB 2.0 Extension capability (LPM) +
/// SuperSpeed USB Device Capability (LS/FS/HS/SS capable, functional at
/// SS, conservative U1/U2 exit latencies).
fn bos_descriptor() -> Vec<u8> {
    let mut b = Vec::new();
    // BOS header.
    b.extend_from_slice(&[5, 0x0F, 0, 0, 2]); // wTotalLength patched below
    // USB 2.0 Extension capability.
    b.extend_from_slice(&[7, 0x10, 0x02]);
    b.extend_from_slice(&0x02u32.to_le_bytes()); // bmAttributes: LPM
    // SuperSpeed USB Device Capability.
    b.extend_from_slice(&[10, 0x10, 0x03]);
    b.push(0x00); // bmAttributes (no LTM)
    b.extend_from_slice(&0x000Fu16.to_le_bytes()); // LS|FS|HS|SS
    b.push(0x02); // bFunctionalitySupport: SS
    b.push(0x0A); // bU1DevExitLat
    b.extend_from_slice(&0x01F4u16.to_le_bytes()); // wU2DevExitLat
    let total = b.len() as u16;
    b[2] = total as u8;
    b[3] = (total >> 8) as u8;
    b
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

/// The four device strings in STR_* index order (1-based; 0 is the
/// language list).
fn device_strings(config: &DeviceConfig) -> [String; 4] {
    [
        config.manufacturer.clone(),
        config.model.clone(),
        config.serial_number.clone(),
        guid_string(&config.guid),
    ]
}

/// FunctionFS strings record PAYLOAD (after the magic+length head, uapi
/// functionfs.h): str_count, lang_count, then per language a le16 langid
/// followed by str_count NUL-terminated UTF-8 strings. The kernel-side
/// table is not what the host reads (ep0 answers carry
/// [`DescriptorSet::usb_strings`]); the record just has to parse.
fn ffs_strings_record(config: &DeviceConfig) -> Vec<u8> {
    let strings = device_strings(config);
    let mut b = Vec::new();
    b.extend_from_slice(&(strings.len() as u32).to_le_bytes()); // str_count
    b.extend_from_slice(&1u32.to_le_bytes()); // lang_count
    b.extend_from_slice(&0x0409u16.to_le_bytes()); // en-US
    for s in &strings {
        b.extend_from_slice(s.as_bytes());
        b.push(0);
    }
    b
}

/// USB string descriptors (USB 2.0 §9.6.7) for the ep0
/// GET_DESCRIPTOR(STRING) answer: language-id descriptor + UTF-16LE
/// string descriptors in STR_* order.
fn usb_strings_record(config: &DeviceConfig) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[4, 0x03, 0x09, 0x04]); // LANGID en-US
    for s in device_strings(config) {
        let utf16: Vec<u16> = s.encode_utf16().collect();
        let len = 2 + utf16.len() * 2;
        b.push(len as u8);
        b.push(0x03);
        for w in utf16 {
            b.extend_from_slice(&w.to_le_bytes());
        }
    }
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
        // Endpoint descriptors carry the high-speed MPS and addresses.
        for needle in [EP_CONTROL_OUT, EP_CONTROL_IN, EP_EVENT_IN, EP_STREAM_IN] {
            let pos = set
                .config
                .windows(7)
                .position(|w| w[1] == 0x05 && w[2] == needle)
                .unwrap_or_else(|| panic!("endpoint 0x{needle:02x}"));
            assert_eq!(&set.config[pos + 4..pos + 6], &HS_BULK_MPS.to_le_bytes());
        }

        // FunctionFS speed sets: interface-level only (no config header:
        // first descriptor is an INTERFACE), per-speed bulk MPS, the
        // endpoint order matches the host-facing blob, and NO
        // class-specific descriptors (kernel f_fs.c rejects CS_INTERFACE
        // for unknown classes — the Device Info CS lives in the ep0-served
        // `config` blob only).
        for (set_blob, mps) in [(&set.ffs_fs, FS_BULK_MPS), (&set.ffs_hs, HS_BULK_MPS)] {
            assert_eq!(set_blob[1], d::DESC_INTERFACE, "starts at interface level");
            let ifs = set_blob
                .windows(9)
                .filter(|w| w[0] == 9 && w[1] == d::DESC_INTERFACE)
                .count();
            assert_eq!(ifs, 3, "three interfaces");
            let eps = set_blob
                .windows(7)
                .filter(|w| w[0] == 7 && w[1] == 0x05)
                .count();
            assert_eq!(eps, 4, "four bulk endpoints");
            assert!(
                !set_blob.windows(2).any(|w| w[1] == d::DESC_CS_INTERFACE),
                "ffs sets carry no CS descriptors (f_fs.c rejects them)"
            );
            for needle in [EP_CONTROL_OUT, EP_CONTROL_IN, EP_EVENT_IN, EP_STREAM_IN] {
                let pos = set_blob
                    .windows(7)
                    .position(|w| w[1] == 0x05 && w[2] == needle)
                    .unwrap_or_else(|| panic!("endpoint 0x{needle:02x}"));
                assert_eq!(&set_blob[pos + 4..pos + 6], &mps.to_le_bytes());
            }
        }
        // The host-facing config blob still carries the Device Info CS.
        assert!(
            set.config.windows(2).any(|w| w[1] == d::DESC_CS_INTERFACE),
            "config blob carries the U3V Device Info CS descriptor"
        );
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

    /// The FunctionFS strings payload is the v2 format (uapi
    /// functionfs.h): str_count, lang_count, per-language le16 langid +
    /// NUL-terminated UTF-8 strings — NOT raw USB string descriptors.
    #[test]
    fn ffs_strings_record_layout() {
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
        let p = &set.ffs_strings;
        assert_eq!(&p[0..4], &4u32.to_le_bytes(), "str_count");
        assert_eq!(&p[4..8], &1u32.to_le_bytes(), "lang_count");
        assert_eq!(&p[8..10], &0x0409u16.to_le_bytes(), "en-US");
        let mut nul = 0;
        for &b in &p[10..] {
            if b == 0 {
                nul += 1;
            }
        }
        assert_eq!(nul, 4, "four NUL-terminated strings");
        // The ep0 STRING answer stays in USB descriptor format.
        assert_eq!(&set.usb_strings[..4], &[4, 0x03, 0x09, 0x04]);
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
