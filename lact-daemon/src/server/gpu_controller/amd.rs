use super::{fan_control::FanCurve, FanControlHandle, GpuController};
use crate::{
    config::{self, ClocksConfiguration, FanControlSettings},
    server::vulkan::get_vulkan_info,
};
use amdgpu_sysfs::{
    error::Error,
    gpu_handle::{
        fan_control::FanCurve as PmfwCurve,
        overdrive::{ClocksTable, ClocksTableGen},
        power_profile_mode::PowerProfileModesTable,
        CommitHandle, GpuHandle, PerformanceLevel, PowerLevelKind, PowerLevels,
    },
    hw_mon::{FanControlMethod, HwMon},
    sysfs::SysFS,
};
use anyhow::{anyhow, Context};
use futures::future::LocalBoxFuture;
use lact_schema::{
    ClocksInfo, ClockspeedStats, DeviceInfo, DeviceStats, DrmInfo, FanStats, GpuPciInfo, LinkInfo,
    PciInfo, PmfwInfo, PowerState, PowerStates, PowerStats, VoltageStats, VramStats,
};
use libdrm_amdgpu_sys::AMDGPU::{ThrottleStatus, ThrottlerBit};
use pciid_parser::Database;
use std::{
    borrow::Cow,
    cell::RefCell,
    cmp,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};
use std::{collections::BTreeMap, fs, time::Instant};
use tokio::{
    select,
    sync::Notify,
    time::{sleep, timeout},
};
use tracing::{debug, error, info, trace, warn};

use {
    lact_schema::DrmMemoryInfo,
    libdrm_amdgpu_sys::AMDGPU::{DeviceHandle as DrmHandle, MetricsInfo, GPU_INFO},
    std::{fs::OpenOptions, os::fd::IntoRawFd},
};

const GPU_CLOCKDOWN_TIMEOUT_SECS: u64 = 3;
const MAX_PSTATE_READ_ATTEMPTS: u32 = 5;

pub struct AmdGpuController {
    handle: GpuHandle,
    drm_handle: Option<DrmHandle>,
    pci_info: Option<GpuPciInfo>,
    fan_control_handle: RefCell<Option<FanControlHandle>>,
}

impl AmdGpuController {
    pub fn new_from_path(sysfs_path: PathBuf, pci_db: &Database) -> anyhow::Result<Self> {
        let handle = GpuHandle::new_from_path(sysfs_path)
            .map_err(|error| anyhow!("failed to initialize gpu handle: {error}"))?;

        let mut drm_handle = None;
        if matches!(handle.get_driver(), "amdgpu" | "radeon") {
            match get_drm_handle(&handle) {
                Ok(handle) => {
                    drm_handle = Some(handle);
                }
                Err(err) => {
                    warn!("Could not get DRM handle: {err}");
                }
            }
        }

        let mut device_pci_info = None;
        let mut subsystem_pci_info = None;

        if let Some((vendor_id, model_id)) = handle.get_pci_id() {
            device_pci_info = Some(PciInfo {
                vendor_id: vendor_id.to_owned(),
                vendor: None,
                model_id: model_id.to_owned(),
                model: None,
            });

            if let Some((subsys_vendor_id, subsys_model_id)) = handle.get_pci_subsys_id() {
                let pci_device_info =
                    pci_db.get_device_info(vendor_id, model_id, subsys_vendor_id, subsys_model_id);

                device_pci_info = Some(PciInfo {
                    vendor_id: vendor_id.to_owned(),
                    vendor: pci_device_info.vendor_name.map(str::to_owned),
                    model_id: model_id.to_owned(),
                    model: pci_device_info.device_name.map(str::to_owned),
                });
                subsystem_pci_info = Some(PciInfo {
                    vendor_id: subsys_vendor_id.to_owned(),
                    vendor: pci_device_info.subvendor_name.map(str::to_owned),
                    model_id: subsys_model_id.to_owned(),
                    model: pci_device_info.subdevice_name.map(str::to_owned),
                });
            };
        }

        let pci_info = device_pci_info.and_then(|device_pci_info| {
            Some(GpuPciInfo {
                device_pci_info,
                subsystem_pci_info: subsystem_pci_info?,
            })
        });

        Ok(Self {
            handle,
            drm_handle,
            pci_info,
            fan_control_handle: RefCell::new(None),
        })
    }

