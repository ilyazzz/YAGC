use crate::{
    config::{self, FanControlSettings},
    server::vulkan::get_vulkan_info,
};

use super::{fan_control::FanCurve, FanControlHandle, GpuController};
use amdgpu_sysfs::{
    gpu_handle::power_profile_mode::PowerProfileModesTable,
    hw_mon::{HwMon, Temperature},
};
use anyhow::{anyhow, Context};
use futures::future::LocalBoxFuture;
use lact_schema::{
    ClocksInfo, ClockspeedStats, DeviceInfo, DeviceStats, DrmInfo, DrmMemoryInfo, FanControlMode,
    FanStats, GpuPciInfo, LinkInfo, PmfwInfo, PowerStates, PowerStats, VoltageStats, VramStats,
};
use nvml_wrapper::{
    bitmasks::device::ThrottleReasons,
    enum_wrappers::device::{Clock, TemperatureSensor, TemperatureThreshold},
    Device, Nvml,
};
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    fmt::Write,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{select, sync::Notify, time::sleep};
use tracing::{debug, error, trace, warn};

pub struct NvidiaGpuController {
    pub nvml: Rc<Nvml>,
    pub pci_slot_id: String,
    pub pci_info: GpuPciInfo,
    pub sysfs_path: PathBuf,
    pub fan_control_handle: RefCell<Option<FanControlHandle>>,
}

