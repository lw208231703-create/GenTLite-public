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
    std::fs::create_dir_all(&root)?;

    write_file(root.join("idVendor"), format!("0x{:04x}", config.vid))?;
    write_file(root.join("idProduct"), format!("0x{:04x}", config.pid))?;
    // Device-level class codes (U3V Table 3-1: EF/02/01). The composite
    // framework answers standard descriptor requests itself — including
    // the DEVICE descriptor built from THESE attributes — so the codes
    // must be right here, not only in the ep0-served blob. Aravis (and
    // likely other strict producers) keys U3V detection on them
    // (observed 2026-10-27: self-loop enumerated with class 0/0/0 and
    // was rejected; kernel-composed config also lacks the Device Info CS
    // — see A1).
    write_file(root.join("bcdDevice"), "0x0100")?;
    // U3V Table 3-1's SuperSpeed profile. The composite framework answers
    // standard descriptor requests from this attribute, so it must match
    // the device descriptor served by our ep0 path.
    write_file(root.join("bcdUSB"), "0x0320")?;
    // Allow the UDC and link partner to negotiate SuperSpeed. The earlier
    // high-speed bring-up cap prevented SS training even with a working PHY.
    write_file(root.join("max_speed"), "super-speed")?;
    let strings = root.join("strings/0x409");
    std::fs::create_dir_all(&strings)?;
    write_file(strings.join("manufacturer"), &config.device.manufacturer)?;
    write_file(strings.join("product"), &config.device.model)?;
    write_file(strings.join("serialnumber"), &config.device.serial_number)?;
    let cfgdir = root.join("configs/c.1");
    std::fs::create_dir_all(&cfgdir)?;
    let funcdir = root.join(format!("functions/{ffs_function}"));
    std::fs::create_dir_all(&funcdir)?;
    // FunctionFS mounts OUTSIDE configfs: the kernel exposes the endpoint
    // files at an arbitrary mount point, conventionally /dev/usb-ffs/<name>.
    // (Mounting inside the configfs function dir is EPERM — configfs
    // function directories do not accept arbitrary subdirectories.)
    let mount = Path::new("/dev/usb-ffs").join(&config.name);
    std::fs::create_dir_all(&mount)?;
    // The FunctionFS filesystem must be mounted on the function's mount
    // point BEFORE the gadget is enabled (UDC write below), otherwise the
    // ffs function enumerates without its endpoint files.
    if !std::fs::read_to_string("/proc/mounts")
        .map(|m| m.lines().any(|l| l.split_whitespace().nth(1) == mount.to_str().or(Some(""))))
        .unwrap_or(false)
    {
        // The mount source is the BARE instance name (the part after
        // "ffs."), not the full configfs function name: the kernel matches
        // f_fs devices by instance name (verified on the vendor kernel
        // 6.1.115-rk35xx — "ffs.foo" fails with ENOENT, "foo" mounts).
        // mount(2) is called directly rather than spawning mount(8):
        // util-linux canonicalizes a relative source that EXISTS in the
        // caller's CWD into an absolute path (strace: source
        // "/root/GenTLite-public/vgige" from CWD /root/GenTLite-public,
        // kernel ENODEV), which breaks the name lookup.
        mount_functionfs(&config.name, &mount)
            .map_err(|e| std::io::Error::new(e.kind(), format!("mount functionfs {ffs_function} on {}: {e}", mount.display())))?;
    }
    let sym = cfgdir.join(&ffs_function);
    if !sym.exists() {
        std::os::unix::fs::symlink(&funcdir, &sym)?;
    }
    // The UDC write is deliberately NOT here: this kernel (6.12, and per
    // f_fs.c the bind path in general) refuses to bind an ffs function
    // whose descriptors have not been written yet — `echo $UDC > UDC`
    // fails with EBUSY and dmesg shows `adding 'Function FS Gadget' -->
    // -19`. Call [`enable`] only after the ep0 descriptor handshake.
    Ok(GadgetPaths {
        root,
        ffs_function,
        mount,
        udc,
    })
}