    fn hw_mon_and_then<U>(&self, f: fn(&HwMon) -> Result<U, Error>) -> Option<U> {
        self.handle.hw_monitors.first().and_then(|mon| f(mon).ok())
    }

    fn hw_mon_map<U>(&self, f: fn(&HwMon) -> U) -> Option<U> {
        self.handle.hw_monitors.first().map(f)
    }

    async fn set_static_fan_control(
        &self,
        static_speed: f64,
    ) -> anyhow::Result<Option<CommitHandle>> {
        // Stop existing task to set static speed
        self.stop_fan_control(false).await?;

        // Use PMFW curve functionality for static speed when it is available
        if let Ok(current_curve) = self.handle.get_fan_curve() {
            let allowed_ranges = current_curve.allowed_ranges.clone().ok_or_else(|| {
                anyhow!("The GPU does not allow setting custom fan values (is overdrive enabled?)")
            })?;
            let min_temperature = allowed_ranges.temperature_range.start();
            let max_temperature = allowed_ranges.temperature_range.end();

            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let custom_pwm = (f64::from(*allowed_ranges.speed_range.end()) * static_speed) as u8;
            let static_pwm = cmp::max(*allowed_ranges.speed_range.start(), custom_pwm);

            let mut points = vec![(*min_temperature, static_pwm)];
            for _ in 1..current_curve.points.len() {
                points.push((*max_temperature, static_pwm));
            }

            let new_curve = PmfwCurve {
                points: points.into_boxed_slice(),
                allowed_ranges: Some(allowed_ranges),
            };

            debug!("setting static curve {new_curve:?}");

            let commit_handle = self
                .handle
                .set_fan_curve(&new_curve)
                .context("Could not set fan curve")?;

            Ok(Some(commit_handle))
        } else {
            let hw_mon = self
                .handle
                .hw_monitors
                .first()
                .cloned()
                .context("This GPU has no monitor")?;

            hw_mon
                .set_fan_control_method(FanControlMethod::Manual)
                .context("Could not set fan control method")?;

            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let static_pwm = (f64::from(u8::MAX) * static_speed) as u8;

            hw_mon
                .set_fan_pwm(static_pwm)
                .context("could not set fan speed")?;

            debug!("set fan speed to {}", static_speed);

            Ok(None)
        }
    }

    async fn start_curve_fan_control(
        &self,
        curve: FanCurve,
        settings: FanControlSettings,
    ) -> anyhow::Result<Option<CommitHandle>> {
        // Use the PMFW curve functionality when it is available
        // Otherwise, fall back to manual fan control via a task
        if let Ok(current_curve) = self.handle.get_fan_curve() {
            let new_curve = curve
                .into_pmfw_curve(current_curve.clone())
                .context("Invalid fan curve")?;

            debug!("setting pmfw curve {new_curve:?}");

            let commit_handle = self
                .handle
                .set_fan_curve(&new_curve)
                .context("Could not set fan curve")?;

            Ok(Some(commit_handle))
        } else {
            self.start_curve_fan_control_task(curve, settings).await?;
            Ok(None)
        }
    }

