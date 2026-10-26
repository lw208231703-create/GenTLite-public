//! Linux FunctionFS physical layer — the integration path that binds a
//! [`crate::U3vDevice`] to a real USB gadget port (Orange Pi Type-C /
//! dwc3). **Machine bring-up 待验** (design note §5, Q39): everything
//! protocol-shaped is unit-tested in `device/`; this module only moves
//! bytes between configfs/`/dev/*-ep*` files and the device traits.
//!
//! Runtime prerequisites (all surface as `io::Error`, never panic):
//! `libcomposite` loaded, configfs mounted at `/sys/kernel/config`, a
//! dwc3 UDC under `/sys/class/udc/`, and the FunctionFS instance mounted
//! on the function's `mount/` directory by the operator. The module
//! compiles on every platform (pure `std::fs` + `std::os::unix` behind
//! its call sites); on a working setup it drives the full device.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::device::{DeviceTransfers, HostCommands, StreamError, desc::DescriptorSet};
use crate::{DeviceConfig, U3vDevice};

/// Gadget identity and location knobs.
pub struct GadgetConfig {
    /// configfs gadget directory name (`/sys/kernel/config/usb_gadget/<name>`).
    pub name: String,
    pub vid: u16,
    pub pid: u16,
    /// USB manufacturer/product/serial strings (the camera identity).
    pub device: DeviceConfig,
}

impl GadgetConfig {
    pub fn new(name: &str, vid: u16, pid: u16, device: DeviceConfig) -> Self {
        Self {
            name: name.into(),
            vid,
            pid,
            device,
        }
    }
}

/// Provision the configfs gadget: directory tree, id/vendor/strings,
/// configuration with the ffs function, UDC bind. Returns the paths the
/// FunctionFS mount needs.
///
/// The file set follows the kernel configfs gadget schema
/// (Documentation/ABI/testing/configfs-usb-gadget); every write is
/// checked and errors carry the path (configfs mounted? libcomposite
/// loaded?).
pub fn provision(config: &GadgetConfig) -> std::io::Result<GadgetPaths> {
    provision_impl(config)
}

#[cfg(unix)]
fn provision_impl(config: &GadgetConfig) -> std::io::Result<GadgetPaths> {
    let root = Path::new("/sys/kernel/config/usb_gadget").join(&config.name);
    let ffs_function = format!("ffs.{}", config.name);
    let udc = pick_udc()?;

    write_file(root.join("idVendor"), format!("0x{:04x}", config.vid))?;
    write_file(root.join("idProduct"), format!("0x{:04x}", config.pid))?;
    write_file(root.join("bcdDevice"), "0x0100")?;
    write_file(root.join("bcdUSB"), "0x0320")?;
    let strings = root.join("strings/0x409");
    std::fs::create_dir_all(&strings)?;
    write_file(strings.join("manufacturer"), &config.device.manufacturer)?;
    write_file(strings.join("product"), &config.device.model)?;
    write_file(strings.join("serialnumber"), &config.device.serial_number)?;
    let cfgdir = root.join("configs/c.1");
    std::fs::create_dir_all(&cfgdir)?;
    let funcdir = root.join(format!("functions/{ffs_function}"));
    std::fs::create_dir_all(&funcdir)?;
    let mount = funcdir.join("mount");
    std::fs::create_dir_all(&mount)?;
    let sym = cfgdir.join(&ffs_function);
    if !sym.exists() {
        std::os::unix::fs::symlink(&funcdir, &sym)?;
    }
    write_file(root.join("UDC"), &udc)?;
    Ok(GadgetPaths { root, ffs_function })
}

#[cfg(not(unix))]
fn provision_impl(_config: &GadgetConfig) -> std::io::Result<GadgetPaths> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the gadget path requires Linux (configfs + FunctionFS); the protocol device is testable without it",
    ))
}

pub struct GadgetPaths {
    pub root: PathBuf,
    /// The FunctionFS instance (`ffs.<name>`), already mkdir'd at
    /// `functions/<ffs>/mount` for the operator's `mount -t functionfs`.
    pub ffs_function: String,
}