/// Bind the gadget to the UDC. Call only after the FunctionFS descriptors
/// have been written (the ep0 handshake in [`open`]), otherwise the ffs
/// function's bind fails with ENODEV (see [`provision`]).
#[cfg(unix)]
pub fn enable(paths: &GadgetPaths) -> std::io::Result<()> {
    write_file(paths.root.join("UDC"), &paths.udc)
}

/// Detach the gadget from the UDC (host sees a disconnect).
#[cfg(unix)]
pub fn disable(paths: &GadgetPaths) -> std::io::Result<()> {
    write_file(paths.root.join("UDC"), "")
}

#[cfg(not(unix))]
pub fn enable(_paths: &GadgetPaths) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "gadget UDC bind requires Linux",
    ))
}

#[cfg(not(unix))]
pub fn disable(_paths: &GadgetPaths) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "gadget UDC bind requires Linux",
    ))
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
    /// The FunctionFS instance (`ffs.<name>`), mounted at `/dev/usb-ffs/<name>`.
    pub ffs_function: String,
    pub mount: PathBuf,
    /// The UDC the gadget binds to on [`enable`].
    pub udc: String,
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

fn write_file(path: impl AsRef<Path>, contents: impl AsRef<str>) -> std::io::Result<()> {
    let path = path.as_ref();
    std::fs::write(path, contents.as_ref()).map_err(|e| {
        std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
    })
}

/// `mount(2)` a FunctionFS instance by its bare name (see [`provision`]).
#[cfg(unix)]
fn mount_functionfs(source: &str, target: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "ffs instance name contains NUL"))?;
    let target = std::ffi::CString::new(target.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "ffs mount path contains NUL"))?;
    let ret = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            c"functionfs".as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
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
    pub event_in: Arc<dyn DeviceTransfers>,
}

