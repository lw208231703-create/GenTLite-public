//! GigE Vision message-channel device events (GenTL `EVENT_REMOTE_DEVICE`).
//!
//! The producer opens the device's message channel per GigE Vision 3.0 §12.2
//! (MCDA first, then MCP with the host port last; the transmission timeout is
//! set to 0 so the device clears the ACKNOWLEDGE bit per CR-103cd and no
//! EVENT_ACK is required). EVENT_CMD / EVENTDATA_CMD datagrams (§17.1/§17.2)
//! are decoded and handed to the GenTL consumer in the same record layout the
//! wire uses, so `EventGetDataInfo` can read the event id / payload back out
//! without another copy step.

use std::collections::HashMap;
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gentl_core::event_queue::{Entry, EventQueue};
use gentl_core::types::{EVENT_REMOTE_DEVICE, EventType};
use gentl_core::{GentlError, GentlResult, Module, ModuleType};
use gentl_gvcp::wire::decode_message_events;
use gentl_gvcp::GvcpClient;

/// One registered GenTL event module for `EVENT_REMOTE_DEVICE`.
pub struct DeviceEventModule {
    pub event_type: EventType,
    pub queue: Arc<EventQueue>,
}

impl Module for DeviceEventModule {
    fn module_type(&self) -> ModuleType {
        ModuleType::Event
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_arc(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> {
        self
    }
}

struct EventPump {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Device-level event bookkeeping on `GevDevice`: at most one module per
/// event type (GenTL 1.6 §6.3.7.1) and the shared message-channel pump.
#[derive(Default)]
pub struct DeviceEvents {
    /// Serializes the register/unregister/close lifecycle so the
    /// check-and-insert and the event-pump start/stop are one atomic step.
    lifecycle: Mutex<()>,
    modules: Arc<Mutex<HashMap<EventType, Arc<DeviceEventModule>>>>,
    pump: Mutex<Option<EventPump>>,
}

impl DeviceEvents {
    /// Register `EVENT_REMOTE_DEVICE`: opens the message channel when the
    /// device provides one (§36.22) and starts the receiver pump.
    pub fn register(
        &self,
        client: &Arc<Mutex<GvcpClient>>,
        camera_ip: Ipv4Addr,
        event_type: EventType,
    ) -> GentlResult<Arc<DeviceEventModule>> {
        if event_type != EVENT_REMOTE_DEVICE {
            return Err(GentlError::NotImplemented("device event type"));
        }
        // GenTL 6.3.7.1: one registration per module and event type. The
        // lifecycle guard keeps check+insert+start_pump atomic with respect
        // to a concurrent register/unregister.
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| GentlError::Io("events lifecycle lock poisoned".into()))?;
        let module = {
            let mut modules = self
                .modules
                .lock()
                .map_err(|_| GentlError::Io("events lock poisoned".into()))?;
            if modules.contains_key(&event_type) {
                return Err(GentlError::ResourceInUse);
            }
            let module = Arc::new(DeviceEventModule {
                event_type,
                queue: Arc::new(EventQueue::new()),
            });
            modules.insert(event_type, Arc::clone(&module));
            module
        };
        if let Err(e) = self.start_pump(client, camera_ip) {
            if let Ok(mut modules) = self.modules.lock() {
                modules.remove(&event_type);
            }
            return Err(e);
        }
        Ok(module)
    }

    /// Unregister one event type; the message channel closes once the pump
    /// stops (§12.4/CR-106ca: MCP first).
    pub fn unregister(
        &self,
        client: &Arc<Mutex<GvcpClient>>,
        event_type: EventType,
    ) -> GentlResult<()> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| GentlError::Io("events lifecycle lock poisoned".into()))?;
        let module = self
            .modules
            .lock()
            .map_err(|_| GentlError::Io("events lock poisoned".into()))?
            .remove(&event_type)
            .ok_or(GentlError::NotInitialized)?;
        module.queue.mark_dead();
        self.stop_pump(client);
        Ok(())
    }

    /// Stop the pump and drop every module (device close path).
    pub fn close(&self, client: &Arc<Mutex<GvcpClient>>) {
        let _lifecycle = match self.lifecycle.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        self.stop_pump(client);
        if let Ok(mut modules) = self.modules.lock() {
            for module in modules.values() {
                module.queue.mark_dead();
            }
            modules.clear();
        }
    }

    fn stop_pump(&self, client: &Arc<Mutex<GvcpClient>>) {
        let pump = self.pump.lock().ok().and_then(|mut p| p.take());
        if let Some(pump) = pump {
            if let Ok(client) = client.lock() {
                let _ = client.close_message_channel();
            }
            pump.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = pump.thread {
                let _ = thread.join();
            }
        }
    }

    fn start_pump(
        &self,
        client: &Arc<Mutex<GvcpClient>>,
        camera_ip: Ipv4Addr,
    ) -> GentlResult<()> {
        let mut guard = self
            .pump
            .lock()
            .map_err(|_| GentlError::Io("event pump lock poisoned".into()))?;
        if guard.is_some() {
            return Ok(());
        }
        let channels = {
            let client = client
                .lock()
                .map_err(|_| GentlError::Io("client lock poisoned".into()))?;
            client.num_message_channels()?
        };
        if channels == 0 {
            return Err(GentlError::NotAvailable);
        }
        // Resolve the routable host address with a throw-away connected
        // socket, then receive on an unconnected one: events arrive from the
        // device's own ephemeral source port (§12.2 CO-098cd), which a
        // connected socket would filter out.
        let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .map_err(|e| GentlError::Io(format!("message channel probe: {e}")))?;
        probe
            .connect((camera_ip, gentl_gvcp::wire::GVCP_PORT))
            .map_err(|e| GentlError::Io(format!("message channel probe: {e}")))?;
        let host_ip = match probe
            .local_addr()
            .map_err(|e| GentlError::Io(format!("message channel local addr: {e}")))?
            .ip()
        {
            std::net::IpAddr::V4(v4) => v4,
            std::net::IpAddr::V6(_) => {
                return Err(GentlError::NotAvailable);
            }
        };
        drop(probe);
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .map_err(|e| GentlError::Io(format!("message channel socket: {e}")))?;
        let host_port = socket
            .local_addr()
            .map_err(|e| GentlError::Io(format!("message channel local addr: {e}")))?
            .port();
        {
            let client = client
                .lock()
                .map_err(|_| GentlError::Io("client lock poisoned".into()))?;
            client.open_message_channel(host_ip, host_port)?;
        }
        let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let modules = Arc::clone(&self.modules);
        let name = format!("gev-message-{camera_ip}");
        let thread = thread::Builder::new()
            .name(name)
            .spawn(move || {
                let mut buf = [0u8; 65536];
                while !thread_stop.load(Ordering::Relaxed) {
                    match socket.recv(&mut buf) {
                        Ok(n) => {
                            let Some(events) = decode_message_events(&buf[..n]) else {
                                continue;
                            };
                            for event in events {
                                let payload = event.encode();
                                if let Ok(modules) = modules.lock() {
                                    for module in modules.values() {
                                        module.queue.push(Entry {
                                            id: u64::from(event.event_id),
                                            payload: payload.clone(),
                                        });
                                    }
                                }
                            }
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            continue
                        }
                        Err(_) => thread::sleep(Duration::from_millis(100)),
                    }
                }
            })
            .map_err(|e| GentlError::Io(format!("spawn message channel pump: {e}")))?;
        *guard = Some(EventPump {
            stop,
            thread: Some(thread),
        });
        Ok(())
    }
}