/// First UDC (dwc3 on the Orange Pi). QoI (Q39): multi-UDC hosts should
/// select explicitly; v1 takes the sorted first.
#[cfg(unix)]
fn pick_udc() -> std::io::Result<String> {
    let mut entries = std::fs::read_dir("/sys/class/udc")?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<Vec<_>>();
    entries.sort();
    entries.first().cloned().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no UDC under /sys/class/udc (dwc3/gadget driver missing?)",
        )
    })
}

#[cfg(unix)]
fn write_file(path: impl AsRef<Path>, contents: impl AsRef<str>) -> std::io::Result<()> {
    let path = path.as_ref();
    std::fs::write(path, contents.as_ref()).map_err(|e| {
        std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
    })
}

/// One SETUP event on FunctionFS ep0 (struct usb_ctrlrequest, USB 2.0
/// §9.3): 8 bytes, little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupEvent {
    pub bm_request_type: u8,
    pub b_request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl SetupEvent {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 8 {
            return None;
        }
        Some(Self {
            bm_request_type: buf[0],
            b_request: buf[1],
            value: u16::from_le_bytes([buf[2], buf[3]]),
            index: u16::from_le_bytes([buf[4], buf[5]]),
            length: u16::from_le_bytes([buf[6], buf[7]]),
        })
    }

    /// GET_DESCRIPTOR (bRequest 6, USB 2.0 §9.4): the host pulls device/
    /// config/string descriptors over ep0.
    pub fn is_get_descriptor(&self) -> bool {
        self.b_request == 6
    }

    /// Descriptor type selector from wValue (high byte).
    pub fn descriptor_type(&self) -> u8 {
        (self.value >> 8) as u8
    }
}

/// Bundle of gadget-side byte streams handed to the device factory.
pub struct GadgetStreams {
    pub control_out: Arc<dyn HostCommands>,
    pub control_in: Arc<dyn DeviceTransfers>,
    pub stream_in: Arc<dyn DeviceTransfers>,
}

/// The integration entry (CLI `--front u3v` on Linux), split in two so
/// the CLI can build the relay between opening the gadget and serving
/// ep0: [`open`] provisions configfs, runs the FunctionFS descriptor
/// handshake, bridges the bulk endpoints into [`GadgetStreams`];
/// [`OpenGadget::serve`] runs the ep0 SETUP loop until the device stops.
///
/// **待验** (Q39): the byte plumbing underneath is unit-tested in
/// `device/`; the configfs/FunctionFS sequence here is structured but
/// not yet exercised on the Orange Pi.
pub fn open(config: &GadgetConfig) -> std::io::Result<OpenGadget> {
    let paths = provision(config)?;
    let mount = paths
        .root
        .join("functions")
        .join(&paths.ffs_function)
        .join("mount");
    let ep0_path = mount.join("ep0");
    let mut ep0 = File::open(&ep0_path).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "{}: {e} (FunctionFS mounted at {}?)",
                ep0_path.display(),
                mount.display()
            ),
        )
    })?;

    // The descriptor set is built from the same config the device gets,
    // so the ep0 answers and the device's ABRM agree on identity.
    let set = DescriptorSet::build(&config.device, config.vid, config.pid);
    write_ffs_record(&mut ep0, &ffs_descs_head(&set), &set.config)?;
    write_ffs_record(&mut ep0, &ffs_strings_head(&set), &set.strings)?;

    let out = open_ep(&mount, "ep1")?; // control bulk OUT (host→device)
    let cin = open_ep(&mount, "ep2")?; // control bulk IN (device→host)
    let sin = open_ep(&mount, "ep3")?; // streaming bulk IN (device→host)

    let (cmd_tx, cmd_rx) = sync_channel(16);
    std::thread::spawn(move || bridge_out(out, cmd_tx));
    let streams = GadgetStreams {
        control_out: Arc::new(QueueCommands { rx: Mutex::new(cmd_rx) }),
        control_in: Arc::new(FileTransfers(cin)),
        stream_in: Arc::new(FileTransfers(sin)),
    };
    Ok(OpenGadget { streams, ep0, set })
}