    async fn start_curve_fan_control_task(
        &self,
        curve: FanCurve,
        settings: FanControlSettings,
    ) -> anyhow::Result<()> {
        // Stop existing task to re-apply new curve
        self.stop_fan_control(false).await?;

        let hw_mon = self
            .handle
            .hw_monitors
            .first()
            .cloned()
            .context("This GPU has no monitor")?;
        hw_mon
            .set_fan_control_method(FanControlMethod::Manual)
            .context("Could not set fan control method")?;

        let mut notify_guard = self
            .fan_control_handle
            .try_borrow_mut()
            .map_err(|err| anyhow!("Lock error: {err}"))?;

        let notify = Rc::new(Notify::new());
        let task_notify = notify.clone();

        debug!("spawning new fan control task");
        let handle = tokio::task::spawn_local(async move {
            let mut last_pwm = (None, Instant::now());
            let mut last_temp = 0.0;

            // If the fan speed could was able to be set at least once
            let mut control_available = false;

            let temp_key = settings.temperature_key.clone();
            let interval = Duration::from_millis(settings.interval_ms);
            let spindown_delay = Duration::from_millis(settings.spindown_delay_ms.unwrap_or(0));
            #[allow(clippy::cast_precision_loss)]
            let change_threshold = settings.change_threshold.unwrap_or(0) as f32;

            loop {
                select! {
                    () = sleep(interval) => (),
                    () = task_notify.notified() => break,
                }

                let mut temps = hw_mon.get_temps();
                let temp = temps
                    .remove(&temp_key)
                    .expect("Could not get temperature by given key");

                let current_temp = temp.current.expect("Missing temp");

                if (last_temp - current_temp).abs() < change_threshold {
                    trace!("temperature changed from {last_temp}°C to {current_temp}°C, which is less than the {change_threshold}°C threshold, skipping speed adjustment");
                    continue;
                }

                let target_pwm = curve.pwm_at_temp(temp);
                let now = Instant::now();

                if let (Some(previous_pwm), previous_timestamp) = last_pwm {
                    let diff = now - previous_timestamp;
                    if target_pwm < previous_pwm && diff < spindown_delay {
                        trace!(
                            "delaying fan spindown ({}ms left)",
                            (spindown_delay - diff).as_millis()
                        );
                        continue;
                    }
                }

                last_pwm = (Some(target_pwm), now);
                last_temp = current_temp;

                trace!("fan control tick: setting pwm to {target_pwm}");

                match hw_mon.set_fan_pwm(target_pwm) {
                    Ok(()) => control_available = true,
                    Err(err) => {
                        error!("could not set fan speed: {err}");
                        if control_available {
                            info!("fan control was previously available, assuming the error is temporary");
                        } else {
                            info!("disabling fan control");
                            break;
                        }
                    }
                }
            }
            debug!("exited fan control task");
        });

        *notify_guard = Some((notify, handle));

        debug!(
            "started fan control with interval {}ms",
            settings.interval_ms
        );

        Ok(())
    }

    async fn stop_fan_control(&self, reset_mode: bool) -> anyhow::Result<()> {
        let maybe_notify = self
            .fan_control_handle
            .try_borrow_mut()
            .map_err(|err| anyhow!("Lock error: {err}"))?
            .take();
        if let Some((notify, handle)) = maybe_notify {
            notify.notify_one();
            handle.await?;
        }

        if reset_mode {
            if self.handle.get_fan_curve().is_ok() {
                if let Err(err) = self.handle.reset_fan_curve() {
                    warn!("could not reset fan curve: {err:#}");
                }
            }

            if let Some(hw_mon) = self.handle.hw_monitors.first().cloned() {
                if let Ok(current_control) = hw_mon.get_fan_control_method() {
                    if !matches!(current_control, FanControlMethod::Auto) {
                        hw_mon
                            .set_fan_control_method(FanControlMethod::Auto)
                            .context("Could not set fan control back to automatic")?;
                    }
                }
            }
        }

        Ok(())
    }

    fn get_power_states_kind(
        &self,
        gpu_config: Option<&config::Gpu>,
        kind: PowerLevelKind,
        attempt: u32,
    ) -> Vec<PowerState> {
        let enabled_states = gpu_config.and_then(|gpu| gpu.power_states.get(&kind));
        let levels = self
            .handle
            .get_clock_levels(kind)
            .unwrap_or_else(|_| PowerLevels {
                levels: Vec::new(),
                active: None,
            })
            .levels;

        if attempt < MAX_PSTATE_READ_ATTEMPTS
            && levels.iter().any(|value| *value >= u64::from(u16::MAX))
        {
            debug!("GPU reported nonsensical p-state value, retrying");
            return self.get_power_states_kind(gpu_config, kind, attempt + 1);
        }

        levels
            .into_iter()
            .enumerate()
            .map(|(i, value)| {
                let i = u8::try_from(i).unwrap();
                let enabled = enabled_states.map_or(true, |enabled| enabled.contains(&i));
                PowerState {
                    enabled,
                    min_value: None,
                    value,
                    index: Some(i),
                }
            })
            .collect()
    }

    fn first_hw_mon(&self) -> anyhow::Result<&HwMon> {
        self.handle
            .hw_monitors
            .first()
            .context("GPU has no hardware monitor")
    }

    fn get_current_gfxclk(&self) -> Option<u16> {
        self.drm_handle
            .as_ref()
            .and_then(|drm_handle| drm_handle.get_gpu_metrics().ok())
            .and_then(|metrics| metrics.get_current_gfxclk())
    }

