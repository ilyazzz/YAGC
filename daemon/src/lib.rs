pub mod config;
pub mod daemon_connection;
pub mod gpu_controller;

use config::{Config, GpuConfig};
use gpu_controller::{PowerProfile, oc_controller::{BasicClocksTable, BasicPowerLevel, OcController, OcControllerError, OldClocksTable}};
use pciid_parser::PciDatabase;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
};

use crate::gpu_controller::GpuController;

// Abstract socket allows anyone to connect without worrying about permissions
// https://unix.stackexchange.com/questions/579612/unix-domain-sockets-for-non-root-user
pub const SOCK_PATH: &str = "amdgpu-configurator.sock";
pub const BUFFER_SIZE: usize = 4096;

pub struct Daemon {
    gpu_controllers: HashMap<u32, GpuController>,
    listener: std::os::unix::io::RawFd,
    config: Config,
}

// u32 is the GPU id here
#[derive(Serialize, Deserialize, Debug)]
pub enum Action {
    CheckAlive,
    GetConfig,
    SetConfig(Config),
    GetGpus,
    GetInfo(u32),
    GetStats(u32),
    StartFanControl(u32),
    StopFanControl(u32),
    GetFanControl(u32),
    SetFanCurve(u32, BTreeMap<i64, f64>),
    SetPowerCap(u32, i64),
    SetPowerProfile(u32, PowerProfile),
    // SetGPUPowerState(u32, u32, i64, Option<i64>),
    //SetGPUMaxPowerState(u32, i64, Option<i64>),
    //SetVRAMMaxClock(u32, i64),
    GetOCController(u32),
    OcControllerOld(u32, OldOCControllerAction),
    OcControllerBasicGetTable(u32),
    OcControllerBasicSetGpuLevels(u32, BTreeMap<u32, BasicPowerLevel>),
    Shutdown,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum OldOCControllerAction {
    GetClocksTable,
    SetGPUPowerState(u32, i64, Option<i64>), // Gpu state id, clockspeed, voltage
    SetVRAMPowerState(u32, i64, Option<i64>),
    Commit,
    Reset,
}

impl Daemon {
    pub fn new(unprivileged: bool) -> Daemon {
        let addr = nix::sys::socket::SockAddr::Unix(
            nix::sys::socket::UnixAddr::new_abstract(SOCK_PATH.as_bytes()).unwrap(),
        );

        let listener = nix::sys::socket::socket(
            nix::sys::socket::AddressFamily::Unix,
            nix::sys::socket::SockType::Stream,
            nix::sys::socket::SockFlag::empty(),
            None,
        )
        .expect("Socket failed");
        nix::sys::socket::bind(listener, &addr).expect("Bind failed");
        nix::sys::socket::listen(listener, 128).expect("Listen failed");

        let config_path = PathBuf::from("/etc/lact.json");
        let mut config = if unprivileged {
            Config::new(&config_path)
        } else {
            match Config::read_from_file(&config_path) {
                Ok(c) => {
                    log::info!("Loaded config from {}", c.config_path.to_string_lossy());
                    c
                }
                Err(_) => {
                    log::info!("Config not found, creating");
                    let c = Config::new(&config_path);
                    //c.save().unwrap();
                    c
                }
            }
        };

        log::info!("Using config {:?}", config);

        let gpu_controllers = Self::load_gpu_controllers(&mut config);

        if !unprivileged {
            config.save().unwrap();
        }

        Daemon {
            listener,
            gpu_controllers,
            config,
        }
    }

