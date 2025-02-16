mod exporter;
pub mod gpu_controller;
pub mod handler;
mod profiles;
pub(crate) mod system;
mod vulkan;

use self::handler::Handler;
use crate::{config::Config, socket};
use anyhow::{anyhow, Context};
use futures::future::join_all;
use lact_schema::{Pong, Request, Response};
use serde::Serialize;
use std::{fmt::Debug, net::SocketAddr};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, UnixListener},
};
use tracing::{error, info, instrument, trace};

pub struct Server {
    pub handler: Handler,
    unix_listener: UnixListener,
    tcp_listener: Option<TcpListener>,
}

impl Server {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let unix_listener = socket::listen(&config.daemon.admin_groups)?;

        let tcp_listener = if let Some(address) = &config.daemon.tcp_listen_address {
            let listener = TcpListener::bind(address)
                .await
                .with_context(|| format!("Could not bind to TCP address {address}"))?;
            info!("TCP listening on {}", listener.local_addr()?);
            Some(listener)
        } else {
            info!("TCP listener disabled");
            None
        };

        let exporter_server = if let Some(exporter_address) = &config.daemon.exporter_listen_address
        {
            let addr: SocketAddr = exporter_address
                .parse()
                .context("Invalid exporter address")?;

            let server = tiny_http::Server::http(addr)
                .map_err(|err| anyhow!("Could not start metrics exporter: {err}"))?;
            info!("Prometheus metrics exporter listening on {exporter_address}");

            Some(server)
        } else {
            info!("Prometheus metrics exporter disabled");
            None
        };

        let handler = Handler::new(config).await?;

        if let Some(server) = exporter_server {
            let handler = handler.clone();
            tokio::task::spawn_local(async move { exporter::run(server, &handler).await });
        }

        Ok(Self {
            handler,
            unix_listener,
            tcp_listener,
        })
    }

    pub async fn run(self) {
        let mut tasks = vec![];

        let unix_handler = self.handler.clone();
        let unix_task = tokio::task::spawn_local(async move {
            loop {
                match self.unix_listener.accept().await {
                    Ok((stream, _)) => {
                        let handler = unix_handler.clone();
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
        });
        tasks.push(unix_task);

        if let Some(tcp_listener) = self.tcp_listener {
            let tcp_task = tokio::task::spawn_local(async move {
                loop {
                    match tcp_listener.accept().await {
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
            });
            tasks.push(tcp_task);
        }

        join_all(tasks).await;
    }
}

#[instrument(level = "debug", skip(stream, handler))]
pub async fn handle_stream<T: AsyncRead + AsyncWrite + Unpin>(
    stream: T,
    handler: Handler,
) -> anyhow::Result<()> {
    let mut stream = BufReader::new(stream);

    let mut buf = String::new();
    while stream.read_line(&mut buf).await? != 0 {
        trace!("handling request: {}", buf.trim_end());

        let maybe_request = serde_json::from_str(&buf);
        let response = match maybe_request {
            Ok(request) => match handle_request(request, &handler).await {
                Ok(response) => response,
                Err(error) => serde_json::to_vec(&Response::<()>::from(error))?,
            },
            Err(error) => serde_json::to_vec(&Response::<()>::from(
                anyhow::Error::new(error).context("Failed to deserialize"),
            ))?,
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
        Request::SystemInfo => ok_response(system::info().await?),
        Request::ListDevices => ok_response(handler.list_devices().await),
        Request::DeviceInfo { id } => ok_response(handler.get_device_info(id).await?),
        Request::DeviceStats { id } => ok_response(handler.get_gpu_stats(id).await?),
        Request::DeviceClocksInfo { id } => ok_response(handler.get_clocks_info(id).await?),
        Request::DevicePowerProfileModes { id } => {
            ok_response(handler.get_power_profile_modes(id).await?)
        }
        Request::SetFanControl(opts) => ok_response(handler.set_fan_control(opts).await?),
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
        Request::SetPowerProfileMode {
            id,
            index,
            custom_heuristics,
        } => ok_response(
            handler
                .set_power_profile_mode(id, index, custom_heuristics)
                .await?,
        ),
        Request::GetPowerStates { id } => ok_response(handler.get_power_states(id).await?),
        Request::SetEnabledPowerStates { id, kind, states } => {
            ok_response(handler.set_enabled_power_states(id, kind, states).await?)
        }
        Request::VbiosDump { id } => ok_response(handler.vbios_dump(id).await?),
        Request::ListProfiles { include_state } => {
            ok_response(handler.list_profiles(include_state).await)
        }
        Request::SetProfile { name, auto_switch } => ok_response(
            handler
                .set_profile(name.map(Into::into), auto_switch)
                .await?,
        ),
        Request::CreateProfile { name, base } => {
            ok_response(handler.create_profile(name, base).await?)
        }
        Request::DeleteProfile { name } => ok_response(handler.delete_profile(name).await?),
        Request::MoveProfile { name, new_position } => {
            ok_response(handler.move_profile(&name, new_position).await?)
        }
        Request::EvaluateProfileRule { rule } => ok_response(handler.evaluate_profile_rule(&rule)?),
        Request::SetProfileRule { name, rule } => {
            ok_response(handler.set_profile_rule(&name, rule).await?)
        }
        Request::EnableOverdrive => ok_response(system::enable_overdrive().await?),
        Request::DisableOverdrive => ok_response(system::disable_overdrive().await?),
        Request::GenerateSnapshot => ok_response(handler.generate_snapshot().await?),
        Request::ConfirmPendingConfig(command) => {
            ok_response(handler.confirm_pending_config(command)?)
        }
        Request::RestConfig => {
            handler.reset_config().await;
            ok_response(())
        }
    }
}

fn ok_response<T: Serialize + Debug>(data: T) -> anyhow::Result<Vec<u8>> {
    trace!("responding with {data:?}");
    Ok(serde_json::to_vec(&Response::Ok(data))?)
}

fn ping() -> Pong {
    Pong
}
