pub(crate) mod plot;

use gtk::prelude::*;
use lact_schema::DeviceStats;
use plot::{Plot, PlotData};
use relm4::{ComponentParts, ComponentSender, RelmWidgetExt};
use std::sync::Arc;

pub struct GraphsWindow {
    time_period_seconds_adj: gtk::Adjustment,
    vram_clock_ratio: f64,
}

#[derive(Debug)]
pub enum GraphsWindowMsg {
    Stats(Arc<DeviceStats>),
    VramClockRatio(f64),
    Refresh,
    Show,
    Clear,
}

#[relm4::component(pub)]
impl relm4::Component for GraphsWindow {
    type Init = ();
    type Input = GraphsWindowMsg;
    type Output = ();
    type CommandOutput = ();

    view! {
        gtk::Window {
            set_default_height: 400,
            set_default_width: 1200,
            set_title: Some("Historical data"),
            set_hide_on_close: true,

            gtk::Grid {
                set_margin_all: 10,
                set_row_spacing: 20,
                set_column_spacing: 20,
                set_column_homogeneous: true,

                attach[0, 0, 1, 1]: temperature_plot = &Plot {
                    set_title: "Temperature",
                    set_hexpand: true,
                    set_value_suffix: "°C",
                    set_y_label_area_relative_size: 0.15,
                    #[watch]
                    set_time_period_seconds: model.time_period_seconds_adj.value() as i64,
                },

                attach[0, 1, 1, 1]: fan_plot = &Plot {
                    set_title: "Fan speed",
                    set_hexpand: true,
                    set_value_suffix: "RPM",
                    set_y_label_area_relative_size: 0.25,
                    set_secondary_y_label_area_relative_size: 0.15,
                    #[watch]
                    set_time_period_seconds: model.time_period_seconds_adj.value() as i64,
                },

                attach[1, 0, 1, 1]: clockspeed_plot = &Plot {
                    set_title: "Clockspeed",
                    set_hexpand: true,
                    set_value_suffix: "MHz",
                    set_y_label_area_relative_size: 0.3,
                    #[watch]
                    set_time_period_seconds: model.time_period_seconds_adj.value() as i64,
                },

                attach[1, 1, 1, 1]: power_plot = &Plot {
                    set_title: "Power usage",
                    set_hexpand: true,
                    set_value_suffix: "W",
                    set_y_label_area_relative_size: 0.2,
                    #[watch]
                    set_time_period_seconds: model.time_period_seconds_adj.value() as i64,
                },

                attach[1, 2, 1, 1] = &gtk::Box {
                    set_halign: gtk::Align::End,
                    set_spacing: 5,

                    gtk::Label {
                        set_label: "Time period (seconds):"
                    },

                    gtk::SpinButton {
                        set_adjustment: &model.time_period_seconds_adj,
                    },
                },
            },

        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let time_period_seconds_adj = gtk::Adjustment::new(60.0, 15.0, 3601.0, 1.0, 1.0, 1.0);

        time_period_seconds_adj.connect_value_changed(move |_| {
            sender.input(GraphsWindowMsg::Refresh);
        });

        let model = Self {
            time_period_seconds_adj,
            vram_clock_ratio: 1.0,
        };

        let widgets = view_output!();

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            GraphsWindowMsg::Refresh => {}
            GraphsWindowMsg::Show => {
                root.show();
            }
            GraphsWindowMsg::VramClockRatio(ratio) => {
                self.vram_clock_ratio = ratio;
            }
            GraphsWindowMsg::Stats(stats) => {
                let mut temperature_plot = widgets.temperature_plot.data_mut();
                let mut clockspeed_plot = widgets.clockspeed_plot.data_mut();
                let mut power_plot = widgets.power_plot.data_mut();
                let mut fan_plot = widgets.fan_plot.data_mut();

                let throttling_plots =
                    [&mut temperature_plot, &mut clockspeed_plot, &mut power_plot];
                match &stats.throttle_info {
                    Some(throttle_info) => {
                        if throttle_info.is_empty() {
                            for plot in throttling_plots {
                                plot.push_throttling("No", false);
                            }
                        } else {
                            let type_text: Vec<String> = throttle_info
                                .iter()
                                .map(|(throttle_type, details)| {
                                    format!("{throttle_type} ({})", details.join(", "))
                                })
                                .collect();

                            let text = type_text.join(", ");

                            for plot in throttling_plots {
                                plot.push_throttling(&text, true);
                            }
                        }
                    }
                    None => {
                        for plot in throttling_plots {
                            plot.push_throttling("Unknown", false);
                        }
                    }
                }

                for (name, value) in &stats.temps {
                    temperature_plot.push_line_series(name, value.current.unwrap_or(0.0) as f64);
                }

                if let Some(average) = stats.power.average {
                    power_plot.push_line_series("Average", average);
                }
                if let Some(current) = stats.power.current {
                    power_plot.push_line_series("Current", current);
                }
                if let Some(limit) = stats.power.cap_current {
                    power_plot.push_line_series("Limit", limit);
                }

                if let Some(point) = stats.clockspeed.gpu_clockspeed {
                    clockspeed_plot.push_line_series("GPU (Avg)", point as f64);
                }
                if let Some(point) = stats.clockspeed.current_gfxclk {
                    clockspeed_plot.push_line_series("GPU (Trgt)", point as f64);
                }
                if let Some(point) = stats.clockspeed.vram_clockspeed {
                    clockspeed_plot.push_line_series("VRAM", point as f64 * self.vram_clock_ratio);
                }

                if let Some(max_speed) = stats.fan.speed_max {
                    fan_plot.push_line_series("Maximum", max_speed as f64);
                }
                if let Some(min_speed) = stats.fan.speed_min {
                    fan_plot.push_line_series("Minimum", min_speed as f64);
                }

                if let Some(current_speed) = stats.fan.speed_current {
                    fan_plot.push_line_series("Current", current_speed as f64);
                }

                if let Some(pwm) = stats.fan.pwm_current {
                    fan_plot.push_secondary_line_series(
                        "Percentage",
                        (pwm as f64 / u8::MAX as f64) * 100.0,
                    );
                }

                let time_period_seconds = self.time_period_seconds_adj.value() as i64;
                temperature_plot.trim_data(time_period_seconds);
                clockspeed_plot.trim_data(time_period_seconds);
                power_plot.trim_data(time_period_seconds);
                fan_plot.trim_data(time_period_seconds);

                Self::queue_plots_draw(widgets);
            }
            GraphsWindowMsg::Clear => {
                *widgets.temperature_plot.data_mut() = PlotData::default();
                *widgets.clockspeed_plot.data_mut() = PlotData::default();
                *widgets.power_plot.data_mut() = PlotData::default();
                *widgets.fan_plot.data_mut() = PlotData::default();

                Self::queue_plots_draw(widgets);
            }
        }

        self.update_view(widgets, sender);
    }
}

impl GraphsWindow {
    fn queue_plots_draw(widgets: &<Self as relm4::Component>::Widgets) {
        widgets.temperature_plot.queue_draw();
        widgets.clockspeed_plot.queue_draw();
        widgets.power_plot.queue_draw();
        widgets.fan_plot.queue_draw();
    }
}