/// The integration entry (CLI `--front u3v` on Linux), split in two so
/// the CLI can build the relay between opening the gadget and serving
/// ep0: [`open`] provisions configfs, runs the FunctionFS descriptor
/// handshake, binds the gadget and bridges the bulk endpoints into
/// [`GadgetStreams`]; [`OpenGadget::serve`] runs the ep0 event loop
/// until the device stops.
///
/// Bring-up order (verified against kernel f_fs.c 6.12,
/// usb_functionfs_descs_head_v2 in uapi linux/usb/functionfs.h):
/// descriptors + strings are written to ep0 FIRST (each record in a
/// SINGLE write — the kernel validates each write's length against the
/// header), then the gadget is bound (UDC write) — only the bind
/// creates the endpoint files, and the ffs function refuses to bind
/// without descriptors. The endpoint files are ep1..ep4 in descriptor
/// order: control OUT, control IN, event IN, stream IN.
pub fn open(config: &GadgetConfig) -> std::io::Result<OpenGadget> {
    let paths = provision(config)?;
    let mount = paths.mount.clone();
    let ep0_path = mount.join("ep0");
    // ep0 carries both directions: SETUP events are read from it, answers
    // written to it — it must be opened read+write (O_RDONLY fails EBADF
    // on the descriptor write below).
    let mut ep0 = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&ep0_path)
        .map_err(|e| {
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
    // FunctionFS v2 records: magic, length (== the write size), flags,
    // then the per-speed descriptor counts CONDITIONALLY on the flags
    // (uapi functionfs.h: "Depending on which flags are set, various
    // fields may be missing" — trailing count words are parsed as
    // descriptors and fail EINVAL). Class-specific descriptors are NOT
    // allowed in the record (f_fs.c whitelists HID/CCID/DFU only); the
    // U3V Device Info CS is served via the ep0 config answer.
    let mut desc_rec = ffs_descs_head(&set.ffs_fs, &set.ffs_hs, &set.ffs_ss);
    desc_rec.extend_from_slice(&set.ffs_fs);
    desc_rec.extend_from_slice(&set.ffs_hs);
    desc_rec.extend_from_slice(&set.ffs_ss);
    let total = desc_rec.len() as u32;
    desc_rec[4..8].copy_from_slice(&total.to_le_bytes()); // length == write size (f_fs.c checks)
    ep0.write_all(&desc_rec)
        .map_err(|e| ctx(&ep0_path, "descriptor record", e))?;
    let mut str_rec = ffs_strings_head(&set);
    str_rec.extend_from_slice(&set.ffs_strings);
    ep0.write_all(&str_rec)
        .map_err(|e| ctx(&ep0_path, "strings record", e))?;

    // Bind BEFORE opening the endpoints: the kernel creates the ep1..
    // files at bind time (ffs_func_bind → ffs_epfiles_create).
    enable(&paths)
        .map_err(|e| std::io::Error::new(e.kind(), format!("enable (UDC bind): {e}")))?;

    // Device-level class codes (U3V Table 3-1: EF/02/01), written AFTER
    // the bind: on 6.12 configfs the pre-bind values are clobbered during
    // function linking (a pre-bind write reads back 0x00 once provision
    // completes; the same write on the bound gadget sticks — verified
    // 2026-10-27). Composite answers standard GET_DESCRIPTOR from
    // cdev->desc at request time, so these land before the host reads
    // the device descriptor. Aravis gates U3V discovery on these codes.
    // NOTE: configfs hex formats are per-attribute empirical — these
    // three REQUIRE the 0x prefix (writing "EF" gives EINVAL), unlike
    // idVendor/idProduct. Do not "tidy".
    write_file(paths.root.join("bDeviceClass"), "0xEF")?;
    write_file(paths.root.join("bDeviceSubClass"), "0x02")?;
    write_file(paths.root.join("bDeviceProtocol"), "0x01")?;

    let out = open_ep(&mount, "ep1")?; // control bulk OUT (host→device)
    let cin = open_ep(&mount, "ep2")?; // control bulk IN (device→host)
    let ein = open_ep(&mount, "ep3")?; // event bulk IN (device→host, parked)
    let sin = open_ep(&mount, "ep4")?; // streaming bulk IN (device→host)

    let (cmd_tx, cmd_rx) = sync_channel(16);
    std::thread::spawn(move || bridge_out(out, cmd_tx));
    let streams = GadgetStreams {
        control_out: Arc::new(QueueCommands { rx: Mutex::new(cmd_rx) }),
        control_in: Arc::new(FileTransfers { file: cin }),
        stream_in: Arc::new(FileTransfers { file: sin }),
        event_in: Arc::new(FileTransfers { file: ein }),
    };
    Ok(OpenGadget { streams, ep0, set, paths })
}

/// A provisioned gadget ready to serve once the device exists.
pub struct OpenGadget {
    pub streams: GadgetStreams,
    ep0: File,
    set: DescriptorSet,
    paths: GadgetPaths,
}

impl OpenGadget {
    /// The ep0 event loop; blocks until the device stops or ep0 errors.
    /// Detaches the gadget from the UDC when the loop ends.
    pub fn serve(mut self, device: &U3vDevice) -> std::io::Result<()> {
        let r = ep0_loop(&mut self.ep0, device, &self.set);
        let _ = disable(&self.paths);
        r
    }
}

/// FunctionFS event record (struct usb_functionfs_event, uapi
/// functionfs.h): the setup packet (8 bytes, USB 2.0 §9.3) followed by
/// the event type at offset 8. Reading ep0 yields whole events, not raw
/// setup packets.
const EVENT_LEN: usize = 12;
const EVENT_SETUP: u8 = 4; // FUNCTIONFS_SETUP

/// ep0 service: consume events; for SETUP events answer
/// GET_DESCRIPTOR from the descriptor set and zero-length-ACK everything
/// else (status phase) — including descriptor types we do not carry
/// (BOS): every FunctionFS SETUP must be answered, an unanswered request
/// stalls enumeration. SETUP events with an OUT data stage are drained
/// before the status ACK. Ends when the device stops or ep0 errors.
fn ep0_loop(ep0: &mut File, device: &U3vDevice, set: &DescriptorSet) -> std::io::Result<()> {
    let mut buf = [0u8; EVENT_LEN];
    while !device.stop_requested() {
        let n = ep0.read(&mut buf)?;
        if n < EVENT_LEN || buf[8] != EVENT_SETUP {
            continue;
        }
        let Some(ev) = SetupEvent::parse(&buf[..8]) else { continue };
        let host_to_device = ev.bm_request_type & 0x80 == 0;
        if host_to_device && ev.length > 0 {
            // OUT data stage: drain it, then the status ACK below.
            let mut left = ev.length as usize;
            let mut chunk = [0u8; 4096];
            while left > 0 {
                let take = left.min(chunk.len());
                let n = ep0.read(&mut chunk[..take])?;
                if n == 0 {
                    break;
                }
                left -= n;
            }
            ep0.write_all(&[])?;
            continue;
        }
        if ev.is_get_descriptor() {
            match ev.descriptor_type() {
                0x01 | 0x02 => {
                    // DEVICE / CONFIGURATION, per the negotiated link
                    // speed: a SuperSpeed link must be answered with the
                    // SS shapes (bcdUSB 3.2 / 1024-MPS endpoints with
                    // companions) or the host falls back to USB 2
                    // (observed on the OTG bring-up, Q39). The link speed
                    // is the UDC's own view (/sys/class/udc).
                    let ss = serve_speed_is_ss();
                    let blob = match (ev.descriptor_type(), ss) {
                        (0x01, true) => &set.device_ss,
                        (0x01, false) => &set.device,
                        (0x02, true) => &set.config_ss,
                        (_, false) => &set.config,
                        _ => unreachable!(),
                    };
                    let n = blob.len().min(ev.length as usize);
                    ep0.write_all(&blob[..n])?;
                }
                0x0F => {
                    // BOS: USB 2.0 Extension + SuperSpeed capability.
                    let n = set.bos.len().min(ev.length as usize);
                    ep0.write_all(&set.bos[..n])?;
                }
                0x03 => {
                    // STRING: the request's wValue low byte selects ONE
                    // descriptor from the table (index 0 = the language-id
                    // descriptor, 1..=N the strings in STR_* order). The
                    // whole table is NOT a valid answer — Windows reads
                    // the GUID string (index 4) during discovery and an
                    // off-index answer fails the identity read (observed
                    // as openable=false in the GenTL device list).
                    let index = (ev.value & 0xFF) as usize;
                    match usb_string_by_index(&set.usb_strings, index) {
                        Some(d) => {
                            let n = d.len().min(ev.length as usize);
                            ep0.write_all(&d[..n])?;
                        }
                        None => ep0.write_all(&[])?,
                    }
                }
                _ => ep0.write_all(&[])?,
            }
        } else {
            ep0.write_all(&[])?;
        }
    }
    Ok(())
}

/// The `index`-th USB string descriptor in a concatenated table (index 0
/// = the language-id descriptor), walked by bLength.
fn usb_string_by_index(table: &[u8], index: usize) -> Option<&[u8]> {
    let mut pos = 0;
    let mut n = 0;
    while pos + 2 <= table.len() {
        let len = table[pos] as usize;
        if len < 2 || pos + len > table.len() {
            break;
        }
        if n == index {
            return Some(&table[pos..pos + len]);
        }
        n += 1;
        pos += len;
    }
    None
}

/// FunctionFS descriptor record header (functionfs_descs_head_v2:
/// magic FUNCTIONFS_DESCRIPTORS_MAGIC_V2 = 3, length, flags — then the
/// per-speed counts CONDITIONALLY on the flags, uapi functionfs.h).
/// We emit full- and high-speed sets; the super-speed set lands with
/// the SS front-end (待验, Q39). Two content rules are enforced by
/// kernel f_fs.c and were hit during bring-up: usb_ep_autoconfig
/// rejects an MPS the speed does not allow (an FS bulk MPS of 1024
/// fails the whole bind with ENODEV), and class-specific descriptors
/// are rejected with EINVAL outside the HID/CCID/DFU whitelist.
fn ffs_descs_head(fs: &[u8], hs: &[u8], ss: &[u8]) -> Vec<u8> {
    const HAS_FS: u32 = 1; // FUNCTIONFS_HAS_FS_DESC
    const HAS_HS: u32 = 2; // FUNCTIONFS_HAS_HS_DESC
    const HAS_SS: u32 = 4; // FUNCTIONFS_HAS_SS_DESC
    let mut flags = 0u32;
    let mut h = Vec::new();
    h.extend_from_slice(&3u32.to_le_bytes()); // magic: descriptors v2
    h.extend_from_slice(&0u32.to_le_bytes()); // length, patched by the caller
    h.extend_from_slice(&0u32.to_le_bytes()); // flags, patched below
    if !fs.is_empty() {
        flags |= HAS_FS;
        h.extend_from_slice(&count_descriptors(fs).to_le_bytes());
    }
    if !hs.is_empty() {
        flags |= HAS_HS;
        h.extend_from_slice(&count_descriptors(hs).to_le_bytes());
    }
    if !ss.is_empty() {
        flags |= HAS_SS;
        h.extend_from_slice(&count_descriptors(ss).to_le_bytes());
    }
    h[8..12].copy_from_slice(&flags.to_le_bytes());
    h
}

/// True when the gadget's UDC link negotiated SuperSpeed — the UDC sysfs
/// attribute is the kernel's own view of the wire speed. Single-UDC
/// board (dwc3 at fc000000); multi-UDC hosts should thread the name
/// through (QoI, Q39).
fn serve_speed_is_ss() -> bool {
    std::fs::read_to_string("/sys/class/udc/fc000000.usb/current_speed")
        .map(|s| s.trim() == "super-speed")
        .unwrap_or(false)
}

/// Number of descriptors in a concatenated blob (walked by bLength).
fn count_descriptors(blob: &[u8]) -> u32 {
    let mut n = 0;
    let mut pos = 0;
    while pos + 2 <= blob.len() {
        let len = blob[pos] as usize;
        if len < 2 || pos + len > blob.len() {
            break;
        }
        n += 1;
        pos += len;
    }
    n
}

fn ffs_strings_head(set: &DescriptorSet) -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(&2u32.to_le_bytes()); // magic: strings
    h.extend_from_slice(&((8 + set.ffs_strings.len()) as u32).to_le_bytes()); // length, == write size
    h
}

