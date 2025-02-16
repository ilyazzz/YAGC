mod drm;

use super::{CommonControllerInfo, GpuController};
use crate::{
    bindings::intel::{
        drm_i915_gem_memory_class_I915_MEMORY_CLASS_DEVICE,
        drm_xe_memory_class_DRM_XE_MEM_REGION_CLASS_VRAM, IntelDrm,
    },
    config,
    server::vulkan::get_vulkan_info,
};
use amdgpu_sysfs::{gpu_handle::power_profile_mode::PowerProfileModesTable, hw_mon::Temperature};
use anyhow::{anyhow, Context};
use futures::future::LocalBoxFuture;
use lact_schema::{
    ClocksInfo, ClocksTable, ClockspeedStats, DeviceInfo, DeviceStats, DrmInfo, DrmMemoryInfo,
    FanStats, IntelClocksTable, IntelDrmInfo, LinkInfo, PowerState, PowerStates, PowerStats,
    VoltageStats, VramStats,
};
use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap},
    fmt::{self, Display},
    fs,
    io::{BufRead, BufReader},
    os::{fd::AsRawFd, raw::c_int},
    path::{Path, PathBuf},
    rc::Rc,
    str::FromStr,
    time::Instant,
};
use tracing::{debug, error, info, trace, warn};

#[derive(Clone, Copy)]
enum DriverType {
    I915,
    Xe,
}

pub struct IntelGpuController {
    driver_type: DriverType,
    common: CommonControllerInfo,
    tile_gts: Vec<PathBuf>,
    hwmon_path: Option<PathBuf>,
    drm_file: fs::File,
    drm: Rc<IntelDrm>,
    last_gpu_busy: Cell<Option<(Instant, u64)>>,
    last_energy_value: Cell<Option<(Instant, u64)>>,
    initial_power_cap: Option<f64>,
}