    fn get_full_vbios_version(&self) -> Option<String> {
        if let Some(drm_handle) = &self.drm_handle {
            if let Ok(vbios_info) = drm_handle.get_vbios_info() {
                return Some(format!("{} [{}]", vbios_info.ver, vbios_info.date));
            }
        }

        self.handle.get_vbios_version().ok()
    }

    fn get_drm_info(&self) -> Option<DrmInfo> {
        use libdrm_amdgpu_sys::AMDGPU::VRAM_TYPE;

        trace!("Reading DRM info");
        let drm_handle = self.drm_handle.as_ref();

        let drm_memory_info =
            drm_handle
                .and_then(|handle| handle.memory_info().ok())
                .map(|memory_info| DrmMemoryInfo {
                    resizeable_bar: Some(memory_info.check_resizable_bar()),
                    cpu_accessible_used: memory_info.cpu_accessible_vram.heap_usage,
                    cpu_accessible_total: memory_info.cpu_accessible_vram.total_heap_size,
                });

        match drm_handle {
            Some(handle) => handle.device_info().ok().map(|drm_info| DrmInfo {
                device_name: drm_info.find_device_name(),
                pci_revision_id: Some(drm_info.pci_rev_id()),
                family_name: Some(drm_info.get_family_name().to_string()),
                family_id: Some(drm_info.family_id()),
                asic_name: Some(drm_info.get_asic_name().to_string()),
                chip_class: Some(drm_info.get_chip_class().to_string()),
                compute_units: Some(drm_info.cu_active_number),
                cuda_cores: None,
                vram_type: Some(drm_info.get_vram_type().to_string()),
                vram_clock_ratio: match drm_info.get_vram_type() {
                    VRAM_TYPE::GDDR6 => 2.0,
                    _ => 1.0,
                },
                vram_bit_width: Some(drm_info.vram_bit_width),
                vram_max_bw: Some(drm_info.peak_memory_bw_gb().to_string()),
                l1_cache_per_cu: Some(drm_info.get_l1_cache_size()),
                l2_cache: Some(drm_info.calc_l2_cache_size()),
                l3_cache_mb: Some(drm_info.calc_l3_cache_size_mb()),
                memory_info: drm_memory_info,
            }),
            None => None,
        }
    }

    fn get_link_info(&self) -> LinkInfo {
        LinkInfo {
            current_width: self.handle.get_current_link_width().ok(),
            current_speed: self.handle.get_current_link_speed().ok(),
            max_width: self.handle.get_max_link_width().ok(),
            max_speed: self.handle.get_max_link_speed().ok(),
        }
    }

    fn get_throttle_info(&self) -> Option<BTreeMap<String, Vec<String>>> {
        use libdrm_amdgpu_sys::AMDGPU::ThrottlerType;

        self.drm_handle
            .as_ref()
            .and_then(|drm_handle| drm_handle.get_gpu_metrics().ok())
            .and_then(|metrics| metrics.get_indep_throttle_status())
            .map(|throttle_value| {
                info!("Current throttle value: {throttle_value:X}");
                let mut grouped_bits: HashMap<ThrottlerType, HashSet<u8>> = HashMap::new();

                let throttle = ThrottleStatus::new(throttle_value);
                for bit in throttle.get_all_throttler() {
                    let throttle_type = ThrottlerType::from(bit);
                    grouped_bits
                        .entry(throttle_type)
                        .or_default()
                        .insert(bit as u8);
                }

                grouped_bits
                    .into_iter()
                    .map(|(throttle_type, bits)| {
                        let mut names: Vec<String> = bits
                            .into_iter()
                            .map(|bit| ThrottlerBit::from(bit).to_string())
                            .collect();
                        names.sort_unstable();
                        (throttle_type.to_string(), names)
                    })
                    .collect()
            })
    }

    fn debugfs_path(&self) -> Option<PathBuf> {
        let slot_id = self.handle.get_pci_slot_name()?;
        let name_search_term = format!("dev={slot_id}");

        for entry in fs::read_dir("/sys/kernel/debug/dri").ok()?.flatten() {
            let debugfs_path = entry.path();
            let name_file_path = debugfs_path.join("name");
            if name_file_path.exists() {
                if let Ok(contents) = fs::read_to_string(&name_file_path) {
                    if contents.contains(&name_search_term) {
                        return Some(debugfs_path);
                    }
                }
            }
        }

        None
    }
}

