//! Device Event Interface (DEI) pump and EVENT_REMOTE_DEVICE modules.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gentl_core::event_queue::{Entry, EventQueue};
use gentl_core::types::EventType;
use gentl_core::{GentlError, GentlResult, Module, ModuleType};

use crate::cti::U3vDevice;
use crate::packets;

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
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
pub struct DeviceEvents {
    modules: Arc<Mutex<HashMap<EventType, Arc<DeviceEventModule>>>>,
    pump: Mutex<Option<EventPump>>,
}

impl DeviceEvents {
    pub fn register(device: &Arc<U3vDevice>, event_type: EventType) -> GentlResult<Arc<DeviceEventModule>> {
        if event_type != gentl_core::types::EVENT_REMOTE_DEVICE {
            return Err(GentlError::NotImplemented("device event type"));
        }
        let module = {
            let mut modules = device.events.modules.lock().map_err(|_| GentlError::Io("events lock poisoned".into()))?;
            if modules.contains_key(&event_type) {
                return Err(GentlError::ResourceInUse);
            }
            let module = Arc::new(DeviceEventModule {
                event_type,
                queue: Arc::new(EventQueue::new()),
            });
            modules.insert(event_type, module.clone());
            module
        };
        if let Err(e) = Self::start_pump(device) {
            device.events.modules.lock().expect("events lock").remove(&event_type);
            return Err(e);
        }
        Ok(module)
    }

    fn start_pump(device: &Arc<U3vDevice>) -> GentlResult<()> {
        let mut guard = device.events.pump.lock().map_err(|_| GentlError::Io("event pump lock poisoned".into()))?;
        if guard.is_some() {
            return Ok(());
        }
        let pipe = device
            .ifaces
            .event_in
            .clone()
            .ok_or(GentlError::NotAvailable)?;
        let eirm = device.bootstrap.eirm_address.ok_or(GentlError::NotAvailable)?;
        device
            .client
            .lock()
            .map_err(|_| GentlError::Io("control client lock poisoned".into()))?
            .write_reg(eirm + packets::eirm::EI_CONTROL, 1)?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let modules = Arc::clone(&device.events.modules);
        let thread = match std::thread::Builder::new()
            .name(format!("u3v-dei-{}", device.id))
            .spawn(move || {
                let mut buf = vec![0u8; 4096];
                while !thread_stop.load(Ordering::Relaxed) {
                    match pipe.read_bulk(&mut buf, Duration::from_millis(100)) {
                        Ok(n) => {
                            if let Some(event) = packets::decode_event(&buf[..n as usize]) {
                                let entry = Entry {
                                    id: u64::from(event.event_id),
                                    payload: packets::encode_event(&event),
                                };
                                if let Ok(modules) = modules.lock() {
                                    for module in modules.values() {
                                        module.queue.push(Entry {
                                            id: entry.id,
                                            payload: entry.payload.clone(),
                                        });
                                    }
                                }
                            }
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(100)),
                    }
                }
            })
        {
            Ok(thread) => thread,
            Err(e) => {
                let _ = device.client.lock().map(|c| c.write_reg(eirm + packets::eirm::EI_CONTROL, 0));
                return Err(GentlError::Io(format!("spawn event pump: {e}")));
            }
        };
        *guard = Some(EventPump {
            stop,
            thread: Some(thread),
        });
        Ok(())
    }

    pub fn unregister(&self, event_type: EventType) -> GentlResult<()> {
        let module = self
            .modules
            .lock()
            .map_err(|_| GentlError::Io("events lock poisoned".into()))?
            .remove(&event_type)
            .ok_or(GentlError::NotInitialized)?;
        module.queue.mark_dead();
        Ok(())
    }

    pub fn close(&self, device: &U3vDevice) {
        if let Ok(modules) = self.modules.lock() {
            for module in modules.values() {
                module.queue.mark_dead();
            }
        }
        let pump = self.pump.lock().ok().and_then(|mut guard| guard.take());
        if let Some(mut pump) = pump {
            pump.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = pump.thread.take() {
                let _ = thread.join();
            }
            if let Some(eirm) = device.bootstrap.eirm_address {
                let _ = device.client.lock().map(|c| c.write_reg(eirm + packets::eirm::EI_CONTROL, 0));
            }
        }
    }
}
