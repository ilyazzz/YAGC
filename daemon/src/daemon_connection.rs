use crate::gpu_controller::{GpuInfo, PowerProfile, oc_controller::{BasicClocksTable, OldClocksTable}};
use crate::Daemon;
use crate::DaemonError;
use crate::{config::Config, OldOCControllerAction};
use crate::{
    gpu_controller::{FanControlInfo, GpuStats},
    OcControllerType,
};
use crate::{Action, DaemonResponse, SOCK_PATH};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy)]
pub struct DaemonConnection {}

pub const BUFFER_SIZE: usize = 4096;

impl DaemonConnection {
    pub fn new() -> Result<Self, DaemonError> {
        let addr = nix::sys::socket::SockAddr::Unix(
            nix::sys::socket::UnixAddr::new_abstract(SOCK_PATH.as_bytes()).unwrap(),
        );
        let socket = nix::sys::socket::socket(
            nix::sys::socket::AddressFamily::Unix,
            nix::sys::socket::SockType::Stream,
            nix::sys::socket::SockFlag::empty(),
            None,
        )
        .expect("Creating socket failed");
        nix::sys::socket::connect(socket, &addr).expect("Socket connect failed");

        nix::unistd::write(socket, &bincode::serialize(&Action::CheckAlive).unwrap())
            .expect("Writing check alive to socket failed");

        nix::sys::socket::shutdown(socket, nix::sys::socket::Shutdown::Write)
            .expect("Could not shut down");

        let mut buffer = Vec::<u8>::new();
        buffer.resize(BUFFER_SIZE, 0);
        loop {
            match nix::unistd::read(socket, &mut buffer) {
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
        nix::sys::socket::shutdown(socket, nix::sys::socket::Shutdown::Both)
            .expect("Could not shut down");

        nix::unistd::close(socket).expect("Failed to close");

        let result: Result<DaemonResponse, DaemonResponse> =
            bincode::deserialize(&buffer).expect("failed to deserialize message");

        match result {
            Ok(_) => Ok(DaemonConnection {}),
            Err(_) => Err(DaemonError::ConnectionFailed),
        }
    }

    fn send_action(&self, action: Action) -> Result<DaemonResponse, DaemonError> {
        let addr = nix::sys::socket::SockAddr::Unix(
            nix::sys::socket::UnixAddr::new_abstract(SOCK_PATH.as_bytes()).unwrap(),
        );
        let socket = nix::sys::socket::socket(
            nix::sys::socket::AddressFamily::Unix,
            nix::sys::socket::SockType::Stream,
            nix::sys::socket::SockFlag::empty(),
            None,
        )
        .expect("Socket failed");
        nix::sys::socket::connect(socket, &addr).expect("connect failed");

        let b = bincode::serialize(&action).unwrap();
        nix::unistd::write(socket, &b).expect("Writing action to socket failed");

        nix::sys::socket::shutdown(socket, nix::sys::socket::Shutdown::Write)
            .expect("Could not shut down");

        let buffer = Daemon::read_buffer(socket);

        nix::sys::socket::shutdown(socket, nix::sys::socket::Shutdown::Both)
            .expect("Failed to shut down");

        nix::unistd::close(socket).expect("Failed to close");

        bincode::deserialize(&buffer).expect("failed to deserialize message")
    }

    pub fn get_gpu_stats(&self, gpu_id: u32) -> Result<GpuStats, DaemonError> {
        match self.send_action(Action::GetStats(gpu_id))? {
            DaemonResponse::GpuStats(stats) => Ok(stats),
            _ => unreachable!(),
        }
    }

    pub fn get_gpu_info(&self, gpu_id: u32) -> Result<GpuInfo, DaemonError> {
        match self.send_action(Action::GetInfo(gpu_id))? {
            DaemonResponse::GpuInfo(info) => Ok(info),
            _ => unreachable!("impossible enum variant"),
        }
    }

    pub fn start_fan_control(&self, gpu_id: u32) -> Result<(), DaemonError> {
        match self.send_action(Action::StartFanControl(gpu_id))? {
            DaemonResponse::OK => Ok(()),
            _ => Err(DaemonError::HWMonError),
        }
    }

    pub fn stop_fan_control(&self, gpu_id: u32) -> Result<(), DaemonError> {
        match self.send_action(Action::StopFanControl(gpu_id))? {
            DaemonResponse::OK => Ok(()),
            _ => Err(DaemonError::HWMonError),
        }
    }

    pub fn get_fan_control(&self, gpu_id: u32) -> Result<FanControlInfo, DaemonError> {
        match self.send_action(Action::GetFanControl(gpu_id))? {
            DaemonResponse::FanControlInfo(info) => Ok(info),
            _ => unreachable!(),
        }
    }

    pub fn set_fan_curve(&self, gpu_id: u32, curve: BTreeMap<i64, f64>) -> Result<(), DaemonError> {
        match self.send_action(Action::SetFanCurve(gpu_id, curve))? {
            DaemonResponse::OK => Ok(()),
            _ => unreachable!(),
        }
    }

    pub fn set_power_cap(&self, gpu_id: u32, cap: i64) -> Result<(), DaemonError> {
        match self.send_action(Action::SetPowerCap(gpu_id, cap))? {
            DaemonResponse::OK => Ok(()),
            _ => unreachable!(),
        }
    }

    pub fn set_power_profile(&self, gpu_id: u32, profile: PowerProfile) -> Result<(), DaemonError> {
        match self.send_action(Action::SetPowerProfile(gpu_id, profile))? {
            DaemonResponse::OK => Ok(()),
            _ => unreachable!(),
        }
    }

    // TODO: make a separate handler for all the OcController-specific commands
    pub fn old_oc_controller_set_gpu_power_state(
        &self,
        gpu_id: u32,
        num: u32,
        clockspeed: i64,
        voltage: Option<i64>,
    ) -> Result<(), DaemonError> {
        match self.send_action(Action::OcControllerOld(
            gpu_id,
            OldOCControllerAction::SetGPUPowerState(num, clockspeed, voltage),
        ))? {
            DaemonResponse::OK => Ok(()),
            _ => unreachable!(),
        }
    }

    pub fn old_oc_controller_set_vram_power_state(
        &self,
        gpu_id: u32,
        num: u32,
        clockspeed: i64,
        voltage: Option<i64>,
    ) -> Result<(), DaemonError> {
        match self.send_action(Action::OcControllerOld(
            gpu_id,
            OldOCControllerAction::SetVRAMPowerState(num, clockspeed, voltage),
        ))? {
            DaemonResponse::OK => Ok(()),
            _ => unreachable!(),
        }
    }

    pub fn old_oc_controller_get_clocks_table(
        &self,
        gpu_id: u32,
    ) -> Result<OldClocksTable, DaemonError> {
        match self.send_action(Action::OcControllerOld(
            gpu_id,
            OldOCControllerAction::GetClocksTable,
        ))? {
            DaemonResponse::OldClocksTable(table) => Ok(table),
            _ => unreachable!(),
        }
    }

    /*pub fn set_gpu_max_power_state(
        &self,
        gpu_id: u32,
        clockspeed: i64,
        voltage: Option<i64>,
    ) -> Result<(), DaemonError> {
        match self.send_action(Action::SetGPUMaxPowerState(gpu_id, clockspeed, voltage))? {
            DaemonResponse::OK => Ok(()),
            _ => unreachable!(),
        }
    }*/

    /*pub fn new_controller_set_vram_max_clock(&self, gpu_id: u32, clockspeed: i64) -> Result<(), DaemonError> {
        match self.send_action(Action(gpu_id, clockspeed))? {
            DaemonResponse::OK => Ok(()),
            _ => unreachable!(),
        }
    }*/

    // TODO
    pub fn commit_gpu_power_states(&self, gpu_id: u32) -> Result<(), DaemonError> {
        /*match self.send_action(Action::CommitGPUPowerStates(gpu_id))? {
            DaemonResponse::OK => Ok(()),
            _ => unreachable!(),
        }*/
        Ok(())
    }

    pub fn reset_gpu_power_states(&self, gpu_id: u32) -> Result<(), DaemonError> {
        /*match self.send_action(Action::ResetGPUPowerStates(gpu_id))? {
        DaemonResponse::OK => Ok(()),
        _ => unreachable!(),
        }*/
        Ok(())
    }

    pub fn get_gpus(&self) -> Result<HashMap<u32, Option<String>>, DaemonError> {
        match self.send_action(Action::GetGpus)? {
            DaemonResponse::Gpus(gpus) => Ok(gpus),
            _ => unreachable!(),
        }
    }

    pub fn get_oc_controller_type(
        &self,
        gpu_id: u32,
    ) -> Result<Option<OcControllerType>, DaemonError> {
        match self.send_action(Action::GetOCController(gpu_id))? {
            DaemonResponse::OcControllerType(oc_controller_type) => Ok(oc_controller_type),
            _ => unreachable!(),
        }
    }

    pub fn basic_oc_controller_get_table(
        &self,
        gpu_id: u32,
    ) -> Result<BasicClocksTable, DaemonError> {
        match self.send_action(Action::OcControllerBasicGetTable(gpu_id))? {
            DaemonResponse::BasicClocksTable(table) => Ok(table),
            _ => unreachable!(),
        }
    }

    pub fn shutdown(&self) {
        let addr = nix::sys::socket::SockAddr::Unix(
            nix::sys::socket::UnixAddr::new_abstract(SOCK_PATH.as_bytes()).unwrap(),
        );
        let socket = nix::sys::socket::socket(
            nix::sys::socket::AddressFamily::Unix,
            nix::sys::socket::SockType::Stream,
            nix::sys::socket::SockFlag::empty(),
            None,
        )
        .expect("Socket failed");
        nix::sys::socket::connect(socket, &addr).expect("connect failed");
        nix::unistd::write(socket, &mut &bincode::serialize(&Action::Shutdown).unwrap())
            .expect("Writing shutdown to socket failed");
    }

    pub fn get_config(&self) -> Result<Config, DaemonError> {
        match self.send_action(Action::GetConfig)? {
            DaemonResponse::Config(config) => Ok(config),
            _ => unreachable!(),
        }
    }

    pub fn set_config(&self, config: Config) -> Result<(), DaemonError> {
        match self.send_action(Action::SetConfig(config))? {
            DaemonResponse::OK => Ok(()),
            _ => unreachable!(),
        }
    }
}
