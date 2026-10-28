//! USB/U3V descriptor constants and the pure configuration-descriptor
//! parser, shared by both USB backends (Phase B Linux port): the Windows
//! backend reads the raw blob via WinUsb_GetDescriptor, the Linux backend
//! re-serializes rusb's parsed descriptors into the same blob shape
//! (`linux/discovery.rs::config_blob`) — one parser, one test set, both
//! platforms. Everything here is spec-traced (U3V 1.2 §3.2 / USB 2.0 §9.6).

// Device-level class codes of a U3V function (U3V 1.2 Table 3-1). The
// interface-level codes are 0xEF/0x05/0x00-0x02 (Tables 3-3/3-6/3-8).
pub const USB_CLASS_MISC: u8 = 0xEF;
pub const USB_DEVICE_SUBCLASS_U3V: u8 = 0x02;
pub const USB_INTERFACE_SUBCLASS_U3V: u8 = 0x05;
pub const USB_PROTOCOL_CONTROL: u8 = 0x00;
pub const USB_PROTOCOL_EVENT: u8 = 0x01;
pub const USB_PROTOCOL_STREAMING: u8 = 0x02;

// USB descriptor types (USB 2.0 §9.6).
pub const DESC_CONFIGURATION: u8 = 0x02;
// String-descriptor reads are a Windows-discovery concern (WinUSB
// GetDescriptor path); the libusb backend goes through rusb.
#[cfg(windows)]
pub const DESC_STRING: u8 = 0x03;
pub const DESC_INTERFACE: u8 = 0x04;
pub const DESC_INTERFACE_ASSOCIATION: u8 = 0x0B;
// U3V class-specific interface descriptor (§3.2.5).
pub const DESC_CS_INTERFACE: u8 = 0x24;
pub const U3V_DEVICEINFO_SUBTYPE: u8 = 0x01;
pub const U3V_DEVICEINFO_LEN: usize = 20;

/// Raw Device Info Descriptor fields (§3.2.5 Table 3-5).
#[derive(Debug, Clone, Copy)]
pub struct DeviceInfoDescriptor {
    pub gen_cp_version: u32,
    pub u3v_version: u32,
    pub i_device_guid: u8,
    pub i_vendor_name: u8,
    pub i_model_name: u8,
    pub i_family_name: u8,
    pub i_device_version: u8,
    pub i_manufacturer_info: u8,
    pub i_serial_number: u8,
    pub i_user_defined_name: u8,
}

/// A parsed standard interface descriptor with its class-specific ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInterface {
    pub number: u8,
    pub class: u8,
    pub sub_class: u8,
    pub protocol: u8,
    pub class_specific: Vec<Vec<u8>>,
}

