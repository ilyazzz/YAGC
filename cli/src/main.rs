use colored::*;
use daemon::{daemon_connection::DaemonConnection, OcControllerType};
use structopt::StructOpt;

#[derive(StructOpt)]
enum ConfigOpt {
    Show,
    AllowOnlineUpdating,
    DisallowOnlineUpdating,
}

#[derive(StructOpt)]
enum CurveOpt {
    /// Shows current fan control information
    Status {
        /// Specify a GPU ID as printed in `lact-cli gpus`. By default, all GPUs are printed.
        gpu_id: Option<u32>,
    },
}

#[derive(StructOpt)]
#[structopt(rename_all = "snake_case")]
enum Opt {
    /// Realtime GPU information
    Metrics {
        /// Specify a GPU ID as printed in `lact-cli gpus`. By default, all GPUs are printed.
        gpu_id: Option<u32>,
    },
    /// Get GPU list
    Gpus,
    /// General information about the GPU
    Info {
        /// Specify a GPU ID as printed in `lact-cli gpus`. By default, all GPUs are printed.
        gpu_id: Option<u32>,
    },
    ClocksTable,
    SetBasicGpuLevels,
    Config(ConfigOpt),
    /// Fan curve control
    Curve(CurveOpt),
}

fn main() {
    env_logger::init();

    let opt = Opt::from_args();

    let d = DaemonConnection::new().unwrap();
    log::trace!("connection established");

    match opt {
        Opt::Gpus => {
            let gpus = d.get_gpus();
            println!("{:?}", gpus);
        }
        Opt::Metrics { gpu_id } => {
            let mut gpu_ids: Vec<u32> = Vec::new();

            if let Some(gpu_id) = gpu_id {
                gpu_ids.push(gpu_id);
            } else {
                for (gpu_id, _) in d.get_gpus().unwrap() {
                    gpu_ids.push(gpu_id);
                }
            }

            for gpu_id in gpu_ids {
                print_stats(&d, gpu_id);
            }
        }
        Opt::Info { gpu_id } => {
            let mut gpu_ids: Vec<u32> = Vec::new();

            if let Some(gpu_id) = gpu_id {
                gpu_ids.push(gpu_id);
            } else {
                for (gpu_id, _) in d.get_gpus().unwrap() {
                    gpu_ids.push(gpu_id);
                }
            }

            for gpu_id in gpu_ids {
                print_info(&d, gpu_id);
            }
        }
        Opt::Curve(curve) => match curve {
            CurveOpt::Status { gpu_id } => {
                let mut gpu_ids: Vec<u32> = Vec::new();

                if let Some(gpu_id) = gpu_id {
                    gpu_ids.push(gpu_id);
                } else {
                    for (gpu_id, _) in d.get_gpus().unwrap() {
                        gpu_ids.push(gpu_id);
                    }
                }

                for gpu_id in gpu_ids {
                    print_fan_curve(&d, gpu_id);
                }
            }
        },
        Opt::Config(config_opt) => match config_opt {
            ConfigOpt::Show => print_config(&d),
            ConfigOpt::AllowOnlineUpdating => enable_online_update(&d),
            ConfigOpt::DisallowOnlineUpdating => disable_online_update(&d),
        },
        Opt::ClocksTable => {
            let gpus = d.get_gpus().unwrap();

            for (id, _) in gpus {
                print_clocks_table(&d, id);
            }
        }
        Opt::SetBasicGpuLevels => {
            let gpus = d.get_gpus().unwrap();

            for (id, _) in gpus {
                print_clocks_table(&d, id);
            }
        }
    }
}

fn set_basic_gpu_levels(d: &DaemonConnection, id: u32) {
    let oc_controller = d.get_oc_controller_type(id).unwrap();

    //oc_controller.set
}


fn print_clocks_table(d: &DaemonConnection, id: u32) {
    let oc_controller = d.get_oc_controller_type(id).unwrap();

    println!("OC Controller: {:?}", oc_controller);

    match oc_controller {
        Some(OcControllerType::Old) => {
            let clocks_table = d.old_oc_controller_get_clocks_table(id).unwrap();

            println!("Old clocks table format: {:?}", clocks_table);
        }
        Some(OcControllerType::Basic) => {
            let clocks_table = d.basic_oc_controller_get_table(id).unwrap();

            println!("Basic clocks table format: {:?}", clocks_table);
        }
        _ => unimplemented!(),
    }
}

