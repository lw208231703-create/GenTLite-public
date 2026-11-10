//! Transport-injected channel traits. `gentl-core` owns the module logic;
//! each transport (GigE GVCP, U3V USB control, ...) provides the wire behavior.

use crate::error::GentlResult;

/// Register/memory control channel to a remote device.
///
/// Addresses and sizes follow the transport's rules (GigE: 32-bit aligned,
/// multiples of 4 bytes — enforced by implementations, not here).
pub trait ControlChannel: Send + Sync {
    /// Read `buf.len()` bytes starting at `addr`. `buf.len()` is a
    /// transport-legal size.
    fn read_mem(&self, addr: u64, buf: &mut [u8]) -> GentlResult<()>;

    /// Write `buf` starting at `addr`.
    fn write_mem(&self, addr: u64, buf: &[u8]) -> GentlResult<()>;

    /// Read one 32-bit register (convenience over `read_mem`).
    fn read_reg(&self, addr: u64) -> GentlResult<u32> {
        let mut buf = [0u8; 4];
        self.read_mem(addr, &mut buf)?;
        Ok(u32::from_be_bytes(buf))
    }

    /// Write one 32-bit register (convenience over `write_mem`).
    fn write_reg(&self, addr: u64, value: u32) -> GentlResult<()> {
        self.write_mem(addr, &value.to_be_bytes())
    }

    /// The URL of the device description (GenICam XML), as stored in the
    /// device's URL bootstrap register / descriptor.
    fn xml_url(&self) -> GentlResult<String>;
}