/// A provisioned gadget ready to serve once the device exists.
pub struct OpenGadget {
    pub streams: GadgetStreams,
    ep0: File,
    set: DescriptorSet,
}

impl OpenGadget {
    /// The ep0 SETUP loop; blocks until the device stops or ep0 errors.
    pub fn serve(mut self, device: &U3vDevice) -> std::io::Result<()> {
        ep0_loop(&mut self.ep0, device, &self.set)
    }
}

/// ep0 SETUP service: answer GET_DESCRIPTOR from the descriptor set,
/// zero-length-ACK everything else (status phase). Ends when the device
/// stops or ep0 errors.
fn ep0_loop(ep0: &mut File, device: &U3vDevice, set: &DescriptorSet) -> std::io::Result<()> {
    let mut buf = [0u8; 8];
    while !device.stop_requested() {
        let n = ep0.read(&mut buf)?;
        if n < 8 {
            continue;
        }
        let Some(ev) = SetupEvent::parse(&buf[..8]) else { continue };
        if ev.is_get_descriptor() {
            let blob = match ev.descriptor_type() {
                0x01 => &set.device,
                0x02 => &set.config,
                0x03 => &set.strings,
                _ => continue,
            };
            let n = blob.len().min(ev.length as usize);
            ep0.write_all(&blob[..n])?;
        } else {
            ep0.write_all(&[])?;
        }
    }
    Ok(())
}

/// FunctionFS descriptor record header (functionfs_descs_head_v2:
/// magic FUNCTIONFS_DESCRIPTORS_MAGIC_V2 = 3, total length, flags, then
/// the fs/hs/ss/os full-descriptor counts). We emit the single
/// full-speed set for now; super-speed companion descriptors land at
/// bring-up (待验, Q39 — the kernel validates the counts against the
/// bytes that follow).
fn ffs_descs_head(set: &DescriptorSet) -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(&3u32.to_le_bytes()); // magic: descriptors v2
    h.extend_from_slice(&((16 + set.config.len()) as u32).to_le_bytes()); // length incl. head
    h.extend_from_slice(&0u32.to_le_bytes()); // flags
    h.extend_from_slice(&1u32.to_le_bytes()); // fs_count
    h.extend_from_slice(&0u32.to_le_bytes()); // hs_count
    h.extend_from_slice(&0u32.to_le_bytes()); // ss_count
    h.extend_from_slice(&0u32.to_le_bytes()); // os_count
    h
}

fn ffs_strings_head(set: &DescriptorSet) -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(&2u32.to_le_bytes()); // magic: strings
    h.extend_from_slice(&((8 + set.strings.len()) as u32).to_le_bytes());
    h
}

fn write_ffs_record(ep0: &mut File, head: &[u8], body: &[u8]) -> std::io::Result<()> {
    ep0.write_all(head)?;
    ep0.write_all(body)
}

fn open_ep(mount: &Path, name: &str) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(mount.join(name))
}

/// control OUT endpoint → command queue → [`HostCommands`].
fn bridge_out(mut file: File, tx: SyncSender<Vec<u8>>) {
    let mut buf = vec![0u8; 4096];
    loop {
        match file.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

struct QueueCommands {
    rx: Mutex<Receiver<Vec<u8>>>,
}

impl HostCommands for QueueCommands {
    fn recv(&self, timeout: Duration) -> Option<Vec<u8>> {
        let rx = self.rx.lock().ok()?;
        rx.recv_timeout(timeout).ok()
    }
}

/// A FunctionFS endpoint file as a device→host byte stream.
struct FileTransfers(File);

impl DeviceTransfers for FileTransfers {
    fn send(&self, bytes: &[u8]) -> Result<(), StreamError> {
        (&self.0).write_all(bytes).map_err(|e| StreamError::Other(e.to_string()))
    }
    fn max_packet(&self) -> u16 {
        crate::device::desc::BULK_MPS
    }
}

// DeviceConfig is Clone (plain data); the descriptor set and the device
// take the same values.
