use crate::server::gpu_controller::{fan_control::FanCurve, VENDOR_NVIDIA};
use amdgpu_sysfs::gpu_handle::{PerformanceLevel, PowerLevelKind};
use anyhow::Context;
use indexmap::IndexMap;
use lact_schema::{
    default_fan_curve,
    request::{ClockspeedType, SetClocksCommand},
    FanControlMode, PmfwOptions, ProfileRule,
};
use nix::unistd::getuid;
use notify::{RecommendedWatcher, Watcher};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::{
    cell::Cell,
    env, fs,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, time};
use tracing::{debug, error, info};

const FILE_NAME: &str = "config.yaml";
const DEFAULT_ADMIN_GROUPS: [&str; 2] = ["wheel", "sudo"];
/// Minimum amount of time between separate config reloads
const CONFIG_RELOAD_INTERVAL_MILLIS: u64 = 50;
/// Period when config changes are ignored after LACT itself has edited the config
const SELF_CONFIG_EDIT_PERIOD_MILLIS: u64 = 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub version: u64,
    pub daemon: Daemon,
    #[serde(default = "default_apply_settings_timer")]
    pub apply_settings_timer: u64,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    gpus: IndexMap<String, Gpu>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub profiles: IndexMap<Rc<str>, Profile>,
    #[serde(default)]
    pub current_profile: Option<Rc<str>>,
    #[serde(default)]
    pub auto_switch_profiles: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: Daemon::default(),
            apply_settings_timer: default_apply_settings_timer(),
            gpus: IndexMap::new(),
            profiles: IndexMap::new(),
            current_profile: None,
            auto_switch_profiles: false,
            version: 0,
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Daemon {
    pub log_level: String,
    pub admin_groups: Vec<String>,
    #[serde(default)]
    pub disable_clocks_cleanup: bool,
    pub tcp_listen_address: Option<String>,
    pub exporter_listen_address: Option<String>,
}

