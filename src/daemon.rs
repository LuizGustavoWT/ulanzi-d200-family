use anyhow::Result;
use async_hid::AsyncHidRead;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::signal;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep_until};

use crate::config::WINDOW_MODE_STATUS;
use crate::config::WINDOW_MODE_CLOCK;
use crate::config::WINDOW_MODE_CLEAR;
use crate::config::Config;
use crate::device::{InputEvent, UlanziDevice};
use crate::openaction_client::BridgeEvent;
use crate::system_monitor::SystemMonitor;

/// How often to probe for a newly plugged (or newly accessible) Ulanzi device.
const DEVICE_RESCAN_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub enum HardwareEvent {
    KeyDown { device_id: String, key_index: u8 },
    KeyUp { device_id: String, key_index: u8 },
    EncoderRotate { device_id: String, position: u8, ticks: i16 },
    EncoderDown { device_id: String, position: u8 },
    EncoderUp { device_id: String, position: u8 },
    DeviceConnected { device_id: String },
    #[allow(dead_code)]
    DeviceDisconnected { device_id: String },
}

pub struct UlanziDaemon {
    devices: HashMap<String, UlanziDevice>,
    config: Config,
    system_monitor: SystemMonitor,
    cpu_usage: u8,
    mem_usage: u8,
    gpu_usage: u8,
    plugin_cmd_rx: Option<mpsc::Receiver<BridgeEvent>>,
    hw_event_tx: Option<mpsc::Sender<HardwareEvent>>,
    device_input_rx: mpsc::Receiver<(String, InputEvent)>,
    device_input_tx: mpsc::Sender<(String, InputEvent)>,
    // Debouncing & rate limiting
    flush_deadline: Option<Instant>,
    last_flush_time: Option<Instant>,
    debounce_delay: Duration,
    min_flush_interval: Duration,
    // Cycle command channel
    cycle_rx: mpsc::Receiver<()>,
}

impl UlanziDaemon {
    pub async fn new(
        config: Config,
        plugin_cmd_rx: Option<mpsc::Receiver<BridgeEvent>>,
        hw_event_tx: Option<mpsc::Sender<HardwareEvent>>,
        cycle_rx: mpsc::Receiver<()>,
    ) -> Result<Self> {
        let (device_input_tx, device_input_rx) = mpsc::channel(100);
        let mut devices = HashMap::new();

        // Try to connect to the first available device
        match UlanziDevice::connect().await {
            Ok(device) => {
                devices.insert(device.get_id().to_string(), device);
            }
            Err(e) => {
                warn!("No devices found at startup: {}", e);
            }
        }

        let system_monitor = SystemMonitor::new();

        Ok(Self {
            devices,
            config,
            system_monitor,
            cpu_usage: 0,
            mem_usage: 0,
            gpu_usage: 0,
            plugin_cmd_rx,
            hw_event_tx,
            device_input_rx,
            device_input_tx,
            flush_deadline: None,
            last_flush_time: None,
            debounce_delay: Duration::from_millis(30),
            min_flush_interval: Duration::from_millis(20),
            cycle_rx,
        })
    }

    /// Connect to the first available Ulanzi device, if any, and insert it.
    /// Returns `true` when a new device was added.
    async fn try_connect(&mut self) -> bool {
        // Already holding a device: do not reopen the HID handle just to
        // rediscover it every few seconds.
        if !self.devices.is_empty() {
            return false;
        }
        match UlanziDevice::connect().await {
            Ok(device) => {
                let id = device.get_id().to_string();
                if self.devices.contains_key(&id) {
                    return false;
                }
                self.devices.insert(id, device);
                info!("Ulanzi device connected");
                true
            }
            Err(e) => {
                debug!("No Ulanzi device available: {}", e);
                false
            }
        }
    }