    fn load_gpu_controllers(config: &mut Config) -> HashMap<u32, GpuController> {
        let pci_db = match config.allow_online_update {
            Some(true) => match Self::get_pci_db_online() {
                Ok(db) => Some(db),
                Err(e) => {
                    log::info!("Error updating PCI db: {:?}", e);
                    None
                }
            },
            Some(false) | None => None,
        };

        let mut gpu_controllers: HashMap<u32, GpuController> = HashMap::new();

        'entries: for entry in
            fs::read_dir("/sys/class/drm").expect("Could not open /sys/class/drm")
        {
            let entry = entry.unwrap();
            if entry.file_name().len() == 5 {
                if entry.file_name().to_str().unwrap().split_at(4).0 == "card" {
                    log::info!("Initializing {:?}", entry.path());

                    let mut controller =
                        GpuController::new(entry.path().join("device"), GpuConfig::new(), &pci_db);

                    let current_identifier = controller.get_identifier();

                    log::info!(
                        "Searching the config for GPU with identifier {:?}",
                        current_identifier
                    );

                    log::info!("{}", &config.gpu_configs.len());
                    for (id, (gpu_identifier, gpu_config)) in &config.gpu_configs {
                        log::info!("Comparing with {:?}", gpu_identifier);
                        if current_identifier == *gpu_identifier {
                            controller.load_config(&gpu_config);
                            gpu_controllers.insert(id.clone(), controller);
                            log::info!("already known");
                            continue 'entries;
                        }

                        /*if gpu_info.pci_slot == gpu_identifier.pci_id
                            && gpu_info.vendor_data.card_model == gpu_identifier.card_model
                            && gpu_info.vendor_data.gpu_model == gpu_identifier.gpu_model
                        {
                            controller.load_config(&gpu_config);
                            gpu_controllers.insert(id.clone(), controller);
                            log::info!("already known");
                            continue 'entries;
                        }*/
                    }

                    log::info!("initializing for the first time");

                    let id: u32 = random();

                    config
                        .gpu_configs
                        .insert(id, (controller.get_identifier(), controller.get_config()));
                    gpu_controllers.insert(id, controller);
                }
            }
        }

        gpu_controllers
    }

    fn get_pci_db_online() -> Result<PciDatabase, reqwest::Error> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("LACT")
            .build()?;
        let vendors = client
            .get("https://pci.endpoint.ml/devices.json")
            .send()?
            .json()?;
        Ok(PciDatabase { vendors })
    }

    pub fn listen(mut self) {
        loop {
            let stream = nix::sys::socket::accept(self.listener).expect("Accept failed");
            if stream < 0 {
                log::error!("Error from accept");
                break;
            } else {
                Daemon::handle_connection(&mut self, stream);
            }
        }
    }

    pub fn read_buffer(stream: i32) -> Vec<u8> {
        log::trace!("Reading buffer");
        let mut buffer = Vec::<u8>::new();
        buffer.resize(BUFFER_SIZE, 0);
        loop {
            match nix::unistd::read(stream, &mut buffer) {
                Ok(0) => {
                    break;
                }
                Ok(n) => {
                    assert!(n < buffer.len());
                    if n < buffer.len() {
                        buffer.resize(n, 0);
                    }
                    break;
                }
                Err(e) => {
                    panic!("Error reading from socket: {}", e);
                }
            }
        }

        buffer
    }

    fn handle_connection(&mut self, stream: i32) {
        let buffer = Self::read_buffer(stream);

        //log::trace!("finished reading, buffer size {}", buffer.len());
        log::trace!("Attempting to deserialize {:?}", &buffer);
        //log::trace!("{:?}", action);

        match bincode::deserialize::<Action>(&buffer) {
            Ok(action) => {
                log::trace!("Executing action {:?}", action);

                let response = self.execute_action(action);

                let buffer = bincode::serialize(&response).unwrap();

                log::trace!("Responding, buffer length {}", buffer.len());
                nix::unistd::write(stream, &buffer).expect("Writing response to socket failed");

                nix::sys::socket::shutdown(stream, nix::sys::socket::Shutdown::Both)
                    .expect("Failed to shut down");
                nix::unistd::close(stream).expect("Failed to close");

                log::trace!("Finished responding");
            }
            Err(_) => {
                println!("Failed deserializing action");
            }
        }
    }