impl NvidiaGpuController {
    fn device(&self) -> Device<'_> {
        self.nvml
            .device_by_pci_bus_id(self.pci_slot_id.as_str())
            .expect("Can no longer get device")
    }

    async fn start_curve_fan_control_task(
        &self,
        curve: FanCurve,
        settings: FanControlSettings,
    ) -> anyhow::Result<()> {
        // Stop existing task to re-apply new curve
        self.stop_fan_control().await?;

        let device = self.device();
        device
            .temperature(TemperatureSensor::Gpu)
            .context("Could not read temperature")?;

        let fan_count = device.num_fans().context("Could not read fan count")?;
        if fan_count == 0 {
            return Err(anyhow!("Device has no fans"));
        }

        let mut notify_guard = self
            .fan_control_handle
            .try_borrow_mut()
            .map_err(|err| anyhow!("Lock error: {err}"))?;

        let notify = Rc::new(Notify::new());
        let task_notify = notify.clone();

        let nvml = self.nvml.clone();
        let pci_slot_id = self.pci_slot_id.clone();
        debug!("spawning new fan control task");

        let handle = tokio::task::spawn_local(async move {
            let mut device = nvml
                .device_by_pci_bus_id(pci_slot_id.as_str())
                .expect("Can no longer get device");

            let mut last_pwm = (None, Instant::now());
            let mut last_temp = 0;

            let interval = Duration::from_millis(settings.interval_ms);
            let spindown_delay = Duration::from_millis(settings.spindown_delay_ms.unwrap_or(0));
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let change_threshold = settings.change_threshold.unwrap_or(0) as i32;

            loop {
                select! {
                    () = sleep(interval) => (),
                    () = task_notify.notified() => break,
                }

                #[allow(clippy::cast_possible_wrap)]
                let current_temp = device
                    .temperature(TemperatureSensor::Gpu)
                    .expect("Could not read temperature") as i32;

                if (last_temp - current_temp).abs() < change_threshold {
                    trace!("temperature changed from {last_temp}°C to {current_temp}°C, which is less than the {change_threshold}°C threshold, skipping speed adjustment");
                    continue;
                }

                let target_pwm = curve.pwm_at_temp(Temperature {
                    #[allow(clippy::cast_precision_loss)]
                    current: Some(current_temp as f32),
                    crit: None,
                    crit_hyst: None,
                });
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

                for fan in 0..fan_count {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    if let Err(err) =
                        device.set_fan_speed(fan, (f64::from(target_pwm) / 2.5) as u32)
                    {
                        error!("could not set fan speed: {err}, disabling fan control");
                        break;
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

    async fn stop_fan_control(&self) -> anyhow::Result<()> {
        let mut fail_on_error = false;

        let maybe_notify = self
            .fan_control_handle
            .try_borrow_mut()
            .map_err(|err| anyhow!("Lock error: {err}"))?
            .take();
        if let Some((notify, handle)) = maybe_notify {
            notify.notify_one();
            handle.await?;
            fail_on_error = true;
        }

        let mut device = self.device();
        let fan_count = device.num_fans().context("Could not get fan count")?;
        for i in 0..fan_count {
            if let Err(err) = device
                .set_default_fan_speed(i)
                .context("Could not reset fan control to default")
            {
                if fail_on_error {
                    return Err(err);
                }
                error!("{err:#?}");
            }
        }

        Ok(())
    }
}

impl GpuController for NvidiaGpuController {
    fn get_id(&self) -> anyhow::Result<String> {
        let GpuPciInfo {
            device_pci_info,
            subsystem_pci_info,
        } = &self.pci_info;

        Ok(format!(
            "{}:{}-{}:{}-{}",
            device_pci_info.vendor_id,
            device_pci_info.model_id,
            subsystem_pci_info.vendor_id,
            subsystem_pci_info.model_id,
            self.pci_slot_id
        ))
    }

    fn get_pci_info(&self) -> Option<&GpuPciInfo> {
        Some(&self.pci_info)
    }

    fn get_path(&self) -> &Path {
        &self.sysfs_path
    }

    fn get_info(&self) -> DeviceInfo {
        let device = self.device();

        let vulkan_info = match get_vulkan_info(
            &self.pci_info.device_pci_info.vendor_id,
            &self.pci_info.device_pci_info.model_id,
        ) {
            Ok(info) => Some(info),
            Err(err) => {
                warn!("could not load vulkan info: {err}");
                None
            }
        };

        DeviceInfo {
            pci_info: Some(Cow::Borrowed(&self.pci_info)),
            vulkan_info,
            driver: format!(
                "nvidia {}",
                self.nvml.sys_driver_version().unwrap_or_default()
            ), // NVML should always be "nvidia"
            vbios_version: device
                .vbios_version()
                .map_err(|err| error!("could not get VBIOS version: {err}"))
                .ok(),
            link_info: LinkInfo {
                current_width: device.current_pcie_link_width().map(|v| v.to_string()).ok(),
                current_speed: device
                    .pcie_link_speed()
                    .map(|v| {
                        let mut output = format!("{} GT/s", v / 1000);
                        if let Ok(gen) = device.current_pcie_link_gen() {
                            let _ = write!(output, " PCIe gen {gen}");
                        }
                        output
                    })
                    .ok(),
                max_width: device.max_pcie_link_width().map(|v| v.to_string()).ok(),
                max_speed: device
                    .max_pcie_link_speed()
                    .ok()
                    .and_then(|v| v.as_integer())
                    .map(|v| {
                        let mut output = format!("{} GT/s", v / 1000);
                        if let Ok(gen) = device.current_pcie_link_gen() {
                            let _ = write!(output, " PCIe gen {gen}");
                        }
                        output
                    }),
            },
            drm_info: Some(DrmInfo {
                device_name: device.name().ok(),
                pci_revision_id: None,
                family_name: device.architecture().map(|arch| arch.to_string()).ok(),
                family_id: None,
                asic_name: None,
                chip_class: device.architecture().map(|arch| arch.to_string()).ok(),
                compute_units: None,
                cuda_cores: device.num_cores().ok(),
                vram_type: None,
                vram_clock_ratio: 1.0,
                vram_bit_width: device.current_pcie_link_width().ok(),
                vram_max_bw: None,
                l1_cache_per_cu: None,
                l2_cache: None,
                l3_cache_mb: None,
                memory_info: device
                    .bar1_memory_info()
                    .map(|info| DrmMemoryInfo {
                        cpu_accessible_used: info.used,
                        cpu_accessible_total: info.total,
                        resizeable_bar: None,
                    })
                    .ok(),
            }),
        }
    }

    fn hw_monitors(&self) -> &[HwMon] {
        &[]
    }

    fn get_pci_slot_name(&self) -> Option<String> {
        Some(self.pci_slot_id.clone())
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn get_stats(&self, gpu_config: Option<&config::Gpu>) -> DeviceStats {
        let device = self.device();

        let mut temps = HashMap::new();

        if let Ok(temp) = device.temperature(TemperatureSensor::Gpu) {
            let crit = device
                .temperature_threshold(TemperatureThreshold::Shutdown)
                .map(|value| value as f32)
                .ok();

            temps.insert(
                "GPU".to_owned(),
                Temperature {
                    current: Some(temp as f32),
                    crit,
                    crit_hyst: None,
                },
            );
        };

        let fan_settings = gpu_config.and_then(|config| config.fan_control_settings.as_ref());

        let pwm_current = if device.num_fans().is_ok_and(|num| num > 0) {
            device
                .fan_speed(0)
                .ok()
                .map(|value| (f64::from(value) * 2.55) as u8)
        } else {
            None
        };

        let vram = device
            .memory_info()
            .map(|info| VramStats {
                total: Some(info.total),
                used: Some(info.used),
            })
            .unwrap_or_default();

        DeviceStats {
            temps,
            fan: FanStats {
                control_enabled: gpu_config.is_some_and(|config| config.fan_control_enabled),
                control_mode: fan_settings.map(|settings| settings.mode),
                static_speed: fan_settings.map(|settings| settings.static_speed),
                curve: fan_settings.map(|settings| settings.curve.0.clone()),
                spindown_delay_ms: fan_settings.and_then(|settings| settings.spindown_delay_ms),
                change_threshold: fan_settings.and_then(|settings| settings.change_threshold),
                speed_current: None,
                speed_max: None,
                speed_min: None,
                pwm_current,
                pmfw_info: PmfwInfo::default(),
            },
            power: PowerStats {
                average: None,
                current: device.power_usage().map(|mw| f64::from(mw) / 1000.0).ok(),
                cap_current: device
                    .power_management_limit()
                    .map(|mw| f64::from(mw) / 1000.0)
                    .ok(),
                cap_max: device
                    .power_management_limit_constraints()
                    .map(|constraints| f64::from(constraints.max_limit) / 1000.0)
                    .ok(),
                cap_min: device
                    .power_management_limit_constraints()
                    .map(|constraints| f64::from(constraints.min_limit) / 1000.0)
                    .ok(),
                cap_default: device
                    .power_management_limit_default()
                    .map(|mw| f64::from(mw) / 1000.0)
                    .ok(),
            },
            busy_percent: device
                .utilization_rates()
                .map(|utilization| u8::try_from(utilization.gpu).expect("Invalid percentage"))
                .ok(),
            vram,
            clockspeed: ClockspeedStats {
                gpu_clockspeed: device.clock_info(Clock::Graphics).map(Into::into).ok(),
                vram_clockspeed: device.clock_info(Clock::Memory).map(Into::into).ok(),
                current_gfxclk: None,
            },
            throttle_info: device.current_throttle_reasons().ok().map(|reasons| {
                reasons
                    .iter()
                    .filter(|reason| *reason != ThrottleReasons::GPU_IDLE)
                    .map(|reason| {
                        let mut name = String::new();
                        bitflags::parser::to_writer(&reason, &mut name).unwrap();
                        (name, vec![])
                    })
                    .collect()
            }),
            voltage: VoltageStats::default(), // Voltage reporting is not supported
            performance_level: None,
            core_power_state: None,
            memory_power_state: None,
            pcie_power_state: None,
        }
    }

    fn get_clocks_info(&self) -> anyhow::Result<ClocksInfo> {
        Ok(ClocksInfo::default())
    }

    fn get_power_states(&self, _gpu_config: Option<&config::Gpu>) -> PowerStates {
        PowerStates::default()
    }

    fn get_power_profile_modes(&self) -> anyhow::Result<PowerProfileModesTable> {
        Err(anyhow!("Not supported on Nvidia"))
    }

    fn reset_pmfw_settings(&self) {}

    fn vbios_dump(&self) -> anyhow::Result<Vec<u8>> {
        Err(anyhow!("Not supported on Nvidia"))
    }

    fn apply_config<'a>(
        &'a self,
        config: &'a config::Gpu,
    ) -> LocalBoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async {
            let mut device = self.device();

            if let Some(cap) = config.power_cap {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let cap = (cap * 1000.0) as u32;

                let current_cap = device
                    .power_management_limit()
                    .context("Could not get current cap")?;

                if current_cap != cap {
                    debug!("setting power cap to {cap}");
                    device
                        .set_power_management_limit(cap)
                        .context("Could not set power cap")?;
                }
            } else {
                let current_cap = device.power_management_limit();
                let default_cap = device.power_management_limit_default();

                if let (Ok(current_cap), Ok(default_cap)) = (current_cap, default_cap) {
                    if current_cap != default_cap {
                        debug!("resetting power cap to {default_cap}");
                        device
                            .set_power_management_limit(default_cap)
                            .context("Could not reset power cap")?;
                    }
                }
            }

            if config.fan_control_enabled {
                let settings = config
                    .fan_control_settings
                    .as_ref()
                    .context("Fan control enabled with no settings")?;
                match settings.mode {
                    FanControlMode::Static => {
                        self.stop_fan_control()
                            .await
                            .context("Could not reset fan control")?;

                        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                        let speed = (settings.static_speed * 100.0) as u32;

                        let fan_count = device.num_fans().context("Could not get fan count")?;
                        for fan in 0..fan_count {
                            device
                                .set_fan_speed(fan, speed)
                                .context("Could not reset fan speed to default")?;
                        }
                    }
                    FanControlMode::Curve => {
                        self.start_curve_fan_control_task(settings.curve.clone(), settings.clone())
                            .await?;
                    }
                }
            } else {
                self.stop_fan_control()
                    .await
                    .context("Could not reset fan control")?;
            }

            Ok(())
        })
    }

    fn cleanup_clocks(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