fn open_ep(mount: &Path, name: &str) -> std::io::Result<File> {
    let path = mount.join(name);
    OpenOptions::new().read(true).write(true).open(&path).map_err(|e| {
        std::io::Error::new(e.kind(), format!("{}: {e} (descriptor set written?)", path.display()))
    })
}

/// Error context for the ep0 handshake writes.
fn ctx(path: &Path, step: &str, e: std::io::Error) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("ep0 {step} ({}): {e}", path.display()))
}

/// control OUT endpoint → command queue → [`HostCommands`].
///
/// Immortal by design: a FunctionFS read can fail transiently (USB reset
/// during host re-enumeration, suspend/resume) and returning would drop
/// the queue's sender, turning the GenCP server loop into a busy spin on
/// the disconnected channel. Retry forever; the daemon's teardown path
/// unmounts the function, which fails the read permanently and ends the
/// thread then.
fn bridge_out(mut file: File, tx: SyncSender<Vec<u8>>) {
    let mut buf = vec![0u8; 4096];
    loop {
        match file.read(&mut buf) {
            Err(_) | Ok(0) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
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
struct FileTransfers {
    file: File,
}

impl DeviceTransfers for FileTransfers {
    fn send(&self, bytes: &[u8]) -> Result<(), StreamError> {
        (&self.file).write_all(bytes).map_err(|e| StreamError::Other(e.to_string()))
    }
    fn max_packet(&self) -> u16 {
        crate::device::desc::HS_BULK_MPS
    }
}

// DeviceConfig is Clone (plain data); the descriptor set and the device
// take the same values.

#[cfg(test)]
mod tests {
    use super::usb_string_by_index;

    /// The STRING-descriptor selector: index 0 is the language-id
    /// descriptor, 1..=N the strings in table order (bring-up: serving
    /// the whole table for every index broke the host's GUID string
    /// read — the device listed as not openable).
    #[test]
    fn usb_string_by_index_walks_the_table() {
        // langid(4) + str1 "AB"(6) + str2 ""(2)
        let table = [4u8, 3, 9, 4, 6, 3, b'A', 0, b'B', 0, 2, 3];
        assert_eq!(usb_string_by_index(&table, 0), Some(&table[0..4]));
        assert_eq!(usb_string_by_index(&table, 1), Some(&table[4..10]));
        assert_eq!(usb_string_by_index(&table, 2), Some(&table[10..12]));
        assert_eq!(usb_string_by_index(&table, 3), None);
        assert_eq!(usb_string_by_index(&table, 255), None);
    }
}
