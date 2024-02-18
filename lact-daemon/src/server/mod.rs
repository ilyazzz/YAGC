pub mod gpu_controller;
pub mod handler;
pub(crate) mod system;
mod vulkan;

use self::handler::Handler;
use crate::{config::Config, socket};
use lact_schema::{Pong, Request, Response};
use serde::Serialize;
use std::fmt::Debug;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use tracing::{debug, error, instrument};

pub struct Server {
    pub handler: Handler,
    listener: UnixListener,
}

impl Server {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let listener = socket::listen(&config.daemon.admin_groups)?;
        let handler = Handler::new(config).await?;

        Ok(Self { handler, listener })
    }

    pub async fn run(self) {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    let handler = self.handler.clone();
                    tokio::task::spawn_local(async move {
                        if let Err(error) = handle_stream(stream, handler).await {
                            error!("{error}");
                        }
                    });
                }
                Err(error) => {
                    error!("failed to handle connection: {error}");
                }
            }
        }
    }
}

#[instrument(level = "debug", skip(stream, handler))]
pub async fn handle_stream(stream: UnixStream, handler: Handler) -> anyhow::Result<()> {
    let mut stream = BufReader::new(stream);

    let mut buf = String::new();
    while stream.read_line(&mut buf).await? != 0 {
        debug!("handling request: {}", buf.trim_end());

        let maybe_request = serde_json::from_str(&buf);
        let response = match maybe_request {
            Ok(request) => match handle_request(request, &handler).await {
                Ok(response) => response,
                Err(error) => serde_json::to_vec(&Response::<()>::Error(format!("{error:?}")))?,
            },
            Err(error) => serde_json::to_vec(&Response::<()>::Error(format!(
                "Failed to deserialize request: {error}"
            )))?,
        };

        stream.write_all(&response).await?;
        stream.write_all(b"\n").await?;

        buf.clear();
    }

    Ok(())
}

#[instrument(level = "debug", skip(handler))]
async fn handle_request<'a>(request: Request<'a>, handler: &'a Handler) -> anyhow::Result<Vec<u8>> {
    match request {
        Request::Ping => ok_response(ping()),
        Request::SystemInfo => ok_response(system::info()?),
        Request::ListDevices => ok_response(handler.list_devices()),
        Request::DeviceInfo { id } => ok_response(handler.get_device_info(id)?),
        Request::DeviceStats { id } => ok_response(handler.get_gpu_stats(id)?),
        Request::DeviceClocksInfo { id } => ok_response(handler.get_clocks_info(id)?),
        Request::DevicePowerProfileModes { id } => {
            ok_response(handler.get_power_profile_modes(id)?)
        }
        Request::SetFanControl(command) => ok_response(handler.set_fan_control(command).await?),
        Request::ResetPmfw { id } => ok_response(handler.reset_pmfw(id).await?),
        Request::SetPowerCap { id, cap } => ok_response(handler.set_power_cap(id, cap).await?),
        Request::SetPerformanceLevel {
            id,
            performance_level,
        } => ok_response(handler.set_performance_level(id, performance_level).await?),
        Request::SetClocksValue { id, command } => {
            ok_response(handler.set_clocks_value(id, command).await?)
        }
        Request::BatchSetClocksValue { id, commands } => {
            ok_response(handler.batch_set_clocks_value(id, commands).await?)
        }
        Request::SetPowerProfileMode { id, index } => {
            ok_response(handler.set_power_profile_mode(id, index).await?)
        }
        Request::GetPowerStates { id } => ok_response(handler.get_power_states(id)?),
        Request::SetEnabledPowerStates { id, kind, states } => {
            ok_response(handler.set_enabled_power_states(id, kind, states).await?)
        }
        Request::EnableOverdrive => ok_response(system::enable_overdrive()?),
        Request::DisableOverdrive => ok_response(system::disable_overdrive()?),
        Request::GenerateSnapshot => ok_response(handler.generate_snapshot()?),
        Request::ConfirmPendingConfig(command) => {
            ok_response(handler.confirm_pending_config(command)?)
        }
    }
}

fn ok_response<T: Serialize + Debug>(data: T) -> anyhow::Result<Vec<u8>> {
    debug!("responding with {data:?}");
    Ok(serde_json::to_vec(&Response::Ok(data))?)
}

fn ping() -> Pong {
    Pong
}
