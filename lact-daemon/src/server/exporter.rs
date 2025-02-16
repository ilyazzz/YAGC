use prometheus::{
    register_gauge_with_registry, register_int_gauge_with_registry, Registry, TextEncoder,
};
use std::collections::HashMap;
use tiny_http::Response;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, warn};

use super::handler::Handler;

pub async fn run(server: tiny_http::Server, handler: &Handler) {
    let (tx, mut rx) = mpsc::channel(1);

    // We listen to http requests in the background on a blocking task, and collect the metrics as needed from the main one
    tokio::task::spawn_blocking(move || loop {
        match server.recv() {
            Ok(req) => {
                let (response_tx, response_rx) = oneshot::channel();
                tx.blocking_send(response_tx).unwrap();

                let response = response_rx.blocking_recv().unwrap();
                if let Err(err) = req.respond(response) {
                    warn!("could not write metrics response: {err}");
                }
            }
            Err(err) => {
                error!("metrics exporter request error: {err}");
            }
        }
    });

    while let Some(response_tx) = rx.recv().await {
        let registry = Registry::new();
        collect_metrics(handler, &registry).await;
        let metric_families = registry.gather();

        let encoder = TextEncoder::new();
        let output = encoder
            .encode_to_string(&metric_families)
            .expect("Failed to encode metrics");

        let response = Response::from_string(output);
        let _ = response_tx.send(response);
    }
}

async fn collect_metrics(handler: &Handler, registry: &Registry) {
    let gpu_controllers = handler.gpu_controllers.read().await;
    let config = handler.config.read().await;

    for (id, controller) in gpu_controllers.iter() {
        let gpu_config = config.gpus().ok().and_then(|gpus| gpus.get(id));

        let info = controller.get_info(true);
        let stats = controller.get_stats(gpu_config);

        let mut device_name = String::new();
        if let Some(pci_info) = &info.pci_info {
            if let Some(vendor) = &pci_info.device_pci_info.vendor {
                device_name.push_str(vendor);
            }
            if let Some(model) = &pci_info.device_pci_info.model {
                if !device_name.is_empty() {
                    device_name.push(' ');
                }
                device_name.push_str(model);
            }
        }

        macro_rules! gpu_opts {
            (
                $name: expr,
                $help: expr,
                $($key:expr => $value:expr),* $(,)*
            ) => {{
                let opts = prometheus::Opts::new($name, $help);

                let labels = HashMap::from_iter([
                    ("gpu_id".to_owned(), id.clone()),
                    ("gpu_name".to_owned(), device_name.clone()),
                    $(
                        ($key.to_string(), $value.to_string()),
                    )*
                ]);
                opts.const_labels(labels)
            }};
        }

        macro_rules! gpu_gauge {
            (
                $name: expr,
                $help: expr,
                $value: expr,
                $($key:expr => $label:expr),* $(,)*
            ) => {{
                #[allow(clippy::cast_precision_loss)]
                register_gauge_with_registry!(
                    gpu_opts! {
                        $name,
                        $help,
                        $($key => $label)*
                    },
                    registry
                )
                .unwrap()
                .set($value.into());
            }}

        }

        register_int_gauge_with_registry!(
            gpu_opts! {
                "lact_gpu_info",
                "A static gauge containing basic GPU info in the labels",
                "driver" => info.driver,
                "family" => info.drm_info.and_then(|drm| drm.family_name).unwrap_or_default(),
            },
            registry
        )
        .unwrap();

        if let Some(usage) = stats.busy_percent {
            gpu_gauge! {
                "lact_gpu_usage",
                "GPU usage percentage",
                usage,
            };
        }

        if let Some(power_current) = stats
            .power
            .average
            .filter(|value| *value != 0.0)
            .or(stats.power.current.filter(|value| *value != 0.0))
        {
            gpu_gauge! {
                "lact_gpu_power_usage",
                "Current power consumption",
                power_current,
            };
        }

        if let Some(power_cap) = stats.power.cap_current {
            gpu_gauge! {
                "lact_gpu_power_cap",
                "Power consumption cap",
                power_cap,
            };
        }

        for (temp_name, temp) in stats.temps {
            if let Some(value) = temp.current {
                gpu_gauge!(
                    "lact_gpu_temperature",
                    "Current temperature",
                    value,
                    "sensor" => temp_name,
                );
            }
        }

        if let Some(gpu_clock) = stats.clockspeed.gpu_clockspeed {
            gpu_gauge!(
                "lact_gpu_frequency",
                "Current frequency",
                gpu_clock as f64,
                "type" => "GPU",
            );
        }

        if let Some(vram_clock) = stats.clockspeed.vram_clockspeed {
            gpu_gauge!(
                "lact_gpu_frequency",
                "Current frequency",
                vram_clock as f64,
                "type" => "VRAM",
            );
        }

        if let Some(fan_pwm) = stats.fan.pwm_current {
            gpu_gauge!("lact_gpu_fan_pwm", "Fan speed (in PWM)", fan_pwm,);
        }

        if let Some(fan_rpm) = stats.fan.speed_current {
            gpu_gauge!("lact_gpu_fan_rpm", "Fan speed (in RPM)", fan_rpm,);
        }
    }
}