impl GpuController for AmdGpuController {
    fn get_id(&self) -> anyhow::Result<String> {
        let handle = &self.handle;
        let pci_id = handle.get_pci_id().context("Device has no vendor id")?;
        let pci_subsys_id = handle
            .get_pci_subsys_id()
            .context("Device has no subsys id")?;
        let pci_slot_name = handle
            .get_pci_slot_name()
            .context("Device has no pci slot")?;

        Ok(format!(
            "{}:{}-{}:{}-{}",
            pci_id.0, pci_id.1, pci_subsys_id.0, pci_subsys_id.1, pci_slot_name
        ))
    }

    fn get_pci_info(&self) -> Option<&GpuPciInfo> {
        self.pci_info.as_ref()
    }

    fn get_path(&self) -> &Path {
        self.handle.get_path()
    }

    fn get_info(&self) -> DeviceInfo {
        let vulkan_info = self.pci_info.as_ref().and_then(|pci_info| {
            match get_vulkan_info(
                &pci_info.device_pci_info.vendor_id,
                &pci_info.device_pci_info.model_id,
            ) {
                Ok(info) => Some(info),
                Err(err) => {
                    warn!("could not load vulkan info: {err}");
                    None
                }
            }
        });
        let pci_info = self.pci_info.as_ref().map(Cow::Borrowed);
        let driver = self.handle.get_driver().to_owned();
        let vbios_version = self.get_full_vbios_version();
        let link_info = self.get_link_info();
        let drm_info = self.get_drm_info();

        DeviceInfo {
            pci_info,
            vulkan_info,
            driver,
            vbios_version,
            link_info,
            drm_info,
        }
    }

    fn hw_monitors(&self) -> &[HwMon] {
        &self.handle.hw_monitors
    }

    fn get_pci_slot_name(&self) -> Option<String> {
        self.handle.get_pci_slot_name().map(str::to_owned)
    }

    fn get_stats(&self, gpu_config: Option<&config::Gpu>) -> DeviceStats {
        let fan_settings = gpu_config.and_then(|config| config.fan_control_settings.as_ref());
        DeviceStats {
            fan: FanStats {
                control_enabled: gpu_config.is_some_and(|config| config.fan_control_enabled),
                control_mode: fan_settings.map(|settings| settings.mode),
                static_speed: fan_settings.map(|settings| settings.static_speed),
                curve: fan_settings.map(|settings| settings.curve.0.clone()),
                spindown_delay_ms: fan_settings.and_then(|settings| settings.spindown_delay_ms),
                change_threshold: fan_settings.and_then(|settings| settings.change_threshold),
                speed_current: self.hw_mon_and_then(HwMon::get_fan_current),
                speed_max: self.hw_mon_and_then(HwMon::get_fan_max),
                speed_min: self.hw_mon_and_then(HwMon::get_fan_min),
                pwm_current: self.hw_mon_and_then(HwMon::get_fan_pwm),
                pmfw_info: PmfwInfo {
                    acoustic_limit: self.handle.get_fan_acoustic_limit().ok(),
                    acoustic_target: self.handle.get_fan_acoustic_target().ok(),
                    target_temp: self.handle.get_fan_target_temperature().ok(),
                    minimum_pwm: self.handle.get_fan_minimum_pwm().ok(),
                },
            },
            clockspeed: ClockspeedStats {
                gpu_clockspeed: self.hw_mon_and_then(HwMon::get_gpu_clockspeed),
                current_gfxclk: self.get_current_gfxclk(),
                vram_clockspeed: self.hw_mon_and_then(HwMon::get_vram_clockspeed),
            },
            voltage: VoltageStats {
                gpu: self.hw_mon_and_then(HwMon::get_gpu_voltage),
                northbridge: self.hw_mon_and_then(HwMon::get_northbridge_voltage),
            },
            vram: VramStats {
                total: self.handle.get_total_vram().ok(),
                used: self.handle.get_used_vram().ok(),
            },
            power: PowerStats {
                average: self.hw_mon_and_then(HwMon::get_power_average),
                current: self.hw_mon_and_then(HwMon::get_power_input),
                cap_current: self.hw_mon_and_then(HwMon::get_power_cap),
                cap_max: self.hw_mon_and_then(HwMon::get_power_cap_max),
                cap_min: self.hw_mon_and_then(HwMon::get_power_cap_min),
                cap_default: self.hw_mon_and_then(HwMon::get_power_cap_default),
            },
            temps: self.hw_mon_map(HwMon::get_temps).unwrap_or_default(),
            busy_percent: self.handle.get_busy_percent().ok(),
            performance_level: self.handle.get_power_force_performance_level().ok(),
            core_power_state: self
                .handle
                .get_core_clock_levels()
                .ok()
                .and_then(|levels| levels.active),
            memory_power_state: self
                .handle
                .get_memory_clock_levels()
                .ok()
                .and_then(|levels| levels.active),
            pcie_power_state: self
                .handle
                .get_pcie_clock_levels()
                .ok()
                .and_then(|levels| levels.active),
            throttle_info: self.get_throttle_info(),
        }
    }

