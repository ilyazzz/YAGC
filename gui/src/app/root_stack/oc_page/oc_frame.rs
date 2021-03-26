use std::sync::{Arc, Mutex};

use daemon::gpu_controller::oc_controller::ClocksTable;
use gtk::*;

mod clocks_frame_new;
mod clocks_frame_old;

use clocks_frame_new::ClocksFrameNew;
use clocks_frame_old::ClocksFrameOld;

pub struct ClocksSettings {
    pub gpu_clock: i64,
    pub vram_clock: i64,
    pub gpu_voltage: i64,
}

#[derive(Clone)]
enum ClocksFrame {
    Old(ClocksFrameOld),
    New(ClocksFrameNew),
}

#[derive(Clone)]
pub struct OcFrame {
    pub container: Frame,
    clocks_frame: Arc<Mutex<Option<ClocksFrame>>>,
    clocks_frame_container: Frame,
    reset_button: Button,
}

impl OcFrame {
    pub fn new() -> Self {
        let container = Frame::new(None);

        container.set_margin_start(10);
        container.set_margin_end(10);

        container.set_shadow_type(ShadowType::None);

        container.set_label_widget(Some(&{
            let label = Label::new(None);
            label.set_markup("<span font_desc='11'><b>Clockspeed and voltage</b></span>");
            label
        }));
        container.set_label_align(0.2, 0.0);

        let root_box = Box::new(Orientation::Vertical, 0);

        let reset_button = Button::new();
        {
            reset_button.set_label("Reset");

            root_box.pack_end(&reset_button, false, true, 5);
        }

        let clocks_frame_container = Frame::new(None);

        clocks_frame_container.set_shadow_type(ShadowType::None);

        root_box.pack_end(&clocks_frame_container, true, true, 5);

        container.add(&root_box);

        Self {
            container,
            reset_button,
            clocks_frame: Arc::new(Mutex::new(None)),
            clocks_frame_container,
        }
    }

    pub fn get_visibility(&self) -> bool {
        self.container.get_visible()
    }

    pub fn set_clocks(&self, clocks_table: ClocksTable) {
        let clocks_frame = &mut *self.clocks_frame.try_lock().unwrap();

        match clocks_frame {
            None => {
                if let Some(child) = self.clocks_frame_container.get_child() {
                    self.clocks_frame_container.remove(&child);
                }

                match clocks_table {
                    ClocksTable::Old(clocks_table_old) => {
                        log::trace!("Old clocks format detected");

                        let clocks_frame_old = ClocksFrameOld::new();
                        clocks_frame_old.set_data(clocks_table_old);

                        self.clocks_frame_container.add(&clocks_frame_old.container);

                        *clocks_frame = Some(ClocksFrame::Old(clocks_frame_old));
                    }
                    ClocksTable::New(clocks_table_new) => {
                        let clocks_frame_new = ClocksFrameNew::new();
                        // clocks_frame.set_info(&clocks_table_old);
                        *clocks_frame = Some(ClocksFrame::New(clocks_frame_new));
                    }
                    ClocksTable::Basic(_) => unimplemented!(),
                }
                
                self.clocks_frame_container.show_all();
            }
            Some(clocks_frame_item) => match clocks_frame_item {
                ClocksFrame::Old(clocks_frame_old) => match clocks_table {
                    ClocksTable::Old(clocks_table_old) => {
                        clocks_frame_old.set_data(clocks_table_old)
                    }
                    ClocksTable::New(_) => {
                        // Clears the current clocks frame as the type has changed
                        *clocks_frame = None;
                        self.set_clocks(clocks_table);
                        return;
                    }
                    ClocksTable::Basic(_) => {
                        *clocks_frame = None;
                        self.set_clocks(clocks_table);
                        return;
                    }
                },
                ClocksFrame::New(clocks_frame_new) => match clocks_table {
                    ClocksTable::New(clocks_frame_new) => {
                        // clocks_frame_new.set_clocks(&clocks_table_new)
                    }
                    ClocksTable::Old(_) => {
                        // Clears the current clocks frame as the type has changed
                        *clocks_frame = None;
                        self.set_clocks(clocks_table);
                        return;
                    }
                    ClocksTable::Basic(_) => {
                        *clocks_frame = None;
                        self.set_clocks(clocks_table);
                        return;
                    }
                },
            },
        }
    }

