use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

#[cfg(nvml)]
use nvml_wrapper::Nvml;

pub struct SystemMonitor {
    sys: System,
    #[cfg(nvml)]
    nvml: Option<Nvml>,
}

impl SystemMonitor {
    pub fn new() -> Self {
        let mut sys = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything()),
        );
        sys.refresh_all();

        #[cfg(nvml)]
        let nvml = Nvml::init().ok();

        Self {
            sys,
            #[cfg(nvml)]
            nvml,
        }
    }

    /// Returns (CPU usage %, Memory usage %, GPU usage %)
    pub fn get_metrics(&mut self) -> (u8, u8, u8) {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();

        let cpu_usage = self.sys.global_cpu_usage() as u8;

        let total_mem = self.sys.total_memory();
        let used_mem = self.sys.used_memory();
        let mem_usage = if total_mem > 0 {
            ((used_mem as f64 / total_mem as f64) * 100.0) as u8
        } else {
            0
        };

        let gpu_usage = self.get_gpu_load();

        (cpu_usage, mem_usage, gpu_usage)
    }

    /// Returns the utilisation percentage of the first usable GPU.
    /// Supports NVIDIA (via NVML), AMD/Intel on Linux (via sysfs
    /// `gpu_busy_percent`) and any macOS GPU (via the IOKit accelerator).
    fn get_gpu_load(&self) -> u8 {
        // 1) Try NVIDIA GPUs through NVML
        #[cfg(nvml)]
        if let Some(nv) = &self.nvml {
            if let Ok(device_count) = nv.device_count() {
                for i in 0..device_count {
                    if let Ok(device) = nv.device_by_index(i) {
                        if let Ok(util) = device.utilization_rates() {
                            return util.gpu as u8;
                        }
                    }
                }
            }
        }

        #[cfg(target_os = "linux")]
        {
            if let Some(load) = Self::linux_drm_gpu_load() {
                return load;
            }
        }

        #[cfg(target_os = "macos")]
        {
            if let Some(load) = Self::macos_ioaccelerator_gpu_load() {
                return load;
            }
        }

        0
    }

    /// Scan `/sys/class/drm` for an AMD or Intel card exposing
    /// `gpu_busy_percent`.
    #[cfg(target_os = "linux")]
    fn linux_drm_gpu_load() -> Option<u8> {
        use std::fs;

        for entry in fs::read_dir("/sys/class/drm").ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Only consider top-level `cardN` entries
            if !(name.starts_with("card") && name.chars().skip(4).all(|c| c.is_ascii_digit())) {
                continue;
            }
            // Detect vendor via PCI vendor ID
            let vendor = fs::read_to_string(format!("/sys/class/drm/{}/device/vendor", name)).ok()?;
            // AMD or Intel -> try gpu_busy_percent
            if !matches!(vendor.trim(), "0x1002" | "0x8086") {
                continue;
            }
            let busy =
                fs::read_to_string(format!("/sys/class/drm/{}/device/gpu_busy_percent", name));
            if let Ok(load) = busy.ok()?.trim().parse::<f32>() {
                return Some(load as u8);
            }
        }
        None
    }

    /// Read `Device Utilization %` out of the IOKit accelerator's
    /// `PerformanceStatistics` dictionary. This covers both Apple Silicon and
    /// the discrete/integrated GPUs of Intel Macs, and needs no entitlement.
    ///
    /// `ioreg` is part of the base system, so shelling out avoids pulling in an
    /// IOKit binding just for a single integer.
    #[cfg(target_os = "macos")]
    fn macos_ioaccelerator_gpu_load() -> Option<u8> {
        use std::process::Command;

        let out = Command::new("/usr/sbin/ioreg")
            .args(["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);

        const KEY: &str = "\"Device Utilization %\"=";
        let rest = text.split(KEY).nth(1)?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<u8>().ok()
    }
}