    fn get_clocks_info(&self) -> anyhow::Result<ClocksInfo> {
        let clocks_table = self
            .handle
            .get_clocks_table()
            .context("Clocks table not available")?;
        Ok(clocks_table.into())
    }

    fn get_power_states(&self, gpu_config: Option<&config::Gpu>) -> PowerStates {
        let core = self.get_power_states_kind(gpu_config, PowerLevelKind::CoreClock, 0);
        let vram = self.get_power_states_kind(gpu_config, PowerLevelKind::MemoryClock, 0);
        PowerStates { core, vram }
    }

    fn get_power_profile_modes(&self) -> anyhow::Result<PowerProfileModesTable> {
        Ok(self.handle.get_power_profile_modes()?)
    }

    fn reset_pmfw_settings(&self) {
        let handle = &self.handle;
        if self.handle.get_fan_target_temperature().is_ok() {
            if let Err(err) = handle.reset_fan_target_temperature() {
                warn!("Could not reset target temperature: {err:#}");
            }
        }
        if self.handle.get_fan_acoustic_target().is_ok() {
            if let Err(err) = handle.reset_fan_acoustic_target() {
                warn!("Could not reset acoustic target: {err:#}");
            }
        }
        if self.handle.get_fan_acoustic_limit().is_ok() {
            if let Err(err) = handle.reset_fan_acoustic_limit() {
                warn!("Could not reset acoustic limit: {err:#}");
            }
        }
        if self.handle.get_fan_minimum_pwm().is_ok() {
            if let Err(err) = handle.reset_fan_minimum_pwm() {
                warn!("Could not reset minimum pwm: {err:#}");
            }
        }
    }

    fn vbios_dump(&self) -> anyhow::Result<Vec<u8>> {
        let debugfs = self.debugfs_path().context("DebugFS not found")?;
        fs::read(debugfs.join("amdgpu_vbios")).context("Could not read VBIOS file")
    }