/// Minimal structural parse of a configuration descriptor blob:
/// configuration header + interface association descriptors + interfaces
/// with their class-specific descriptors. Endpoint descriptors are not
/// needed for matching (endpoint discovery happens per backend at open
/// time).
pub fn parse_config_descriptor(buf: &[u8]) -> Option<ParsedConfig> {
    if buf.len() < 9 || buf[1] != DESC_CONFIGURATION {
        return None;
    }
    let total = u16::from_le_bytes([buf[2], buf[3]]) as usize;
    let limit = total.min(buf.len());
    let mut cfg = ParsedConfig::default();
    let mut pos = 9usize; // skip CONFIGURATION header
    let mut current: Option<usize> = None; // index into cfg.interfaces
    while pos + 2 <= limit {
        let len = buf[pos] as usize;
        let dtype = buf[pos + 1];
        if len < 3 || pos + len > limit {
            break;
        }
        let body = &buf[pos..pos + len];
        match dtype {
            DESC_INTERFACE_ASSOCIATION if len >= 8 => {
                cfg.iads.push(body.to_vec());
                current = None;
            }
            DESC_INTERFACE if len >= 9 => {
                cfg.interfaces.push(ParsedInterface {
                    number: body[2],
                    class: body[5],
                    sub_class: body[6],
                    protocol: body[7],
                    class_specific: Vec::new(),
                });
                current = Some(cfg.interfaces.len() - 1);
            }
            DESC_CS_INTERFACE => {
                if let Some(i) = current {
                    cfg.interfaces[i].class_specific.push(body.to_vec());
                }
            }
            _ => {}
        }
        pos += len;
    }
    Some(cfg)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedConfig {
    pub iads: Vec<Vec<u8>>,
    pub interfaces: Vec<ParsedInterface>,
}

/// Locate the U3V Device Info descriptor inside a configuration
/// descriptor blob. The descriptor must follow the default Device Control
/// Interface descriptor (R-15cd); we accept it anywhere among the
/// class-specific descriptors of an EF/05/00 interface — position is an
/// ordering rule for transmitters, not a parsing constraint for hosts.
pub fn device_info_descriptor(cfg: &[u8]) -> Option<DeviceInfoDescriptor> {
    for iface in parse_config_descriptor(cfg)?.interfaces.iter() {
        if iface.class != USB_CLASS_MISC
            || iface.sub_class != USB_INTERFACE_SUBCLASS_U3V
            || iface.protocol != USB_PROTOCOL_CONTROL
        {
            continue;
        }
        for cs in &iface.class_specific {
            if cs.len() == U3V_DEVICEINFO_LEN
                && cs[1] == DESC_CS_INTERFACE
                && cs[2] == U3V_DEVICEINFO_SUBTYPE
            {
                return Some(DeviceInfoDescriptor {
                    gen_cp_version: u32::from_le_bytes([cs[3], cs[4], cs[5], cs[6]]),
                    u3v_version: u32::from_le_bytes([cs[7], cs[8], cs[9], cs[10]]),
                    i_device_guid: cs[11],
                    i_vendor_name: cs[12],
                    i_model_name: cs[13],
                    i_family_name: cs[14],
                    i_device_version: cs[15],
                    i_manufacturer_info: cs[16],
                    i_serial_number: cs[17],
                    i_user_defined_name: cs[18],
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal U3V configuration descriptor:
    /// config(9) + IAD(8) + control IF(9) + CS devinfo(20) + event IF(9)
    /// + streaming IF(9).
    fn sample_config() -> Vec<u8> {
        let mut b = Vec::new();
        let total_len_pos = b.len() + 2;
        b.extend_from_slice(&[9, DESC_CONFIGURATION, 0, 0, 1, 0, 0, 0, 0xE0]);
        b.extend_from_slice(&[
            8,
            DESC_INTERFACE_ASSOCIATION,
            0, // bFirstInterface
            3, // bInterfaceCount
            0xEF,
            0x05,
            0x00, // function codes
            0,    // iFunction
        ]);
        b.extend_from_slice(&[9, DESC_INTERFACE, 0, 0, 1, 0xEF, 0x05, 0x00, 0]);
        b.extend_from_slice(&[
            20,
            DESC_CS_INTERFACE,
            0x01, // U3V_DEVICEINFO
            0x02,
            0x00,
            0x03,
            0x00, // GenCP version 3.2 (minor, major)
            0x02,
            0x00,
            0x01,
            0x00, // U3V version 1.2
            5,
            1,
            2,
            3,
            4,
            6,
            7,
            8,    // string indexes
            0x08, // bmSpeedSupport: SuperSpeed
        ]);
        b.extend_from_slice(&[9, DESC_INTERFACE, 1, 0, 1, 0xEF, 0x05, 0x01, 0]);
        b.extend_from_slice(&[9, DESC_INTERFACE, 2, 0, 1, 0xEF, 0x05, 0x02, 0]);
        let total = b.len() as u16;
        b[total_len_pos] = total as u8;
        b[total_len_pos + 1] = (total >> 8) as u8;
        b
    }

    #[test]
    fn config_parse_finds_u3v_interfaces() {
        let cfg = parse_config_descriptor(&sample_config()).expect("parse");
        assert_eq!(cfg.iads.len(), 1);
        assert_eq!(cfg.interfaces.len(), 3);
        assert_eq!(cfg.interfaces[0].protocol, USB_PROTOCOL_CONTROL);
        assert_eq!(cfg.interfaces[1].protocol, USB_PROTOCOL_EVENT);
        assert_eq!(cfg.interfaces[2].protocol, USB_PROTOCOL_STREAMING);
        assert_eq!(cfg.interfaces[0].class_specific[0].len(), 20);
    }

    #[test]
    fn device_info_descriptor_fields() {
        let cfg = sample_config();
        let d = device_info_descriptor(&cfg).expect("devinfo");
        assert_eq!(d.gen_cp_version, 0x0003_0002);
        assert_eq!(d.u3v_version, 0x0001_0002);
        assert_eq!(d.i_device_guid, 5);
        assert_eq!(d.i_vendor_name, 1);
        assert_eq!(d.i_serial_number, 7);
        assert_eq!(d.i_user_defined_name, 8);
    }

    #[test]
    fn device_info_rejects_foreign_interface() {
        let mut cfg = sample_config();
        // Flip the subclass of the first interface descriptor (the
        // control interface, R-10cd ordering).
        let pos = cfg
            .windows(9)
            .position(|w| w[1] == DESC_INTERFACE && w[5] == 0xEF)
            .expect("control interface descriptor");
        cfg[pos + 6] = 0x99;
        assert!(device_info_descriptor(&cfg).is_none());
    }
}