impl Default for Daemon {
    fn default() -> Self {
        Self {
            log_level: "info".to_owned(),
            admin_groups: DEFAULT_ADMIN_GROUPS.map(str::to_owned).to_vec(),
            disable_clocks_cleanup: false,
            tcp_listen_address: None,
            exporter_listen_address: None,
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub gpus: IndexMap<String, Gpu>,
    pub rule: Option<ProfileRule>,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Gpu {
    pub fan_control_enabled: bool,
    pub fan_control_settings: Option<FanControlSettings>,
    #[serde(default, skip_serializing_if = "PmfwOptions::is_empty")]
    pub pmfw_options: PmfwOptions,
    pub power_cap: Option<f64>,
    pub performance_level: Option<PerformanceLevel>,
    #[serde(default, flatten)]
    pub clocks_configuration: ClocksConfiguration,
    pub power_profile_mode_index: Option<u16>,
    /// Outer vector is for power profile components, inner vector is for the heuristics within a component
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_power_profile_mode_hueristics: Vec<Vec<Option<i32>>>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub power_states: IndexMap<PowerLevelKind, Vec<u8>>,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ClocksConfiguration {
    pub min_core_clock: Option<i32>,
    pub min_memory_clock: Option<i32>,
    pub min_voltage: Option<i32>,
    pub max_core_clock: Option<i32>,
    pub max_memory_clock: Option<i32>,
    pub max_voltage: Option<i32>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub gpu_clock_offsets: IndexMap<u32, i32>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub mem_clock_offsets: IndexMap<u32, i32>,
    pub voltage_offset: Option<i32>,
}

impl Gpu {
    pub fn is_core_clocks_used(&self) -> bool {
        self.clocks_configuration != ClocksConfiguration::default()
    }

    pub fn apply_clocks_command(&mut self, command: &SetClocksCommand) {
        let clocks = &mut self.clocks_configuration;
        let value = command.value;
        match command.r#type {
            ClockspeedType::MaxCoreClock => clocks.max_core_clock = value,
            ClockspeedType::MaxMemoryClock => clocks.max_memory_clock = value,
            ClockspeedType::MaxVoltage => clocks.max_voltage = value,
            ClockspeedType::MinCoreClock => clocks.min_core_clock = value,
            ClockspeedType::MinMemoryClock => clocks.min_memory_clock = value,
            ClockspeedType::MinVoltage => clocks.min_voltage = value,
            ClockspeedType::VoltageOffset => clocks.voltage_offset = value,
            ClockspeedType::GpuClockOffset(pstate) => match value {
                Some(value) => {
                    clocks.gpu_clock_offsets.insert(pstate, value);
                }
                None => {
                    clocks.gpu_clock_offsets.shift_remove(&pstate);
                }
            },
            ClockspeedType::MemClockOffset(pstate) => match value {
                Some(value) => {
                    clocks.mem_clock_offsets.insert(pstate, value);
                }
                None => {
                    clocks.mem_clock_offsets.shift_remove(&pstate);
                }
            },
            ClockspeedType::Reset => {
                *clocks = ClocksConfiguration::default();
                assert!(!self.is_core_clocks_used());
            }
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FanControlSettings {
    #[serde(default)]
    pub mode: FanControlMode,
    #[serde(default = "default_fan_static_speed")]
    pub static_speed: f64,
    pub temperature_key: String,
    pub interval_ms: u64,
    pub curve: FanCurve,
    pub spindown_delay_ms: Option<u64>,
    pub change_threshold: Option<u64>,
}

impl Default for FanControlSettings {
    fn default() -> Self {
        Self {
            mode: FanControlMode::default(),
            static_speed: default_fan_static_speed(),
            temperature_key: "edge".to_owned(),
            interval_ms: 500,
            curve: FanCurve(default_fan_curve()),
            spindown_delay_ms: None,
            change_threshold: None,
        }
    }
}

pub fn default_fan_static_speed() -> f64 {
    0.5
}

impl Config {
    pub fn load() -> anyhow::Result<Option<Self>> {
        let path = get_path();
        if path.exists() {
            let raw_config = fs::read_to_string(path).context("Could not open config file")?;
            let config =
                serde_yaml::from_str(&raw_config).context("Could not deserialize config")?;
            Ok(Some(config))
        } else {
            let parent = path.parent().unwrap();
            fs::create_dir_all(parent)?;
            Ok(None)
        }
    }

    pub fn save(&self, config_last_saved: &Cell<Instant>) -> anyhow::Result<()> {
        let path = get_path();
        debug!("saving config to {path:?}");
        let raw_config = serde_yaml::to_string(self)?;

        fs::write(path, raw_config).context("Could not write config")?;
        config_last_saved.set(Instant::now());

        Ok(())
    }

    pub fn load_or_create() -> anyhow::Result<Self> {
        if let Some(config) = Config::load()? {
            Ok(config)
        } else {
            let config = Config::default();
            config.save(&Cell::new(Instant::now()))?;
            Ok(config)
        }
    }

    pub fn migrate_versions(&mut self) {
        loop {
            let next_version = self.version + 1;
            match next_version {
                0 => unreachable!(),
                // Reset VRAM settings on Nvidia after new offset ratio logic
                1 => {
                    for (id, gpu) in &mut self.gpus {
                        if id.starts_with(VENDOR_NVIDIA) {
                            gpu.clocks_configuration.max_memory_clock = None;
                            gpu.clocks_configuration.min_memory_clock = None;
                        }
                    }

                    for profile in &mut self.profiles.values_mut() {
                        for (id, gpu) in &mut profile.gpus {
                            if id.starts_with(VENDOR_NVIDIA) {
                                gpu.clocks_configuration.max_memory_clock = None;
                                gpu.clocks_configuration.min_memory_clock = None;
                            }
                        }
                    }
                }
                2 => {
                    for (id, gpu) in &mut self.gpus {
                        if id.starts_with(VENDOR_NVIDIA) {
                            gpu.clocks_configuration.max_core_clock = None;
                            gpu.clocks_configuration.max_memory_clock = None;
                        }
                    }

                    for profile in &mut self.profiles.values_mut() {
                        for (id, gpu) in &mut profile.gpus {
                            if id.starts_with(VENDOR_NVIDIA) {
                                gpu.clocks_configuration.max_core_clock = None;
                                gpu.clocks_configuration.max_memory_clock = None;
                            }
                        }
                    }
                }
                _ => break,
            }
            info!("migrated config version {} to {next_version}", self.version);
            self.version = next_version;
        }
    }

    /// Gets the GPU configs according to the current profile. Returns an error if the current profile could not be found.
    pub fn gpus(&self) -> anyhow::Result<&IndexMap<String, Gpu>> {
        match &self.current_profile {
            Some(profile) => {
                let profile = self
                    .profiles
                    .get(profile)
                    .with_context(|| format!("Could not find profile '{profile}'"))?;
                Ok(&profile.gpus)
            }
            None => Ok(&self.gpus),
        }
    }

    /// Same as [`gpus`], but with a mutable reference
    pub fn gpus_mut(&mut self) -> anyhow::Result<&mut IndexMap<String, Gpu>> {
        match &self.current_profile {
            Some(profile) => {
                let profile = self
                    .profiles
                    .get_mut(profile)
                    .with_context(|| format!("Could not find profile '{profile}'"))?;
                Ok(&mut profile.gpus)
            }
            None => Ok(&mut self.gpus),
        }
    }

    /// Get a specific profile
    pub fn profile(&self, profile: &str) -> anyhow::Result<&Profile> {
        self.profiles
            .get(profile)
            .with_context(|| format!("Profile {profile} not found"))
    }

    /// Get the settings for "default" profile (aka no profile)
    pub fn default_profile(&self) -> Profile {
        Profile {
            gpus: self.gpus.clone(),
            rule: None,
        }
    }

    pub fn clear(&mut self) {
        self.gpus.clear();
        self.profiles.clear();
        self.current_profile = None;
    }
}

pub fn start_watcher(config_last_saved: Rc<Cell<Instant>>) -> mpsc::UnboundedReceiver<Config> {
    let (config_tx, config_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::channel(64);

    tokio::task::spawn_local(async move {
        let mut watcher =
            RecommendedWatcher::new(SenderEventHandler(event_tx), notify::Config::default())
                .expect("Could not create config file watcher");

        let config_path = get_path();
        let watch_path = config_path
            .parent()
            .expect("Config path always has a parent");
        watcher
            .watch(watch_path, notify::RecursiveMode::Recursive)
            .expect("Could not subscribe to config file changes");

        while let Some(res) = event_rx.recv().await {
            debug!("got config file event {res:?}");
            match res {
                Ok(event) => {
                    use notify::EventKind;

                    if let EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) =
                        event.kind
                    {
                        if config_last_saved.get().elapsed()
                            < Duration::from_millis(SELF_CONFIG_EDIT_PERIOD_MILLIS)
                        {
                            debug!("ignoring fs event after self-inflicted config change");
                            continue;
                        }

                        // Accumulate FS events, reload config only after a period has passed since the last event
                        debug!(
                            "waiting for {CONFIG_RELOAD_INTERVAL_MILLIS}ms before reloading config"
                        );
                        let timeout =
                            time::sleep(Duration::from_millis(CONFIG_RELOAD_INTERVAL_MILLIS));
                        tokio::pin!(timeout);

                        loop {
                            tokio::select! {
                               () = &mut timeout => {
                                   break;
                               }
                               Some(res) = event_rx.recv() => {
                                    match res {
                                        Ok(event) => {
                                            if let EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) = event.kind {
                                                debug!("got another fs event, resetting reload timer");
                                                timeout.as_mut().reset(time::Instant::now() + Duration::from_millis(CONFIG_RELOAD_INTERVAL_MILLIS));
                                            }
                                        }
                                        Err(err) => error!("filesystem event error: {err}")
                                    }
                               }
                            }
                        }

                        match Config::load() {
                            Ok(Some(new_config)) => config_tx.send(new_config).unwrap(),
                            Ok(None) => error!("config was removed!"),
                            Err(err) => {
                                error!("could not read config after it was changed: {err:#}");
                            }
                        }
                    }
                }
                Err(err) => error!("filesystem event error: {err}"),
            }
        }

        debug!("registered config file event listener at path {watch_path:?}");
    });

    config_rx
}

struct SenderEventHandler(mpsc::Sender<notify::Result<notify::Event>>);

impl notify::EventHandler for SenderEventHandler {
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        let _ = self.0.blocking_send(event);
    }
}

fn get_path() -> PathBuf {
    let uid = getuid();
    if uid.is_root() {
        PathBuf::from("/etc/lact").join(FILE_NAME)
    } else {
        let config_dir = PathBuf::from(env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
            let home = env::var("HOME").expect("$HOME variable is not set");
            format!("{home}/.config")
        }));
        config_dir.join("lact").join(FILE_NAME)
    }
}

fn default_apply_settings_timer() -> u64 {
    5
}

#[cfg(test)]
mod tests {
    use super::{ClocksConfiguration, Config, Daemon, FanControlSettings, Gpu};
    use crate::server::gpu_controller::fan_control::FanCurve;
    use indexmap::IndexMap;
    use insta::assert_yaml_snapshot;
    use lact_schema::{FanControlMode, PmfwOptions};

    #[test]
    fn serde_de_full() {
        let config = Config {
            daemon: Daemon::default(),
            gpus: [(
                "my-gpu-id".to_owned(),
                Gpu {
                    fan_control_enabled: true,
                    fan_control_settings: Some(FanControlSettings {
                        curve: FanCurve::default(),
                        temperature_key: "edge".to_owned(),
                        interval_ms: 500,
                        mode: FanControlMode::Curve,
                        static_speed: 0.5,
                        spindown_delay_ms: Some(5000),
                        change_threshold: Some(3),
                    }),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        let data = serde_yaml::to_string(&config).unwrap();
        let deserialized_config: Config = serde_yaml::from_str(&data).unwrap();
        assert_eq!(config, deserialized_config);
    }

    #[test]
    fn parse_doc() {
        let doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/CONFIG.md"));
        let example_config_start = doc
            .find("```yaml")
            .expect("Could not find example config start")
            + 7;
        let example_config_end = doc[example_config_start..]
            .find("```")
            .expect("Could not find example config end")
            + example_config_start;
        let example_config = &doc[example_config_start..example_config_end];

        let deserialized_config: Config = serde_yaml::from_str(example_config).unwrap();
        assert_yaml_snapshot!(deserialized_config);
    }

    #[test]
    fn clocks_configuration_applied() {
        let mut gpu = Gpu {
            fan_control_enabled: false,
            fan_control_settings: None,
            pmfw_options: PmfwOptions::default(),
            power_cap: None,
            performance_level: None,
            clocks_configuration: ClocksConfiguration::default(),
            power_profile_mode_index: None,
            custom_power_profile_mode_hueristics: vec![],
            power_states: IndexMap::new(),
        };

        assert!(!gpu.is_core_clocks_used());
        gpu.clocks_configuration.voltage_offset = Some(10);
        assert!(gpu.is_core_clocks_used());
    }

    #[test]
    fn migrate_versions() {
        let mut config = Config {
            version: 0,
            daemon: Daemon::default(),
            apply_settings_timer: 5,
            gpus: IndexMap::from([
                (
                    "10DE:2704-1462:5110-0000:09:00.0".to_owned(),
                    Gpu {
                        clocks_configuration: ClocksConfiguration {
                            max_core_clock: Some(3000),
                            max_memory_clock: Some(10_000),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                ),
                (
                    "1002:687F-1043:0555-0000:0b:00.0".to_owned(),
                    Gpu {
                        clocks_configuration: ClocksConfiguration {
                            max_core_clock: Some(1500),
                            max_memory_clock: Some(920),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                ),
            ]),
            profiles: IndexMap::new(),
            current_profile: None,
            auto_switch_profiles: false,
        };

        config.migrate_versions();

        assert_eq!(
            config
                .gpus
                .get("10DE:2704-1462:5110-0000:09:00.0")
                .unwrap()
                .clocks_configuration
                .max_core_clock,
            None,
        );
        assert_eq!(
            config
                .gpus
                .get("10DE:2704-1462:5110-0000:09:00.0")
                .unwrap()
                .clocks_configuration
                .max_memory_clock,
            None,
        );
        assert_eq!(
            config
                .gpus
                .get("1002:687F-1043:0555-0000:0b:00.0")
                .unwrap()
                .clocks_configuration
                .max_memory_clock,
            Some(920),
        );
    }
}