    fn execute_action(&mut self, action: Action) -> Result<DaemonResponse, DaemonError> {
        match action {
            Action::CheckAlive => Ok(DaemonResponse::OK),
            Action::GetGpus => {
                let mut gpus: HashMap<u32, Option<String>> = HashMap::new();
                for (id, controller) in &self.gpu_controllers {
                    gpus.insert(*id, controller.get_info().vendor_data.gpu_model.clone());
                }
                Ok(DaemonResponse::Gpus(gpus))
            }
            Action::GetStats(i) => match self.gpu_controllers.get(&i) {
                Some(controller) => match controller.get_stats() {
                    Ok(stats) => Ok(DaemonResponse::GpuStats(stats)),
                    Err(_) => Err(DaemonError::HWMonError),
                },
                None => Err(DaemonError::InvalidID),
            },
            Action::GetInfo(i) => match self.gpu_controllers.get(&i) {
                Some(controller) => Ok(DaemonResponse::GpuInfo(controller.get_info().clone())),
                None => Err(DaemonError::InvalidID),
            },
            Action::StartFanControl(i) => match self.gpu_controllers.get_mut(&i) {
                Some(controller) => match controller.start_fan_control() {
                    Ok(_) => {
                        self.config
                            .gpu_configs
                            .insert(i, (controller.get_identifier(), controller.get_config()));
                        self.config.save().unwrap();
                        Ok(DaemonResponse::OK)
                    }
                    Err(_) => Err(DaemonError::HWMonError),
                },
                None => Err(DaemonError::InvalidID),
            },
            Action::StopFanControl(i) => match self.gpu_controllers.get_mut(&i) {
                Some(controller) => match controller.stop_fan_control() {
                    Ok(_) => {
                        self.config
                            .gpu_configs
                            .insert(i, (controller.get_identifier(), controller.get_config()));
                        self.config.save().unwrap();
                        Ok(DaemonResponse::OK)
                    }
                    Err(_) => Err(DaemonError::HWMonError),
                },
                None => Err(DaemonError::InvalidID),
            },
            Action::GetFanControl(i) => match self.gpu_controllers.get(&i) {
                Some(controller) => match controller.get_fan_control() {
                    Ok(info) => Ok(DaemonResponse::FanControlInfo(info)),
                    Err(_) => Err(DaemonError::HWMonError),
                },
                None => Err(DaemonError::InvalidID),
            },
            Action::SetFanCurve(i, curve) => match self.gpu_controllers.get_mut(&i) {
                Some(controller) => match controller.set_fan_curve(curve) {
                    Ok(_) => {
                        self.config
                            .gpu_configs
                            .insert(i, (controller.get_identifier(), controller.get_config()));
                        self.config.save().unwrap();
                        Ok(DaemonResponse::OK)
                    }
                    Err(_) => Err(DaemonError::HWMonError),
                },
                None => Err(DaemonError::InvalidID),
            },
            Action::SetPowerCap(i, cap) => match self.gpu_controllers.get_mut(&i) {
                Some(controller) => match controller.set_power_cap(cap) {
                    Ok(_) => {
                        self.config
                            .gpu_configs
                            .insert(i, (controller.get_identifier(), controller.get_config()));
                        self.config.save().unwrap();
                        Ok(DaemonResponse::OK)
                    }
                    Err(_) => Err(DaemonError::HWMonError),
                },
                None => Err(DaemonError::InvalidID),
            },
            // While mapping the types manually may not be too desirable, returning a full
            // controller with all the methods (that wouldn't work from a client) wouldn't make
            // sense either
            Action::GetOCController(i) => match self.gpu_controllers.get_mut(&i) {
                Some(controller) => match controller.oc_controller {
                    Some(OcController::New(_)) => Ok(DaemonResponse::OcControllerType(Some(
                        OcControllerType::New,
                    ))),
                    Some(OcController::Old(_)) => Ok(DaemonResponse::OcControllerType(Some(
                        OcControllerType::Old,
                    ))),
                    Some(OcController::Basic(_)) => Ok(DaemonResponse::OcControllerType(Some(
                        OcControllerType::Basic,
                    ))),
                    None => Ok(DaemonResponse::OcControllerType(None)),
                },
                None => Err(DaemonError::InvalidID),
            },
            Action::SetPowerProfile(i, profile) => match self.gpu_controllers.get_mut(&i) {
                Some(controller) => match controller.set_power_profile(profile) {
                    Ok(_) => {
                        self.config
                            .gpu_configs
                            .insert(i, (controller.get_identifier(), controller.get_config()));
                        self.config.save().unwrap();
                        Ok(DaemonResponse::OK)
                    }
                    Err(_) => Err(DaemonError::ControllerError),
                },
                None => Err(DaemonError::InvalidID),
            },
            /*Action::SetGPUPowerState(i, num, clockspeed, voltage) => {
                match self.gpu_controllers.get_mut(&i) {
                    Some(controller) => {
                        match controller.set_gpu_power_state(num, clockspeed, voltage) {
                            Ok(_) => {
                                self.config.gpu_configs.insert(
                                    i,
                                    (controller.get_identifier(), controller.get_config()),
                                );
                                self.config.save().unwrap();
                                Ok(DaemonResponse::OK)
                            }
                            Err(_) => Err(DaemonError::ControllerError),
                        }
                    }
                    None => Err(DaemonError::InvalidID),
                }
            }*/
            Action::Shutdown => {
                for (id, controller) in &mut self.gpu_controllers {
                    #[allow(unused_must_use)]
                    {
                        // TODO
                        //controller.reset_gpu_power_states();
                        //controller.commit_gpu_power_states();
                        controller.set_power_profile(PowerProfile::Auto);

                        if self
                            .config
                            .gpu_configs
                            .get(id)
                            .unwrap()
                            .1
                            .fan_control_enabled
                        {
                            controller.stop_fan_control();
                        }
                    }
                }
                std::process::exit(0);
            }
            Action::OcControllerOld(i, action) => match self.gpu_controllers.get_mut(&i) {
                Some(controller) => match &mut controller.oc_controller {
                    Some(OcController::Old(oc_controller)) => match action {
                        OldOCControllerAction::GetClocksTable => {
                            Ok(DaemonResponse::OldClocksTable(oc_controller.get_table()?))
                        }
                        OldOCControllerAction::SetGPUPowerState(num, clockspeed, voltage) => {
                            oc_controller.set_gpu_power_state(num, clockspeed, voltage)?;
                            Ok(DaemonResponse::OK)
                        }
                        OldOCControllerAction::SetVRAMPowerState(num, clockspeed, voltage) => {
                            oc_controller.set_vram_power_state(num, clockspeed, voltage)?;
                            Ok(DaemonResponse::OK)
                        }
                        OldOCControllerAction::Commit => {
                            oc_controller.commit_gpu_power_states()?;
                            Ok(DaemonResponse::OK)
                        }
                        OldOCControllerAction::Reset => {
                            oc_controller.reset_gpu_power_states()?;
                            Ok(DaemonResponse::OK)
                        }
                    },
                    _ => Err(DaemonError::ControllerError),
                },
                None => Err(DaemonError::InvalidID),
            },
            Action::SetConfig(config) => {
                self.config = config;
                self.gpu_controllers.clear();
                self.gpu_controllers = Self::load_gpu_controllers(&mut self.config);
                self.config.save().expect("Failed to save config");
                Ok(DaemonResponse::OK)
            }
            Action::GetConfig => Ok(DaemonResponse::Config(self.config.clone())),
            Action::OcControllerBasicGetTable(i) => match self.gpu_controllers.get(&i) {
                Some(controller) => match &controller.oc_controller {
                    Some(OcController::Basic(basic_controller)) => Ok(
                        DaemonResponse::BasicClocksTable(basic_controller.get_table()),
                    ),
                    _ => Err(DaemonError::ControllerError),
                },
                None => Err(DaemonError::InvalidID),
            },
            Action::OcControllerBasicSetGpuLevels(i, levels) => match self.gpu_controllers.get_mut(&i) {
                Some(controller) => match &mut controller.oc_controller {
                    Some(OcController::Basic(basic_controller)) => {
                        match basic_controller.set_gpu_power_levels(levels) {
                            Ok(()) => Ok(DaemonResponse::OK),
                            Err(e) => Err(DaemonError::ControllerError),
                            // TODO return an actual error here
                        }

                    },
                    _ => Err(DaemonError::ControllerError),
                },
                None => Err(DaemonError::InvalidID),
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum DaemonResponse {
    OK,
    GpuInfo(gpu_controller::GpuInfo),
    GpuStats(gpu_controller::GpuStats),
    Gpus(HashMap<u32, Option<String>>),
    PowerCap((i64, i64)),
    FanControlInfo(gpu_controller::FanControlInfo),
    OcControllerType(Option<OcControllerType>),
    Config(Config),
    OldClocksTable(OldClocksTable),
    BasicClocksTable(BasicClocksTable),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum OcControllerType {
    New,
    Old,
    Basic,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum DaemonError {
    ConnectionFailed,
    InvalidID,
    HWMonError,
    ControllerError,
    OcControllerError(OcControllerError),
    IoError,
}

impl From<OcControllerError> for DaemonError {
    fn from(err: OcControllerError) -> Self {
        Self::OcControllerError(err)
    }
}

impl From<std::io::Error> for DaemonError {
    fn from(err: std::io::Error) -> Self {
        Self::IoError
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[test]
    fn recognize_polaris() {
        init();

        let db = Daemon::get_pci_db_online().unwrap();

        let vendor_data = db.get_by_ids("1002", "67df", "1da2", "e387").unwrap();

        assert_eq!(
            vendor_data.gpu_vendor,
            Some("Advanced Micro Devices, Inc. [AMD/ATI]".to_string())
        );

        assert_eq!(
            vendor_data.gpu_model,
            Some("Ellesmere [Radeon RX 470/480/570/570X/580/580X/590]".to_string())
        );

        assert_eq!(
            vendor_data.card_model,
            Some("Radeon RX 580 Pulse 4GB".to_string())
        );
    }
}