    #[allow(clippy::too_many_lines)]
    fn apply_config<'a>(
        &'a self,
        config: &'a config::Gpu,
    ) -> LocalBoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async {
            if let Some(cap) = config.power_cap {
                let hw_mon = self.first_hw_mon()?;

                let current_usage = hw_mon
                    .get_power_input()
                    .or_else(|_| hw_mon.get_power_average())
                    .context("Could not get current power usage")?;

                // When applying a power limit that's lower than the current power consumption,
                // try to downclock the GPU first by forcing it into the lowest performance level.
                // Workaround for behaviour described in https://github.com/ilya-zlobintsev/LACT/issues/207
                let mut original_performance_level = None;
                if current_usage > cap {
                    if let Ok(performance_level) = self.handle.get_power_force_performance_level() {
                        if self
                            .handle
                            .set_power_force_performance_level(PerformanceLevel::Low)
                            .is_ok()
                        {
                            debug!(
                            "waiting for the GPU to clock down before applying a new power limit"
                        );

                            match timeout(
                                Duration::from_secs(GPU_CLOCKDOWN_TIMEOUT_SECS),
                                wait_until_lowest_clock_level(&self.handle),
                            )
                            .await
                            {
                                Ok(()) => {
                                    debug!("GPU clocked down successfully");
                                }
                                Err(_) => {
                                    warn!(
                                        "GPU did not clock down after {GPU_CLOCKDOWN_TIMEOUT_SECS}"
                                    );
                                }
                            }

                            original_performance_level = Some(performance_level);
                        }
                    }
                }

                // Due to possible driver bug, RX 7900 XTX really doesn't like when we set the same value again.
                // But, also in general we want to avoid setting same value twice
                if Ok(cap) != hw_mon.get_power_cap() {
                    hw_mon
                        .set_power_cap(cap)
                        .with_context(|| format!("Failed to set power cap: {cap}"))?;
                }

                // Reapply old power level
                if let Some(level) = original_performance_level {
                    self.handle
                        .set_power_force_performance_level(level)
                        .context("Could not reapply original performance level")?;
                }
            } else if let Ok(hw_mon) = self.first_hw_mon() {
                if let Ok(default_cap) = hw_mon.get_power_cap_default() {
                    // Due to possible driver bug, RX 7900 XTX really doesn't like when we set the same value again.
                    // But, also in general we want to avoid setting same value twice
                    if Ok(default_cap) != hw_mon.get_power_cap() {
                        hw_mon.set_power_cap(default_cap).with_context(|| {
                            format!("Failed to set power cap to default cap: {default_cap}")
                        })?;
                    }
                }
            }

            let mut commit_handles = Vec::new();

            // Reset the clocks table in case the settings get reverted back to not having a clocks value configured
            self.handle.reset_clocks_table().ok();

            // Reset performance level to work around some GPU quirks (found to be an issue on RDNA2)
            self.handle
                .set_power_force_performance_level(PerformanceLevel::Auto)
                .ok();

            if config.is_core_clocks_used() {
                let original_table = self
                    .handle
                    .get_clocks_table()
                    .context("Failed to get clocks table")?;
                let mut table = original_table.clone();
                config
                    .clocks_configuration
                    .apply_to_table(&mut table)
                    .context("Failed to apply clocks configuration to table")?;

                debug!(
                    "writing clocks commands: {:#?}",
                    table
                        .get_commands(&original_table)
                        .context("Failed to get table commands")?
                );

                let handle = self
                    .handle
                    .set_clocks_table(&table)
                    .context("Could not write clocks table")
                    .with_context(|| {
                        format!(
                            "Clocks table commands: {:?}",
                            table.get_commands(&original_table)
                        )
                    })?;
                commit_handles.push(handle);
            }

            if let Some(level) = config.performance_level {
                self.handle
                    .set_power_force_performance_level(level)
                    .context("Failed to set power performance level")?;
            }
            // Else is not needed, it was previously reset to auto already

            if let Some(mode_index) = config.power_profile_mode_index {
                if config.performance_level != Some(PerformanceLevel::Manual) {
                    return Err(anyhow!(
                        "Performance level has to be set to `manual` to use power profile modes"
                    ));
                }

                if config.custom_power_profile_mode_hueristics.is_empty() {
                    self.handle
                        .set_active_power_profile_mode(mode_index)
                        .context("Failed to set active power profile mode")?;
                } else {
                    self.handle
                        .set_custom_power_profile_mode_heuristics(
                            &config.custom_power_profile_mode_hueristics,
                        )
                        .context("Failed to set custom power profile mode heuristics")?;
                }
            }

            for (kind, states) in &config.power_states {
                if config.performance_level != Some(PerformanceLevel::Manual) {
                    return Err(anyhow!(
                        "Performance level has to be set to `manual` to configure power states"
                    ));
                }

                self.handle
                    .set_enabled_power_levels(*kind, states)
                    .with_context(|| format!("Could not set {kind:?} power states"))?;
            }

            if config.fan_control_enabled {
                if let Some(ref settings) = config.fan_control_settings {
                    match settings.mode {
                        lact_schema::FanControlMode::Static => {
                            if let Some(commit_handle) = self
                                .set_static_fan_control(settings.static_speed)
                                .await
                                .context("Failed to set static fan control")?
                            {
                                commit_handles.push(commit_handle);
                            }
                        }
                        lact_schema::FanControlMode::Curve => {
                            if settings.curve.0.is_empty() {
                                return Err(anyhow!("Cannot use empty fan curve"));
                            }

                            if let Some(commit_handle) = self
                                .start_curve_fan_control(settings.curve.clone(), settings.clone())
                                .await
                                .context("Failed to set curve fan control")?
                            {
                                commit_handles.push(commit_handle);
                            }
                        }
                    }
                } else {
                    return Err(anyhow!(
                        "Trying to enable fan control with no settings provided"
                    ));
                }
            } else {
                let pmfw = &config.pmfw_options;
                if let Some(acoustic_limit) = pmfw.acoustic_limit {
                    if self
                        .handle
                        .get_fan_acoustic_limit()
                        .context("Could not get acoustic limit")?
                        .current
                        != acoustic_limit
                    {
                        let commit_handle = self
                            .handle
                            .set_fan_acoustic_limit(acoustic_limit)
                            .context("Could not set acoustic limit")?;
                        commit_handles.push(commit_handle);
                    }
                }
                if let Some(acoustic_target) = pmfw.acoustic_target {
                    if self
                        .handle
                        .get_fan_acoustic_target()
                        .context("Could not get acoustic target")?
                        .current
                        != acoustic_target
                    {
                        let commit_handle = self
                            .handle
                            .set_fan_acoustic_target(acoustic_target)
                            .context("Could not set acoustic target")?;
                        commit_handles.push(commit_handle);
                    }
                }
                if let Some(target_temperature) = pmfw.target_temperature {
                    if self
                        .handle
                        .get_fan_target_temperature()
                        .context("Could not get target temperature")?
                        .current
                        != target_temperature
                    {
                        let commit_handle = self
                            .handle
                            .set_fan_target_temperature(target_temperature)
                            .context("Could not set target temperature")?;
                        commit_handles.push(commit_handle);
                    }
                }
                if let Some(minimum_pwm) = pmfw.minimum_pwm {
                    if self
                        .handle
                        .get_fan_minimum_pwm()
                        .context("Could not get minimum pwm")?
                        .current
                        != minimum_pwm
                    {
                        let commit_handle = self
                            .handle
                            .set_fan_minimum_pwm(minimum_pwm)
                            .context("Could not set minimum pwm")?;
                        commit_handles.push(commit_handle);
                    }
                }

                self.stop_fan_control(true)
                    .await
                    .context("Failed to stop fan control")?;
            }

            for handle in commit_handles {
                handle.commit()?;
            }

            Ok(())
        })
    }

    fn cleanup_clocks(&self) -> anyhow::Result<()> {
        if self.handle.get_clocks_table().is_err() {
            return Ok(());
        }

        self.handle.reset_clocks_table()?;

        Ok(())
    }
}