impl IntelGpuController {
    pub fn new(common: CommonControllerInfo, drm: Rc<IntelDrm>) -> anyhow::Result<Self> {
        let driver_type = match common.driver.as_str() {
            "xe" => DriverType::Xe,
            "i915" => DriverType::I915,
            _ => unreachable!(),
        };

        let mut tile_gts = vec![];

        for entry in fs::read_dir(&common.sysfs_path)
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with("tile") {
                    for gt_entry in fs::read_dir(entry.path()).into_iter().flatten().flatten() {
                        if let Some(gt_name) = gt_entry.file_name().to_str() {
                            if gt_name.starts_with("gt") {
                                let gt_path = gt_entry
                                    .path()
                                    .strip_prefix(&common.sysfs_path)
                                    .unwrap()
                                    .to_owned();
                                debug!("initialized GT at '{}'", gt_path.display());
                                tile_gts.push(gt_path);
                            }
                        }
                    }
                }
            }
        }

        if !tile_gts.is_empty() {
            info!(
                "initialized {} gt at '{}'",
                tile_gts.len(),
                common.sysfs_path.display()
            );
        }
        let drm_file = if cfg!(not(test)) {
            let drm_path = format!("/dev/dri/by-path/pci-{}-render", common.pci_slot_name);
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(drm_path)
                .context("Could not open DRM file")?
        } else {
            fs::File::open("/dev/null").unwrap()
        };

        let hwmon_path = fs::read_dir(common.sysfs_path.join("hwmon"))
            .ok()
            .and_then(|mut read_dir| read_dir.next())
            .and_then(Result::ok)
            .map(|entry| entry.path());
        debug!("Initialized hwmon: {hwmon_path:?}");

        let mut controller = Self {
            common,
            driver_type,
            tile_gts,
            hwmon_path,
            drm_file,
            drm,
            last_gpu_busy: Cell::new(None),
            last_energy_value: Cell::new(None),
            initial_power_cap: None,
        };

        let stats = controller.get_stats(None);
        controller.initial_power_cap = stats.power.cap_current.filter(|cap| *cap != 0.0);

        Ok(controller)
    }

    #[allow(clippy::unused_self)]
    fn debugfs_path(&self) -> PathBuf {
        #[cfg(test)]
        return PathBuf::from("/dev/null");

        #[cfg(not(test))]
        Path::new("/sys/kernel/debug/dri").join(&self.common.pci_slot_name)
    }

    fn first_tile_gt(&self) -> Option<&Path> {
        self.tile_gts.first().map(PathBuf::as_ref)
    }

    fn read_file<T>(&self, path: impl AsRef<Path>) -> Option<T>
    where
        T: FromStr,
        T::Err: Display,
    {
        let file_path = self.common.sysfs_path.join(path);

        trace!("reading file from '{}'", file_path.display());

        if file_path.exists() {
            match fs::read_to_string(&file_path) {
                Ok(contents) => match contents.trim().parse() {
                    Ok(value) => return Some(value),
                    Err(err) => {
                        error!(
                            "could not parse value from '{}': {err}",
                            file_path.display()
                        );
                    }
                },
                Err(err) => {
                    error!("could not read file at '{}': {err}", file_path.display());
                }
            }
        }
        None
    }

    fn write_file(&self, path: impl AsRef<Path>, contents: &str) -> anyhow::Result<()> {
        let file_path = self.common.sysfs_path.join(path);

        if file_path.exists() {
            fs::write(&file_path, contents)
                .with_context(|| format!("Could not write to '{}'", file_path.display()))?;
            Ok(())
        } else {
            Err(anyhow!("File '{}' does not exist", file_path.display()))
        }
    }

    fn read_hwmon_file<T>(&self, file_prefix: &str, file_suffix: &str) -> Option<T>
    where
        T: FromStr,
        T::Err: Display,
    {
        self.hwmon_path.as_ref().and_then(|hwmon_path| {
            let entries = fs::read_dir(hwmon_path).ok()?;
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(file_prefix) && name.ends_with(file_suffix) {
                        return self.read_file(entry.path());
                    }
                }
            }
            None
        })
    }

    fn write_hwmon_file(
        &self,
        file_prefix: &str,
        file_suffix: &str,
        contents: &str,
    ) -> anyhow::Result<()> {
        debug!("writing value '{contents}' to '{file_prefix}*{file_suffix}'");

        if let Some(hwmon_path) = &self.hwmon_path {
            let entries = fs::read_dir(hwmon_path)?;
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(file_prefix) && name.ends_with(file_suffix) {
                        return self.write_file(entry.path(), contents);
                    }
                }
            }

            Err(anyhow!("File not found"))
        } else {
            Err(anyhow!("No hwmon available"))
        }
    }

    fn get_drm_info_i915(&self) -> IntelDrmInfo {
        IntelDrmInfo {
            execution_units: self.drm_try(IntelDrm::drm_intel_get_eu_total),
            subslices: self.drm_try(IntelDrm::drm_intel_get_subslice_total),
        }
    }

    #[allow(clippy::unused_self)]
    fn get_drm_info_xe(&self) -> IntelDrmInfo {
        IntelDrmInfo {
            execution_units: None,
            subslices: None,
        }
    }

    #[cfg_attr(test, allow(unreachable_code, unused_variables))]
    fn drm_try<T: Default>(&self, f: unsafe fn(&IntelDrm, c_int, *mut T) -> c_int) -> Option<T> {
        #[cfg(test)]
        return None;

        unsafe {
            let mut out = T::default();
            let result = f(&self.drm, self.drm_file.as_raw_fd(), &mut out);
            if result == 0 {
                Some(out)
            } else {
                None
            }
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    fn get_busy_percent(&self) -> Option<u8> {
        let path = self.debugfs_path().join("gt0/rps_boost");
        let file = fs::File::open(path).ok()?;
        let mut lines = BufReader::new(file).lines();

        while let Some(Ok(line)) = lines.next() {
            if let Some(contents) = line.strip_prefix("GPU busy?") {
                let raw_value = contents
                    .split_ascii_whitespace()
                    .last()?
                    .strip_suffix("ms")?;
                let gpu_busy: u64 = raw_value.parse().ok()?;
                let timestamp = Instant::now();

                if let Some((last_timestamp, last_gpu_busy)) =
                    self.last_gpu_busy.replace(Some((timestamp, gpu_busy)))
                {
                    let time_delta = timestamp - last_timestamp;
                    let gpu_busy_delta = gpu_busy - last_gpu_busy;

                    let percentage =
                        (gpu_busy_delta as f64 / time_delta.as_millis() as f64) * 100.0;
                    return Some(percentage as u8);
                }
            }
        }

        None
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn get_power_usage(&self) -> Option<f64> {
        self.read_hwmon_file::<u64>("power", "_input")
            .or_else(|| {
                let energy = self.read_hwmon_file("energy", "_input")?;
                let timestamp = Instant::now();

                match self.last_energy_value.replace(Some((timestamp, energy))) {
                    Some((last_timestamp, last_energy)) => {
                        let time_delta = timestamp - last_timestamp;
                        let energy_delta = energy - last_energy;

                        Some(energy_delta / time_delta.as_millis() as u64 * 1000)
                    }
                    None => None,
                }
            })
            .map(|value| value as f64 / 1_000_000.0)
    }

    fn get_temperatures(&self) -> HashMap<String, Temperature> {
        self.read_hwmon_file::<f32>("temp", "_input")
            .into_iter()
            .map(|temp| {
                let key = "gpu".to_owned();
                let temperature = Temperature {
                    current: Some(temp / 1000.0),
                    crit: None,
                    crit_hyst: None,
                };
                (key, temperature)
            })
            .collect()
    }

    fn read_freq(&self, freq: FrequencyType) -> Option<u64> {
        self.freq_path(freq).and_then(|path| self.read_file(&path))
    }

    fn write_freq(&self, freq: FrequencyType, value: i32) -> anyhow::Result<()> {
        let path = self.freq_path(freq).context("Frequency info not found")?;
        self.write_file(path, &value.to_string())
            .context("Could not write frequency")?;
        Ok(())
    }

    fn freq_path(&self, freq: FrequencyType) -> Option<PathBuf> {
        let path = &self.common.sysfs_path;

        match self.driver_type {
            DriverType::I915 => {
                let card_path = path.parent().expect("Device has no parent path");

                let infix = match freq {
                    FrequencyType::Cur => "cur",
                    FrequencyType::Act => "act",
                    FrequencyType::Boost => "boost",
                    FrequencyType::Min => "min",
                    FrequencyType::Max => "max",
                    FrequencyType::Rp0 => "RP0",
                    FrequencyType::Rpe => "RP1",
                    FrequencyType::Rpn => "RPn",
                };
                Some(card_path.join(format!("gt_{infix}_freq_mhz")))
            }
            DriverType::Xe => match self.first_tile_gt() {
                Some(gt_path) => {
                    let prefix = match freq {
                        FrequencyType::Cur => "cur",
                        FrequencyType::Act => "act",
                        FrequencyType::Boost => return None,
                        FrequencyType::Min => "min",
                        FrequencyType::Max => "max",
                        FrequencyType::Rp0 => "rp0",
                        FrequencyType::Rpe => "rpe",
                        FrequencyType::Rpn => "rpn",
                    };
                    Some(gt_path.join("freq0").join(format!("{prefix}_freq")))
                }
                None => None,
            },
        }
    }

    fn get_throttle_info(&self) -> Option<BTreeMap<String, Vec<String>>> {
        let mut reasons = BTreeMap::new();

        match self.driver_type {
            DriverType::I915 => {
                let card_path = self
                    .common
                    .sysfs_path
                    .parent()
                    .expect("Device has no parent path");
                let gt_path = card_path.join("gt").join("gt0");
                let gt_files = fs::read_dir(gt_path).ok()?;
                for file in gt_files.flatten() {
                    if let Some(name) = file.file_name().to_str() {
                        if let Some(reason) = name.strip_prefix("throttle_reason_") {
                            if reason == "status" {
                                continue;
                            }

                            if let Some(value) = self.read_file::<i32>(file.path()) {
                                if value != 0 {
                                    reasons.insert(reason.to_owned(), vec![]);
                                }
                            }
                        }
                    }
                }
            }
            DriverType::Xe => {
                if let Some(tile) = self.first_tile_gt() {
                    let path = self.common.sysfs_path.join(tile).join("freq0/throttle");

                    let throttle_files = fs::read_dir(path).ok()?;
                    for file in throttle_files.flatten() {
                        if let Some(name) = file.file_name().to_str() {
                            if let Some(reason) = name.strip_prefix("reason_") {
                                if let Some(value) = self.read_file::<i32>(file.path()) {
                                    if value != 0 {
                                        reasons.insert(reason.to_owned(), vec![]);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Some(reasons)
    }

    fn get_vram_info(&self) -> IntelVramInfo {
        let mut total = 0;
        let mut used = 0;
        let mut cpu_accessible_total = 0;
        let mut cpu_accessible_used = 0;

        match self.driver_type {
            DriverType::I915 => {
                if let Ok(Some(query)) = drm::i915::query_memory_regions(&self.drm_file) {
                    let mut i915_unallocated = 0;
                    let mut cpu_unallocated = 0;

                    unsafe {
                        let regions = query.regions.as_slice(query.num_regions as usize);
                        for region_info in regions {
                            if u32::from(region_info.region.memory_class)
                                == drm_i915_gem_memory_class_I915_MEMORY_CLASS_DEVICE
                            {
                                total += region_info.probed_size;
                                i915_unallocated += region_info.unallocated_size;

                                let cpu_region_info = region_info.__bindgen_anon_1.__bindgen_anon_1;
                                if cpu_region_info.probed_cpu_visible_size > 0 {
                                    cpu_accessible_total += cpu_region_info.probed_cpu_visible_size;
                                    cpu_unallocated += cpu_region_info.unallocated_cpu_visible_size;
                                }
                            }
                        }
                    }

                    if total > 0 {
                        used = total - i915_unallocated;
                    }

                    if cpu_accessible_total > 0 {
                        cpu_accessible_used = cpu_accessible_total - cpu_unallocated;
                    }
                }
            }
            DriverType::Xe => {
                if let Ok(Some(query)) = drm::xe::query_mem_regions(&self.drm_file) {
                    unsafe {
                        let regions = query.mem_regions.as_slice(query.num_mem_regions as usize);
                        for region_info in regions {
                            if u32::from(region_info.mem_class)
                                == drm_xe_memory_class_DRM_XE_MEM_REGION_CLASS_VRAM
                            {
                                total += region_info.total_size;
                                used += region_info.used;

                                if region_info.cpu_visible_size > 0 {
                                    cpu_accessible_total += region_info.cpu_visible_size;
                                }
                            }
                        }
                    }
                }
            }
        }

        IntelVramInfo {
            total,
            used,
            mem_info: DrmMemoryInfo {
                cpu_accessible_used,
                cpu_accessible_total,
                resizeable_bar: Some(cpu_accessible_total == total),
            },
        }
    }
}

impl GpuController for IntelGpuController {
    fn controller_info(&self) -> &CommonControllerInfo {
        &self.common
    }

    fn get_info(&self, skip_vulkan: bool) -> DeviceInfo {
        let vulkan_info = if skip_vulkan {
            None
        } else {
            match get_vulkan_info(&self.common.pci_info) {
                Ok(info) => Some(info),
                Err(err) => {
                    warn!("could not load vulkan info: {err}");
                    None
                }
            }
        };

        let vram_info = self.get_vram_info();

        let drm_info = DrmInfo {
            intel: match self.driver_type {
                DriverType::I915 => self.get_drm_info_i915(),
                DriverType::Xe => self.get_drm_info_xe(),
            },
            vram_clock_ratio: 1.0,
            memory_info: Some(vram_info.mem_info),
            ..Default::default()
        };

        DeviceInfo {
            pci_info: Some(self.common.pci_info.clone()),
            vulkan_info,
            driver: self.common.driver.clone(),
            vbios_version: None,
            link_info: LinkInfo::default(),
            drm_info: Some(drm_info),
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn apply_config<'a>(
        &'a self,
        config: &'a config::Gpu,
    ) -> LocalBoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async {
            if let Some(max_clock) = config.clocks_configuration.max_core_clock {
                self.write_freq(FrequencyType::Max, max_clock)
                    .context("Could not set max clock")?;
            }

            if let Some(min_clock) = config.clocks_configuration.min_core_clock {
                self.write_freq(FrequencyType::Min, min_clock)
                    .context("Could not set min clock")?;
            }

            if let Some(cap) = config.power_cap {
                self.write_hwmon_file("power", "_max", &((cap * 1_000_000.0) as u64).to_string())
                    .context("Could not set power cap")?;
            }

            Ok(())
        })
    }

    fn get_stats(&self, _gpu_config: Option<&config::Gpu>) -> DeviceStats {
        let current_gfxclk = self.read_freq(FrequencyType::Cur);
        let gpu_clockspeed = self
            .read_freq(FrequencyType::Act)
            .filter(|value| *value != 0)
            .or(current_gfxclk);

        let clockspeed = ClockspeedStats {
            gpu_clockspeed,
            current_gfxclk,
            vram_clockspeed: None,
        };

        let cap_current = self
            .read_hwmon_file("power", "_max")
            .map(|value: f64| value / 1_000_000.0)
            .map(|cap| if cap == 0.0 { 100.0 } else { cap }); // Placeholder max value

        let power = PowerStats {
            average: None,
            current: self.get_power_usage(),
            cap_current,
            cap_min: Some(0.0),
            cap_max: self
                .read_hwmon_file::<f64>("power", "_rated_max")
                .filter(|max| *max != 0.0)
                .map(|cap| cap / 1_000_000.0)
                .or_else(|| cap_current.map(|current| current * 2.0)),
            cap_default: self.initial_power_cap,
        };

        let voltage = VoltageStats {
            gpu: self.read_hwmon_file("in", "_input"),
            northbridge: None,
        };

        let fan = FanStats {
            speed_current: self.read_hwmon_file("fan", "_input"),
            ..Default::default()
        };

        let vram_info = self.get_vram_info();
        let vram = VramStats {
            total: match vram_info.total {
                0 => None,
                total => Some(total),
            },
            used: match vram_info.used {
                0 => None,
                used => Some(used),
            },
        };

        DeviceStats {
            clockspeed,
            vram,
            busy_percent: self.get_busy_percent(),
            power,
            temps: self.get_temperatures(),
            voltage,
            throttle_info: self.get_throttle_info(),
            fan,
            ..Default::default()
        }
    }

    fn get_clocks_info(&self) -> anyhow::Result<ClocksInfo> {
        let clocks_table = IntelClocksTable {
            gt_freq: self
                .read_freq(FrequencyType::Min)
                .zip(self.read_freq(FrequencyType::Max)),
            rp0_freq: self.read_freq(FrequencyType::Rp0),
            rpe_freq: self.read_freq(FrequencyType::Rpe),
            rpn_freq: self.read_freq(FrequencyType::Rpn),
        };

        let table = if clocks_table == IntelClocksTable::default() {
            None
        } else {
            Some(ClocksTable::Intel(clocks_table))
        };

        Ok(ClocksInfo {
            table,
            ..Default::default()
        })
    }

    fn get_power_states(&self, _gpu_config: Option<&config::Gpu>) -> PowerStates {
        let core = [
            FrequencyType::Rpn,
            FrequencyType::Rpe,
            FrequencyType::Rp0,
            FrequencyType::Boost,
        ]
        .into_iter()
        .filter_map(|freq_type| {
            let value = self.read_freq(freq_type)?;
            Some(PowerState {
                enabled: true,
                min_value: None,
                value,
                index: None,
            })
        })
        .collect();

        PowerStates { core, vram: vec![] }
    }

    fn reset_pmfw_settings(&self) {}

    #[allow(clippy::cast_possible_truncation)]
    fn cleanup_clocks(&self) -> anyhow::Result<()> {
        if let Some(rp0) = self.read_freq(FrequencyType::Rp0) {
            if let Err(err) = self.write_freq(FrequencyType::Max, rp0 as i32) {
                warn!("could not reset max clock: {err:#}");
            }
        }

        if let Some(rpn) = self.read_freq(FrequencyType::Rpn) {
            if let Err(err) = self.write_freq(FrequencyType::Min, rpn as i32) {
                warn!("could not reset min clock: {err:#}");
            }
        }

        Ok(())
    }

    fn get_power_profile_modes(&self) -> anyhow::Result<PowerProfileModesTable> {
        Err(anyhow!("Not supported"))
    }

    fn vbios_dump(&self) -> anyhow::Result<Vec<u8>> {
        Err(anyhow!("Not supported"))
    }
}

#[derive(Clone, Copy)]
enum FrequencyType {
    Cur,
    Act,
    Boost,
    Min,
    Max,
    Rp0,
    Rpe,
    Rpn,
}

impl fmt::Display for FrequencyType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FrequencyType::Cur => "Current",
            FrequencyType::Act => "Actual",
            FrequencyType::Boost => "Boost",
            FrequencyType::Min => "Minimum",
            FrequencyType::Max => "Maximum",
            FrequencyType::Rp0 => "Maximum (RP0)",
            FrequencyType::Rpe => "Efficient (RPe)",
            FrequencyType::Rpn => "Minimum (RPn)",
        };
        s.fmt(f)
    }
}

struct IntelVramInfo {
    total: u64,
    used: u64,
    mem_info: DrmMemoryInfo,
}