fn disable_online_update(d: &DaemonConnection) {
    let mut config = d.get_config().unwrap();
    config.allow_online_update = Some(false);
    d.set_config(config).unwrap();
}

fn enable_online_update(d: &DaemonConnection) {
    let mut config = d.get_config().unwrap();
    config.allow_online_update = Some(true);
    d.set_config(config).unwrap();
}

fn print_config(d: &DaemonConnection) {
    let config = d.get_config().unwrap();

    println!(
        "{} {:?}",
        "Online PCI DB updating:".purple(),
        config.allow_online_update
    );
}

fn print_fan_curve(d: &DaemonConnection, gpu_id: u32) {
    let fan_control = d.get_fan_control(gpu_id).unwrap();

    if fan_control.enabled {
        println!("{}", "Fan curve:".yellow());

        for (temp, fan_speed) in fan_control.curve {
            println!(
                "{}{}: {}{}",
                temp.to_string().yellow(),
                "C°".yellow(),
                fan_speed.round().to_string().bold(),
                "%".bold()
            );
        }
    } else {
        println!("{}", "Automatic fan control used".yellow());
    }
}

fn print_info(d: &DaemonConnection, gpu_id: u32) {
    let gpu_info = d.get_gpu_info(gpu_id).unwrap();
    println!(
        "{} {}",
        "GPU Model:".blue(),
        gpu_info.vendor_data.card_model.unwrap_or_default().bold()
    );
    println!(
        "{} {}",
        "GPU Vendor:".blue(),
        gpu_info.vendor_data.gpu_vendor.unwrap_or_default().bold()
    );
    println!("{} {}", "Driver in use:".blue(), gpu_info.driver.bold());
    println!(
        "{} {}",
        "VBIOS Version:".blue(),
        gpu_info.vbios_version.bold()
    );
    println!(
        "{} {}",
        "VRAM Size:".blue(),
        gpu_info.vram_size.to_string().bold()
    );
    println!("{} {}", "Link Speed:".blue(), gpu_info.link_speed.bold());
}

fn print_stats(d: &DaemonConnection, gpu_id: u32) {
    let gpu_stats = d.get_gpu_stats(gpu_id).unwrap();
    println!(
        "{} {}/{}{}",
        "VRAM Usage:".green(),
        gpu_stats.mem_used.unwrap_or_default().to_string().bold(),
        gpu_stats.mem_total.unwrap_or_default().to_string().bold(),
        "MiB".bold(),
    );
    println!(
        "{} {}{}",
        "Temperature:".green(),
        gpu_stats
            .temperatures
            .get("edge")
            .unwrap()
            .current
            .to_string()
            .bold(),
        "°C".bold(),
    );
    println!(
        "{} {}/{}{}",
        "Fan Speed:".green(),
        gpu_stats.fan_speed.unwrap_or_default().to_string().bold(),
        gpu_stats
            .max_fan_speed
            .unwrap_or_default()
            .to_string()
            .bold(),
        "RPM".bold(),
    );
    println!(
        "{} {}{}",
        "GPU Clock:".green(),
        gpu_stats.gpu_freq.unwrap_or_default().to_string().bold(),
        "MHz".bold(),
    );
    println!(
        "{} {}{}",
        "GPU Voltage:".green(),
        (gpu_stats.voltage.unwrap_or_default() as f64 / 1000.0)
            .to_string()
            .bold(),
        "V".bold(),
    );
    println!(
        "{} {}{}",
        "VRAM Clock:".green(),
        gpu_stats.mem_freq.unwrap_or_default().to_string().bold(),
        "MHz".bold(),
    );
    println!(
        "{} {}/{}{}",
        "Power Usage:".green(),
        gpu_stats.power_avg.unwrap_or_default().to_string().bold(),
        gpu_stats.power_cap.unwrap_or_default().to_string().bold(),
        "W".bold(),
    );
}
