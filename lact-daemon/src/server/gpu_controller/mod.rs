pub mod fan_control;

use self::fan_control::FanCurve;
use super::vulkan::get_vulkan_info;
use crate::config::{self, ClocksConfiguration};
use amdgpu_sysfs::{
    error::Error,
    gpu_handle::{
        fan_control::FanCurve as PmfwCurve,
        overdrive::{ClocksTable, ClocksTableGen},
        GpuHandle, PerformanceLevel, PowerLevelKind, PowerLevels,
    },
    hw_mon::{FanControlMethod, HwMon},
    sysfs::SysFS,
};
use anyhow::{anyhow, Context};
use lact_schema::{
    ClocksInfo, ClockspeedStats, DeviceInfo, DeviceStats, DrmInfo, FanStats, GpuPciInfo, LinkInfo,
    PciInfo, PmfwInfo, PowerState, PowerStates, PowerStats, VoltageStats, VramStats,
};
use pciid_parser::Database;
use std::{
    borrow::Cow,
    cell::RefCell,
    cmp,
    path::{Path, PathBuf},
    rc::Rc,
    str::FromStr,
    time::Duration,
};
use tokio::{
    select,
    sync::Notify,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{debug, error, trace, warn};
#[cfg(feature = "libdrm_amdgpu_sys")]
use {
    lact_schema::DrmMemoryInfo,
    libdrm_amdgpu_sys::AMDGPU::{DeviceHandle as DrmHandle, MetricsInfo, GPU_INFO},
    std::{fs::OpenOptions, os::fd::IntoRawFd},
};

type FanControlHandle = (Rc<Notify>, JoinHandle<()>);

const GPU_CLOCKDOWN_TIMEOUT_SECS: u64 = 3;

pub struct GpuController {
    pub(super) handle: GpuHandle,
    #[cfg(feature = "libdrm_amdgpu_sys")]
    pub drm_handle: Option<DrmHandle>,
    pub pci_info: Option<GpuPciInfo>,
    pub fan_control_handle: RefCell<Option<FanControlHandle>>,
}

impl GpuController {
    pub fn new_from_path(sysfs_path: PathBuf, pci_db: &Database) -> anyhow::Result<Self> {
        let handle = GpuHandle::new_from_path(sysfs_path)
            .map_err(|error| anyhow!("failed to initialize gpu handle: {error}"))?;

        #[cfg(feature = "libdrm_amdgpu_sys")]
        let drm_handle = match get_drm_handle(&handle) {
            Ok(handle) => Some(handle),
            Err(err) => {
                warn!("Could not get DRM handle: {err}");
                None
            }
        };

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
            #[cfg(feature = "libdrm_amdgpu_sys")]
            drm_handle,
            pci_info,
            fan_control_handle: RefCell::new(None),
        })
    }

    pub fn get_id(&self) -> anyhow::Result<String> {
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

    pub fn get_path(&self) -> &Path {
        self.handle.get_path()
    }

    fn first_hw_mon(&self) -> anyhow::Result<&HwMon> {
        self.handle
            .hw_monitors
            .first()
            .context("GPU has no hardware monitor")
    }

    pub fn get_info(&self) -> DeviceInfo {
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
        let driver = self.handle.get_driver();
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

    #[cfg(feature = "libdrm_amdgpu_sys")]
    fn get_full_vbios_version(&self) -> Option<String> {
        if let Some(drm_handle) = &self.drm_handle {
            if let Ok(vbios_info) = drm_handle.get_vbios_info() {
                return Some(format!("{} [{}]", vbios_info.ver, vbios_info.date));
            }
        }

        self.handle.get_vbios_version().ok()
    }

    #[cfg(not(feature = "libdrm_amdgpu_sys"))]
    fn get_full_vbios_version(&self) -> Option<String> {
        self.handle.get_vbios_version().ok()
    }

    #[cfg(feature = "libdrm_amdgpu_sys")]
    fn get_drm_info(&self) -> Option<DrmInfo> {
        trace!("Reading DRM info");
        let drm_handle = self.drm_handle.as_ref();

        let drm_memory_info =
            drm_handle
                .and_then(|handle| handle.memory_info().ok())
                .map(|memory_info| DrmMemoryInfo {
                    resizeable_bar: memory_info.check_resizable_bar(),
                    cpu_accessible_used: memory_info.cpu_accessible_vram.heap_usage,
                    cpu_accessible_total: memory_info.cpu_accessible_vram.total_heap_size,
                });

        drm_handle
            .and_then(|handle| handle.device_info().ok())
            .map(|drm_info| DrmInfo {
                family_name: drm_info.get_family_name().to_string(),
                family_id: drm_info.family_id(),
                asic_name: drm_info.get_asic_name().to_string(),
                chip_class: drm_info.get_chip_class().to_string(),
                compute_units: drm_info.cu_active_number,
                vram_type: drm_info.get_vram_type().to_string(),
                vram_bit_width: drm_info.vram_bit_width,
                vram_max_bw: drm_info.peak_memory_bw_gb().to_string(),
                l1_cache_per_cu: drm_info.get_l1_cache_size(),
                l2_cache: drm_info.calc_l2_cache_size(),
                l3_cache_mb: drm_info.calc_l3_cache_size_mb(),
                memory_info: drm_memory_info,
            })
    }

    #[cfg(not(feature = "libdrm_amdgpu_sys"))]
    fn get_drm_info(&self) -> Option<DrmInfo> {
        None
    }

    #[cfg(feature = "libdrm_amdgpu_sys")]
    fn get_current_gfxclk(&self) -> Option<u16> {
        self.drm_handle
            .as_ref()
            .and_then(|drm_handle| drm_handle.get_gpu_metrics().ok())
            .and_then(|metrics| metrics.get_current_gfxclk())
    }

    #[cfg(not(feature = "libdrm_amdgpu_sys"))]
    fn get_current_gfxclk(&self) -> Option<u16> {
        None
    }

    fn get_link_info(&self) -> LinkInfo {
        LinkInfo {
            current_width: self.handle.get_current_link_width().ok(),
            current_speed: self.handle.get_current_link_speed().ok(),
            max_width: self.handle.get_max_link_width().ok(),
            max_speed: self.handle.get_max_link_speed().ok(),
        }
    }

    pub fn get_stats(&self, gpu_config: Option<&config::Gpu>) -> DeviceStats {
        DeviceStats {
            fan: FanStats {
                control_enabled: gpu_config
                    .map(|config| config.fan_control_enabled)
                    .unwrap_or_default(),
                control_mode: gpu_config
                    .and_then(|config| config.fan_control_settings.as_ref())
                    .map(|settings| settings.mode),
                static_speed: gpu_config
                    .and_then(|config| config.fan_control_settings.as_ref())
                    .map(|settings| settings.static_speed),
                curve: gpu_config
                    .and_then(|config| config.fan_control_settings.as_ref())
                    .map(|settings| settings.curve.0.clone()),
                speed_current: self.hw_mon_and_then(HwMon::get_fan_current),
                speed_max: self.hw_mon_and_then(HwMon::get_fan_max),
                speed_min: self.hw_mon_and_then(HwMon::get_fan_min),
                pmfw_info: PmfwInfo {
                    acoustic_limit: self.handle.get_fan_acoustic_limit().ok(),
                    acoustic_target: self.handle.get_fan_acoustic_target().ok(),
                    target_temp: self.handle.get_fan_target_temperature().ok(),
                    minimum_pwm: self.handle.get_fan_minimum_pwm().ok(),
                },
                start_threshold: gpu_config
                    .and_then(|config| config.fan_control_settings.as_ref())
                    .map(|settings| settings.start_threshold),
                stop_threshold: gpu_config
                    .and_then(|config| config.fan_control_settings.as_ref())
                    .map(|settings| settings.stop_threshold),
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
        }
    }

    pub fn get_clocks_info(&self) -> anyhow::Result<ClocksInfo> {
        let clocks_table = self
            .handle
            .get_clocks_table()
            .context("Clocks table not available")?;
        Ok(clocks_table.into())
    }

    fn hw_mon_and_then<U>(&self, f: fn(&HwMon) -> Result<U, Error>) -> Option<U> {
        self.handle.hw_monitors.first().and_then(|mon| f(mon).ok())
    }

    fn hw_mon_map<U>(&self, f: fn(&HwMon) -> U) -> Option<U> {
        self.handle.hw_monitors.first().map(f)
    }

    async fn set_static_fan_control(&self, static_speed: f64) -> anyhow::Result<()> {
        // Stop existing task to set static speed
        self.stop_fan_control(false).await?;

        // Use PMFW curve functionality for static speed when it is available
        if let Ok(current_curve) = self.handle.get_fan_curve() {
            let allowed_ranges = current_curve.allowed_ranges.ok_or_else(|| {
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

            self.handle
                .set_fan_curve(&new_curve)
                .context("Could not set fan curve")?;

            Ok(())
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

            Ok(())
        }
    }

    async fn start_curve_fan_control(
        &self,
        curve: FanCurve,
        temp_key: String,
        interval: Duration,
        start_threshold: f64,
        stop_threshold: f64,
    ) -> anyhow::Result<()> {
        // Use the PMFW curve functionality when it is available
        // Otherwise, fall back to manual fan control via a task
        match self.handle.get_fan_curve() {
            Ok(current_curve) => {
                let new_curve = curve
                    .into_pmfw_curve(current_curve)
                    .context("Invalid fan curve")?;
                debug!("setting pmfw curve {new_curve:?}");

                self.handle
                    .set_fan_curve(&new_curve)
                    .context("Could not set fan curve")?;

                Ok(())
            }
            Err(_) => {
                self.start_curve_fan_control_task(
                    curve,
                    temp_key,
                    interval,
                    start_threshold,
                    stop_threshold,
                )
                .await
            }
        }
    }

    async fn start_curve_fan_control_task(
        &self,
        curve: FanCurve,
        temp_key: String,
        interval: Duration,
        start_threshold: f64,
        stop_threshold: f64,
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

        let handle = tokio::task::spawn_local(async move {
            let mut previous_pwm = 0;
            loop {
                select! {
                    () = sleep(interval) => (),
                    () = task_notify.notified() => break,
                }

                let mut temps = hw_mon.get_temps();
                let temp = temps
                    .remove(&temp_key)
                    .expect("Could not get temperature by given key");

                let target_pwm =
                    curve.pwm_at_temp(temp, start_threshold, stop_threshold, previous_pwm);
                trace!("fan control tick: setting pwm to {target_pwm}");

                if let Err(err) = hw_mon.set_fan_pwm(target_pwm) {
                    error!("could not set fan speed: {err}, disabling fan control");
                    break;
                }
                previous_pwm = target_pwm;
            }
            debug!("exited fan control task");
        });

        *notify_guard = Some((notify, handle));

        debug!(
            "started fan control with interval {}ms",
            interval.as_millis()
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

    pub fn get_power_states(&self, gpu_config: Option<&config::Gpu>) -> PowerStates {
        let core = self.get_power_states_kind(gpu_config, PowerLevelKind::CoreClock);
        let vram = self.get_power_states_kind(gpu_config, PowerLevelKind::MemoryClock);
        PowerStates { core, vram }
    }

    fn get_power_states_kind<T>(
        &self,
        gpu_config: Option<&config::Gpu>,
        kind: PowerLevelKind,
    ) -> Vec<PowerState<T>>
    where
        T: FromStr,
        <T as std::str::FromStr>::Err: std::fmt::Display,
    {
        let enabled_states = gpu_config.and_then(|gpu| gpu.power_states.get(&kind));
        let levels = self
            .handle
            .get_clock_levels::<T>(kind)
            .unwrap_or_else(|_| PowerLevels {
                levels: Vec::new(),
                active: None,
            })
            .levels;

        levels
            .into_iter()
            .enumerate()
            .map(|(i, value)| {
                let i = u8::try_from(i).unwrap();
                let enabled = enabled_states.map_or(true, |enabled| enabled.contains(&i));
                PowerState { enabled, value }
            })
            .collect()
    }

    pub fn reset_pmfw_settings(&self) {
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

    pub async fn apply_config(&self, config: &config::Gpu) -> anyhow::Result<()> {
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
                                warn!("GPU did not clock down after {GPU_CLOCKDOWN_TIMEOUT_SECS}");
                            }
                        }

                        original_performance_level = Some(performance_level);
                    }
                }
            }

            hw_mon.set_power_cap(cap)?;

            // Reapply old power level
            if let Some(level) = original_performance_level {
                self.handle
                    .set_power_force_performance_level(level)
                    .context("Could not reapply original performance level")?;
            }
        } else if let Ok(hw_mon) = self.first_hw_mon() {
            if let Ok(default_cap) = hw_mon.get_power_cap_default() {
                hw_mon.set_power_cap(default_cap)?;
            }
        }

        if let Some(level) = config.performance_level {
            self.handle.set_power_force_performance_level(level)?;
        } else if self.handle.get_power_force_performance_level().is_ok() {
            self.handle
                .set_power_force_performance_level(PerformanceLevel::Auto)?;
        }

        if let Some(mode_index) = config.power_profile_mode_index {
            if config.performance_level != Some(PerformanceLevel::Manual) {
                return Err(anyhow!(
                    "Performance level has to be set to `manual` to use power profile modes"
                ));
            }

            self.handle.set_active_power_profile_mode(mode_index)?;
        }

        // Reset the clocks table in case the settings get reverted back to not having a clocks value configured
        self.handle.reset_clocks_table().ok();

        if config.is_core_clocks_used() {
            let mut table = self.handle.get_clocks_table()?;
            config.clocks_configuration.apply_to_table(&mut table)?;

            debug!("writing clocks commands: {:#?}", table.get_commands()?);

            self.handle
                .set_clocks_table(&table)
                .context("Could not write clocks table")
                .with_context(|| format!("Clocks table commands: {:?}", table.get_commands()))?;
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
                        self.set_static_fan_control(settings.static_speed).await?;
                    }
                    lact_schema::FanControlMode::Curve => {
                        if settings.curve.0.is_empty() {
                            return Err(anyhow!("Cannot use empty fan curve"));
                        }

                        let interval = Duration::from_millis(settings.interval_ms);
                        self.start_curve_fan_control(
                            settings.curve.clone(),
                            settings.temperature_key.clone(),
                            interval,
                            settings.start_threshold,
                            settings.stop_threshold,
                        )
                        .await?;
                    }
                }
            } else {
                return Err(anyhow!(
                    "Trying to enable fan control with no settings provided"
                ));
            }
        } else {
            self.stop_fan_control(true).await?;

            let pmfw = &config.pmfw_options;
            if let Some(acoustic_limit) = pmfw.acoustic_limit {
                self.handle
                    .set_fan_acoustic_limit(acoustic_limit)
                    .context("Could not set acoustic limit")?;
            }
            if let Some(acoustic_target) = pmfw.acoustic_target {
                self.handle
                    .set_fan_acoustic_target(acoustic_target)
                    .context("Could not set acoustic target")?;
            }
            if let Some(target_temperature) = pmfw.target_temperature {
                self.handle
                    .set_fan_target_temperature(target_temperature)
                    .context("Could not set target temperature")?;
            }
            if let Some(minimum_pwm) = pmfw.minimum_pwm {
                self.handle
                    .set_fan_minimum_pwm(minimum_pwm)
                    .context("Could not set minimum pwm")?;
            }
        }

        Ok(())
    }
}

#[cfg(feature = "libdrm_amdgpu_sys")]
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
