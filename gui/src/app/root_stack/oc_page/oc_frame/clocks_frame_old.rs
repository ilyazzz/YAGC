use daemon::gpu_controller::oc_controller::OldClocksTable;
use gtk::{prelude::ComboBoxExtManual, *};
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct ClocksFrameOld {
    pub container: Grid,
    gpu_powerlevel_dropdown: ComboBoxText,
    clocks_table: Arc<RwLock<OldClocksTable>>,
    gpu_clock_adjustment: Adjustment,
    gpu_voltage_adjustment: Adjustment,
}

impl ClocksFrameOld {
    pub fn new() -> Self {
        let container = Grid::new();

        container.set_column_spacing(5);
        container.set_row_spacing(5);

        let gpu_powerlevel_dropdown = ComboBoxText::new();
        {
            container.attach(
                &{
                    let label = Label::new(Some("GPU Power level"));
                    label.set_tooltip_text(Some("When the automatic power profile is selected, the GPU will dynamically change between these levels depending on the load."));
                    label
                },
                0,
                0,
                1,
                1,
            );

            gpu_powerlevel_dropdown.set_hexpand(true);
            container.attach(&gpu_powerlevel_dropdown, 1, 0, 1, 1);
        }

        let gpu_clock_adjustment = Adjustment::new(0.0, 0.0, 0.0, 1.0, 10.0, 0.0);
        {
            container.attach(&Label::new(Some("GPU Clockspeed (MHz)")), 0, 1, 1, 1);

            let gpu_clock_scale = Scale::new(Orientation::Horizontal, Some(&gpu_clock_adjustment));
            gpu_clock_scale.set_value_pos(PositionType::Right);

            container.attach(&gpu_clock_scale, 1, 1, 1, 1);
        }

        let gpu_voltage_adjustment = Adjustment::new(0.0, 0.0, 0.0, 0.001, 0.001, 0.0);
        {
            container.attach(&Label::new(Some("GPU Voltage (V)")), 0, 2, 1, 1);

            let gpu_voltage_scale =
                Scale::new(Orientation::Horizontal, Some(&gpu_voltage_adjustment));
            gpu_voltage_scale.set_value_pos(PositionType::Right);
            gpu_voltage_scale.set_digits(3);

            container.attach(&gpu_voltage_scale, 1, 2, 1, 1);
        }

        let clocks_table = Arc::new(RwLock::new(OldClocksTable::default()));

        Self {
            container,
            gpu_powerlevel_dropdown,
            clocks_table,
            gpu_clock_adjustment,
            gpu_voltage_adjustment,
        }
    }

    pub fn set_data(&self, clocks_table: OldClocksTable) {
        for (profile, _) in clocks_table.gpu_power_levels.iter().rev() {
            log::trace!("Adding profile {}", profile);
            self.gpu_powerlevel_dropdown
                .append(Some(&profile.to_string()), &profile.to_string());
        }

        self.gpu_clock_adjustment
            .set_lower(clocks_table.gpu_clocks_range.0 as f64);
        self.gpu_clock_adjustment
            .set_upper(clocks_table.gpu_clocks_range.1 as f64);

        self.gpu_voltage_adjustment
            .set_lower((clocks_table.voltage_range.0 as f64) / 1000.0);
        self.gpu_voltage_adjustment
            .set_upper((clocks_table.voltage_range.1 as f64) / 1000.0);
        
        log::info!("{}", self.gpu_voltage_adjustment.get_upper());

        *self.clocks_table.try_write().unwrap() = clocks_table;

        {
            let sself = self.clone();

            self.gpu_powerlevel_dropdown
                .connect_changed(move |dropdown| {
                    let selected_id: u32 = dropdown
                        .get_active_id()
                        .unwrap()
                        .parse()
                        .unwrap();

                    let clocks_table = sself.clocks_table.try_read().unwrap();

                    let power_level = clocks_table.gpu_power_levels.get(&selected_id).expect("Invalid power profile selected");

                    log::info!("Selected power profile {:?}", power_level);
                    
                    sself.gpu_clock_adjustment.set_value(power_level.0 as f64);
                    sself.gpu_voltage_adjustment.set_value((power_level.1 as f64) / 1000.0);
                });
        }

        self.gpu_powerlevel_dropdown.set_active(Some(0));
    }
}