fn get_drm_handle(handle: &GpuHandle) -> anyhow::Result<DrmHandle> {
    let slot_name = handle
        .get_pci_slot_name()
        .context("Device has no PCI slot name")?;
    let path = format!("/dev/dri/by-path/pci-{slot_name}-render");
    let drm_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("Could not open drm file at {path}"))?;
    let (handle, _, _) = DrmHandle::init(drm_file.into_raw_fd())
        .map_err(|err| anyhow!("Could not open drm handle, error code {err}"))?;
    Ok(handle)
}

impl ClocksConfiguration {
    fn apply_to_table(&self, table: &mut ClocksTableGen) -> anyhow::Result<()> {
        if let ClocksTableGen::Vega20(ref mut table) = table {
            // Avoid writing settings to the clocks table except the user-specified ones
            // There is an issue on some GPU models where the default values are actually outside of the allowed range
            // See https://github.com/sibradzic/amdgpu-clocks/issues/32#issuecomment-829953519 (part 2) for an example

            if table.vddc_curve.is_empty() {
                table.clear();
            }

            // Normalize the VDDC curve - make sure all of the values are within the allowed range
            table.normalize_vddc_curve();

            match self.voltage_offset {
                Some(offset) => table.set_voltage_offset(offset)?,
                None => table.voltage_offset = None,
            }
        }

        if let Some(min_clockspeed) = self.min_core_clock {
            table.set_min_sclk(min_clockspeed)?;
        }
        if let Some(min_clockspeed) = self.min_memory_clock {
            table.set_min_mclk(min_clockspeed)?;
        }
        if let Some(min_voltage) = self.min_voltage {
            table.set_min_voltage(min_voltage)?;
        }

        if let Some(clockspeed) = self.max_core_clock {
            table.set_max_sclk(clockspeed)?;
        }
        if let Some(clockspeed) = self.max_memory_clock {
            table.set_max_mclk(clockspeed)?;
        }
        if let Some(voltage) = self.max_voltage {
            table.set_max_voltage(voltage)?;
        }

        Ok(())
    }
}

async fn wait_until_lowest_clock_level(handle: &GpuHandle) {
    loop {
        match handle.get_core_clock_levels() {
            Ok(levels) => {
                if levels.active == Some(0) {
                    break;
                }

                sleep(Duration::from_millis(250)).await;
            }
            Err(err) => {
                warn!("could not get core clock levels: {err}");
                break;
            }
        }
    }
}