    /// Apply brightness/label style/status window to a device, announce it to
    /// OpenDeck, and spawn its input reader task.
    async fn setup_device(&mut self, device_id: &str) {
        let Some(device) = self.devices.get_mut(device_id) else {
            return;
        };

        // 1. Reset the staged icons locally, but do NOT push an empty bundle.
        //    Blanking the whole grid here makes every key go black until
        //    OpenDeck's first image batch lands, which reads as a dead screen.
        //    The grid stays marked dirty, so the first real flush paints it.
        device.forget_all_images();

        // 2. Apply brightness and label style from config
        if let Err(e) = device.set_brightness(self.config.brightness).await {
            error!("Failed to set brightness for {}: {}", device.get_id(), e);
        }
        if let Ok(label_style) = serde_json::to_value(&self.config.label_style) {
            let _ = device.set_label_style(&label_style).await;
        }

        // 3. Start the small-window data with zeros
        let _ = device
            .set_small_window_data(self.config.display_mode, 0, 0, "", 0)
            .await;

        // 4. Notify plugins that a device is connected
        if let Some(ref tx) = self.hw_event_tx {
            let _ = tx
                .send(HardwareEvent::DeviceConnected {
                    device_id: device.get_id().to_string(),
                })
                .await;
        }

        // 5. Spawn reader task for button events
        if let Some(mut reader) = device.take_reader() {
            let tx = self.device_input_tx.clone();
            let device_id = device.get_id().to_string();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    match reader.read_input_report(&mut buf).await {
                        Ok(len) if len > 0 => {
                            if let Some(event) = UlanziDevice::parse_input(&buf[..len]) {
                                if tx.send((device_id.clone(), event)).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(_) => continue,
                        Err(e) => {
                            error!("Device {} read error: {}", device_id, e);
                            break;
                        }
                    }
                }
                info!("Reader task finished for {}", device_id);
            });
        }
    }

    /// Remove a device that stopped answering, telling OpenDeck it is gone.
    #[allow(dead_code)]
    async fn drop_device(&mut self, device_id: &str) {
        if self.devices.remove(device_id).is_none() {
            return;
        }
        warn!("Ulanzi device {} disconnected", device_id);
        if let Some(ref tx) = self.hw_event_tx {
            let _ = tx
                .send(HardwareEvent::DeviceDisconnected {
                    device_id: device_id.to_string(),
                })
                .await;
        }
    }

    pub async fn run(mut self) -> Result<()> {
        info!("Ulanzi Daemon started (debounced flush)");

        // --- Initial device setup for all connected devices ---
        for device_id in self.devices.keys().cloned().collect::<Vec<_>>() {
            self.setup_device(&device_id).await;
        }


        // --- Drain any initial plugin commands that arrived before the main loop ---
        let mut initial_commands = Vec::new();
        if let Some(rx) = &mut self.plugin_cmd_rx {
            while let Ok(cmd) = rx.try_recv() {
                initial_commands.push(cmd);
            }
        }
        if !initial_commands.is_empty() {
            info!(
                "Processing {} initial plugin commands",
                initial_commands.len()
            );
            for cmd in initial_commands {
                self.handle_plugin_command(cmd).await;
            }
            // Schedule a flush after the initial batch
            self.schedule_flush();
        }

        // --- Timers and shutdown signal ---
        let mut keep_alive_interval = interval(Duration::from_millis(100));
        let mut system_monitor_interval =
            interval(Duration::from_millis(self.config.stats_interval_ms));
        // Poll for the device so that plugging it in (or granting access to an
        // already plugged one) does not require restarting OpenDeck.
        let mut device_rescan_interval = interval(DEVICE_RESCAN_INTERVAL);
        device_rescan_interval.tick().await;

        let shutdown = async {
            #[cfg(unix)]
            {
                let mut sigint =
                    signal::unix::signal(signal::unix::SignalKind::interrupt()).unwrap();
                let mut sigterm =
                    signal::unix::signal(signal::unix::SignalKind::terminate()).unwrap();
                tokio::select! {
                    _ = sigint.recv() => info!("Received SIGINT, shutting down..."),
                    _ = sigterm.recv() => info!("Received SIGTERM, shutting down..."),
                }
            }
            #[cfg(windows)]
            {
                let _ = signal::ctrl_c().await;
                info!("Received Ctrl-C, shutting down...");
            }
        };
        tokio::pin!(shutdown);

        // --- Main event loop ---
        loop {
            // Copy the current deadline (if any) so the future doesn't borrow self.
            let deadline = self.flush_deadline;

            tokio::select! {
                _ = &mut shutdown => break,

                // Rescan for the device: pick up a newly plugged one, or one
                // whose permissions only just became accessible.
                _ = device_rescan_interval.tick() => {
                    if self.try_connect().await {
                        for device_id in self.devices.keys().cloned().collect::<Vec<_>>() {
                            // Only set up devices that have no reader yet, i.e.
                            // ones added by this very rescan.
                            let needs_setup = self
                                .devices
                                .get(&device_id)
                                .map(|d| d.has_reader())
                                .unwrap_or(false);
                            if needs_setup {
                                self.setup_device(&device_id).await;
                            }
                        }
                    }
                }

                // Handle WebSocket commands from OpenDeck plugin
                Some(cmd) = async {
                    if let Some(rx) = &mut self.plugin_cmd_rx {
                        rx.recv().await
                    } else {
                        std::future::pending::<Option<BridgeEvent>>().await
                    }
                } => {
                    // Collect all pending commands
                    let mut commands = vec![cmd];
                    if let Some(rx) = &mut self.plugin_cmd_rx {
                        while let Ok(c) = rx.try_recv() {
                            commands.push(c);
                        }
                    }

                    for cmd in commands {
                        self.handle_plugin_command(cmd).await;
                    }
                    self.schedule_flush();
                }

                // Flush deadline reached (debounced)
                _ = async move {
                    if let Some(d) = deadline {
                        sleep_until(d.into()).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if deadline.is_some() => {
                    self.perform_flush().await;
                }

                // Keep‑alive: update small window with current stats
                _ = keep_alive_interval.tick() => {
                    use chrono::Local;
                    let now = Local::now();
                    let time_str = now.format("%H:%M:%S").to_string();
                    for device in self.devices.values() {
                        if let Err(e) = device.set_small_window_data(
                            self.config.display_mode,
                            self.cpu_usage,
                            self.mem_usage,
                            &time_str,
                            self.gpu_usage,
                        ).await {
                            debug!("Failed to send keep-alive to {}: {}", device.get_id(), e);
                        }
                    }
                }

                // Forward hardware button events to plugins
                Some((device_id, event)) = self.device_input_rx.recv() => {
                    self.handle_device_event(&device_id, event).await;
                }

                // Update system_monitor every `stats_interval_ms`
                _ = system_monitor_interval.tick() => {
                    let (cpu, mem, gpu) = self.system_monitor.get_metrics();
                    self.cpu_usage = cpu;
                    self.mem_usage = mem;
                    self.gpu_usage = gpu;
                }

                // Handle cycle command from action
                Some(()) = self.cycle_rx.recv() => {
                    self.cycle_small_window().await;
                }
            }
        }

        info!("Shutdown complete.");
        Ok(())
    }

    /// Schedule a flush after the debounce delay, respecting the minimum interval
    /// since the last flush.
    fn schedule_flush(&mut self) {
        let now = Instant::now();
        let mut deadline = now + self.debounce_delay;
        if let Some(last) = self.last_flush_time {
            let earliest = last + self.min_flush_interval;
            if deadline < earliest {
                deadline = earliest;
            }
        }
        self.flush_deadline = Some(deadline);
    }

    /// Perform the actual flush (send all staged button images to the device).
    /// Uses a timeout to avoid hanging, and logs errors without crashing.
    async fn perform_flush(&mut self) {
        self.flush_deadline = None;

        // No timeout here on purpose. Aborting a flush mid-transfer tears the
        // ZIP stream in half, and the firmware then ends up with a truncated
        // icon bundle: the screens go blank instead of showing the new page.
        // A slow transfer is far less harmful than a corrupted one.
        let t0 = Instant::now();
        for device in self.devices.values() {
            if let Err(e) = device.flush().await {
                info!("Failed to flush device {}: {}", device.get_id(), e);
                // Continue with other devices (if any) – don't break.
            }
        }
        self.last_flush_time = Some(Instant::now());
        // At info level so a user can read real timings from the log and tell
        // whether the deck is slow to paint or the plugin is slow to send.
        info!("Flush completed in {:?}", t0.elapsed());
    }

    async fn handle_device_event(&mut self, device_id: &str, event: InputEvent) {
        debug!("Button event from {}: {:?}", device_id, event);
        let Some(tx) = &self.hw_event_tx else { return };
        let event = match event {
            InputEvent::Key { index, pressed } => if pressed {
                HardwareEvent::KeyDown { device_id: device_id.to_string(), key_index: index as u8 }
            } else { HardwareEvent::KeyUp { device_id: device_id.to_string(), key_index: index as u8 } },
            InputEvent::Encoder { position, ticks } => HardwareEvent::EncoderRotate { device_id: device_id.to_string(), position, ticks },
            InputEvent::EncoderPress { position, pressed } => if pressed {
                HardwareEvent::EncoderDown { device_id: device_id.to_string(), position }
            } else { HardwareEvent::EncoderUp { device_id: device_id.to_string(), position } },
            InputEvent::SideButton { index, pressed } => if pressed {
                HardwareEvent::KeyDown { device_id: device_id.to_string(), key_index: index as u8 }
            } else { HardwareEvent::KeyUp { device_id: device_id.to_string(), key_index: index as u8 } },
        };
        let _ = tx.send(event).await;
        return;
        /*
        if let Some(ref tx) = self.hw_event_tx {
            let outbound = if event.pressed {
                HardwareEvent::KeyDown {
                    device_id: device_id.to_string(),
                    key_index: event.index as u8,
                }
            } else {
                HardwareEvent::KeyUp {
                    device_id: device_id.to_string(),
                    key_index: event.index as u8,
                }
            };
            if let Err(e) = tx.send(outbound).await {
                warn!("Failed to broadcast hardware event: {}", e);
            }
        }
        */
    }

    async fn handle_plugin_command(&mut self, cmd: BridgeEvent) {
        match cmd {
            BridgeEvent::SetImage {
                device_id,
                controller,
                position,
                image_base64,
            } => {
                // Only keypad cells have a screen to paint. The D200X's three
                // encoders and its two side buttons have no display of their
                // own, so OpenDeck still sends us their icon but there is
                // nothing to write it to.
                if controller.as_deref() != Some("Keypad") {
                    debug!(
                        "Ignoring image for non-keypad controller {:?} position {}",
                        controller, position
                    );
                    return;
                }
                let dev = if let Some(d) = self.devices.get_mut(&device_id) {
                    Some(d)
                } else {
                    self.devices.values_mut().next()
                };
                if let Some(dev) = dev {
                    let index = position as usize;
                    debug!("Setting image for button {} on {}", index, dev.get_id());
                    match dev.set_button_image(index, &image_base64).await {
                        Ok(true) => {
                            debug!(
                                "SetImage: device={} position={} image_len={} (staged)",
                                dev.get_id(),
                                index,
                                image_base64.len()
                            );
                        }
                        Ok(false) => {
                            debug!("Image unchanged for button {}, skipping", index);
                        }
                        Err(e) => error!("Failed to set image: {}", e),
                    }
                } else {
                    warn!("SetImage: No target device found for {}", device_id);
                }
            }
            BridgeEvent::ClearImage {
                device_id,
                controller,
                position,
            } => {
                if controller.as_deref() != Some("Keypad") {
                    return;
                }
                let dev = if let Some(d) = self.devices.get_mut(&device_id) {
                    Some(d)
                } else {
                    self.devices.values_mut().next()
                };
                if let Some(dev) = dev {
                    let index = position as usize;
                    debug!(
                        "ClearImage: device={} position={} (staged)",
                        dev.get_id(),
                        index
                    );
                    dev.clear_button_image(index);
                    // A scene switch typically arrives as ClearImage followed by
                    // SetImage for the same slot. If a flush fired between the
                    // two, the device would receive a bundle with that slot
                    // blanked out and paint it empty for a moment. Give the
                    // replacement image time to land first.
                    self.schedule_flush();
                } else {
                    warn!("ClearImage: No target device found for {}", device_id);
                }
            }
            BridgeEvent::SetBrightness {
                device_id,
                brightness,
            } => {
                let dev = if let Some(d) = self.devices.get_mut(&device_id) {
                    Some(d)
                } else {
                    self.devices.values_mut().next()
                };
                if let Some(dev) = dev {
                    if let Err(e) = dev.set_brightness(brightness).await {
                        error!("Failed to set brightness: {}", e);
                    }
                } else {
                    warn!("SetBrightness: No target device found for {}", device_id);
                }
            }
            BridgeEvent::DeviceConnected(_) | BridgeEvent::DeviceDisconnected(_) => {}
        }
    }

    async fn cycle_small_window(&mut self) {
        let current = self.config.display_mode;
        let new_mode = match current {
            WINDOW_MODE_STATUS => WINDOW_MODE_CLOCK, // Status -> Clock
            WINDOW_MODE_CLOCK => WINDOW_MODE_CLEAR, // Clock -> Clear
            WINDOW_MODE_CLEAR => WINDOW_MODE_STATUS, // Clear -> Status
            _ => WINDOW_MODE_STATUS,
        };
        self.config.display_mode = new_mode;
        if let Err(e) = self.config.save() {
            warn!("Failed to save config: {}", e);
        }
        info!("Small‑window mode cycled: {:?} -> {:?}", current, new_mode);

        // Convert to u8 for device command
        let mode_byte = new_mode as u8;
        let now = chrono::Local::now();
        let time_str = now.format("%H:%M:%S").to_string();
        for device in self.devices.values() {
            if let Err(e) = device
                .set_small_window_data(
                    mode_byte,
                    self.cpu_usage,
                    self.mem_usage,
                    &time_str,
                    self.gpu_usage,
                )
                .await
            {
                warn!("Failed to update small window after mode cycle: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::ButtonEvent;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_handle_device_event_keydown() {
        let (hw_event_tx, mut hw_event_rx) = mpsc::channel(1);
        let config = Config::default();
        let (_device_input_tx, device_input_rx) = mpsc::channel(1);
        let (device_input_tx_internal, _device_input_rx_internal) = mpsc::channel(1);
        let (_cycle_tx, cycle_rx) = mpsc::channel(1);

        let mut daemon = UlanziDaemon {
            devices: HashMap::new(),
            config,
            system_monitor: SystemMonitor::new(),
            cpu_usage: 0,
            mem_usage: 0,
            gpu_usage: 0,
            plugin_cmd_rx: None,
            hw_event_tx: Some(hw_event_tx),
            device_input_rx,
            device_input_tx: device_input_tx_internal,
            flush_deadline: None,
            last_flush_time: None,
            debounce_delay: Duration::from_millis(50),
            min_flush_interval: Duration::from_millis(20),
            cycle_rx,
        };

        let event = ButtonEvent {
            index: 5,
            pressed: true,
            state: 1,
        };
        daemon.handle_device_event("test_device", InputEvent::Key { index: event.index, pressed: event.pressed }).await;

        let received = hw_event_rx.recv().await.unwrap();
        match received {
            HardwareEvent::KeyDown { device_id, key_index } => {
                assert_eq!(device_id, "test_device");
                assert_eq!(key_index, 5);
            }
            _ => panic!("Expected KeyDown event"),
        }
    }

    #[tokio::test]
    async fn test_handle_device_event_keyup() {
        let (hw_event_tx, mut hw_event_rx) = mpsc::channel(1);
        let config = Config::default();
        let (_device_input_tx, device_input_rx) = mpsc::channel(1);
        let (device_input_tx_internal, _device_input_rx_internal) = mpsc::channel(1);
        let (_cycle_tx, cycle_rx) = mpsc::channel(1);

        let mut daemon = UlanziDaemon {
            devices: HashMap::new(),
            config,
            system_monitor: SystemMonitor::new(),
            cpu_usage: 0,
            mem_usage: 0,
            gpu_usage: 0,
            plugin_cmd_rx: None,
            hw_event_tx: Some(hw_event_tx),
            device_input_rx,
            device_input_tx: device_input_tx_internal,
            flush_deadline: None,
            last_flush_time: None,
            debounce_delay: Duration::from_millis(50),
            min_flush_interval: Duration::from_millis(20),
            cycle_rx,
        };

        let event = ButtonEvent {
            index: 3,
            pressed: false,
            state: 0,
        };
        daemon.handle_device_event("test_device", InputEvent::Key { index: event.index, pressed: event.pressed }).await;

        let received = hw_event_rx.recv().await.unwrap();
        match received {
            HardwareEvent::KeyUp { device_id, key_index } => {
                assert_eq!(device_id, "test_device");
                assert_eq!(key_index, 3);
            }
            _ => panic!("Expected KeyUp event"),
        }
    }
}