    /*pub fn set_clocks(&self, clocks_table: &ClocksTable) {
        match clocks_table {
            ClocksTable::Old(clocks_table) => {
                self.gpu_clock_adjustment
                    .set_lower(clocks_table.gpu_clocks_range.0 as f64);
                self.gpu_clock_adjustment
                    .set_upper(clocks_table.gpu_clocks_range.1 as f64);

                self.gpu_voltage_adjustment
                    .set_lower(clocks_table.voltage_range.0 as f64 / 1000.0);
                self.gpu_voltage_adjustment
                    .set_upper(clocks_table.voltage_range.1 as f64 / 1000.0);

                self.vram_clock_adjustment
                    .set_lower(clocks_table.mem_clocks_range.0 as f64);
                self.vram_clock_adjustment
                    .set_upper(clocks_table.mem_clocks_range.1 as f64);

                let (gpu_clockspeed, gpu_voltage) =
                    clocks_table.gpu_power_levels.iter().next_back().unwrap().1;

                self.gpu_clock_adjustment.set_value(*gpu_clockspeed as f64);

                self.gpu_voltage_adjustment
                    .set_value(*gpu_voltage as f64 / 1000.0);

                let (vram_clockspeed, _) =
                    clocks_table.mem_power_levels.iter().next_back().unwrap().1;

                self.vram_clock_adjustment
                    .set_value(*vram_clockspeed as f64);
            }
            ClocksTable::New(clocks_table) => {
                self.gpu_clock_adjustment
                    .set_lower(clocks_table.gpu_clocks_range.0 as f64);
                self.gpu_clock_adjustment
                    .set_upper(clocks_table.gpu_clocks_range.1 as f64);

                /* self.gpu_voltage_adjustment
                   .set_lower(clocks_table.voltage_range.0 as f64 / 1000.0);
                self.gpu_voltage_adjustment
                   .set_upper(clocks_table.voltage_range.1 as f64 / 1000.0);*/

                self.vram_clock_adjustment
                    .set_lower(clocks_table.mem_clocks_range.0 as f64);
                self.vram_clock_adjustment
                    .set_upper(clocks_table.mem_clocks_range.1 as f64);

                self.gpu_clock_adjustment
                    .set_value(clocks_table.current_gpu_clocks.1 as f64);

                // self.gpu_voltage_adjustment
                // .set_value(*clocks_table.gpu_voltage as f64 / 1000.0);

                self.vram_clock_adjustment
                    .set_value(clocks_table.current_max_mem_clock as f64);
            }
        }
    }

    pub fn get_settings(&self) -> ClocksSettings {
        let gpu_clock = self.gpu_clock_adjustment.get_value() as i64;

        let vram_clock = self.vram_clock_adjustment.get_value() as i64;

        let gpu_voltage = (self.gpu_voltage_adjustment.get_value() * 1000.0) as i64;

        ClocksSettings {
            gpu_clock,
            vram_clock,
            gpu_voltage,
        }
    }*/

    pub fn connect_clocks_reset<F: Fn() + 'static + Clone>(&self, f: F) {
        self.reset_button.connect_clicked(move |_| {
            f();
        });
    }

    /*pub fn connect_clocks_changed<F: Fn() + 'static + Clone>(&self, f: F) {
        {
            let f = f.clone();
            self.gpu_clock_adjustment.connect_value_changed(move |_| {
                f();
            });
        }
        {
            let f = f.clone();
            self.vram_clock_adjustment.connect_value_changed(move |_| {
                f();
            });
        }
        {
            self.gpu_voltage_adjustment.connect_value_changed(move |_| {
                f();
            });
        }
    }*/

    pub fn hide(&self) {
        self.container.set_visible(false);
    }

    pub fn show(&self) {
        self.container.set_visible(true);
    }
}
