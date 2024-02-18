mod fan_curve_frame;
mod pmfw_frame;

use glib::clone;
use gtk::prelude::*;
use gtk::*;
use lact_client::schema::{
    default_fan_curve, DeviceInfo, DeviceStats, FanControlMode, FanCurveMap, PmfwOptions,
    SystemInfo,
};
use lact_daemon::AMDGPU_FAMILY_GC_11_0_0;
use tracing::debug;

use self::{fan_curve_frame::FanCurveFrame, pmfw_frame::PmfwFrame};
use super::{label_row, values_grid};
use crate::app::page_section::PageSection;

const PMFW_WARNING: &str =
    "Warning: Overclocking support is disabled, fan control functionality is not available.";

#[derive(Debug)]
pub struct ThermalsSettings {
    pub manual_fan_control: bool,
    pub mode: Option<FanControlMode>,
    pub static_speed: Option<f64>,
    pub curve: Option<FanCurveMap>,
    pub pmfw: PmfwOptions,
    pub start_threshold: f64,
    pub stop_threshold: f64,
}

#[derive(Clone)]
pub struct ThermalsPage {
    pub container: Box,
    pmfw_warning_label: Label,
    temperatures_label: Label,
    fan_speed_label: Label,
    pmfw_frame: PmfwFrame,
    fan_static_speed_adjustment: Adjustment,
    fan_curve_frame: FanCurveFrame,
    fan_control_mode_stack: Stack,
    fan_control_mode_stack_switcher: StackSwitcher,

    overdrive_enabled: Option<bool>,
}

impl ThermalsPage {
    pub fn new(system_info: &SystemInfo) -> Self {
        let container = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(15)
            .margin_start(20)
            .margin_end(20)
            .build();

        let pmfw_warning_label = Label::builder()
            .label(PMFW_WARNING)
            .halign(Align::Start)
            .build();
        container.append(&pmfw_warning_label);

        let stats_section = PageSection::new("Statistics");
        let stats_grid = values_grid();

        let temperatures_label = label_row("Temperatures:", &stats_grid, 0, 0, false);
        let fan_speed_label = label_row("Fan speed:", &stats_grid, 1, 0, false);

        stats_section.append(&stats_grid);

        container.append(&stats_section);

        let fan_curve_frame = FanCurveFrame::new();

        let fan_static_speed_frame = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(5)
            .valign(Align::Start)
            .build();
        let fan_static_speed_adjustment = static_speed_adj(&fan_static_speed_frame);

        let pmfw_frame = PmfwFrame::new();

        let fan_control_section = PageSection::new("Fan control");

        let fan_control_mode_stack = Stack::builder().build();
        let fan_control_mode_stack_switcher = StackSwitcher::builder()
            .stack(&fan_control_mode_stack)
            .visible(false)
            .sensitive(false)
            .build();

        fan_control_mode_stack.add_titled(&pmfw_frame.container, Some("automatic"), "Automatic");
        fan_control_mode_stack.add_titled(&fan_curve_frame.container, Some("curve"), "Curve");
        fan_control_mode_stack.add_titled(&fan_static_speed_frame, Some("static"), "Static");

        fan_control_section.append(&fan_control_mode_stack_switcher);
        fan_control_section.append(&fan_control_mode_stack);

        container.append(&fan_control_section);

        fan_control_mode_stack.connect_visible_child_name_notify(|stack| {
            if stack.visible_child_name() == Some("automatic".into()) {
                show_fan_control_warning()
            }
        });

        Self {
            pmfw_warning_label,
            container,
            temperatures_label,
            fan_speed_label,
            fan_static_speed_adjustment,
            fan_curve_frame,
            fan_control_mode_stack,
            fan_control_mode_stack_switcher,
            pmfw_frame,
            overdrive_enabled: system_info.amdgpu_overdrive_enabled,
        }
    }

    pub fn set_info(&self, info: &DeviceInfo) {
        let pmfw_disabled = info.drm_info.as_ref().is_some_and(|info| {
            debug!(
                "family id: {}, overdrive enabled {:?}",
                info.family_id, self.overdrive_enabled
            );
            (info.family_id >= AMDGPU_FAMILY_GC_11_0_0) && (self.overdrive_enabled != Some(true))
        });
        self.pmfw_warning_label.set_visible(pmfw_disabled);

        let sensitive = self.fan_control_mode_stack_switcher.is_sensitive() && !pmfw_disabled;
        self.fan_control_mode_stack_switcher
            .set_sensitive(sensitive);
    }

    pub fn set_stats(&self, stats: &DeviceStats, initial: bool) {
        let mut temperatures: Vec<String> = stats
            .temps
            .iter()
            .filter_map(|(label, temp)| temp.current.map(|current| format!("{label}: {current}°C")))
            .collect();
        temperatures.sort();
        let temperatures_text = if temperatures.is_empty() {
            String::from("No sensors found")
        } else {
            temperatures.join(", ")
        };

        self.temperatures_label
            .set_markup(&format!("<b>{temperatures_text}</b>",));

        match stats.fan.speed_current {
            Some(fan_speed_current) => self.fan_speed_label.set_markup(&format!(
                "<b>{} RPM ({}%)</b>",
                fan_speed_current,
                (fan_speed_current as f64
                    / stats.fan.speed_max.unwrap_or(fan_speed_current) as f64
                    * 100.0)
                    .round()
            )),
            None => self.fan_speed_label.set_text("No fan detected"),
        }

        if initial {
            self.fan_control_mode_stack_switcher.set_visible(true);
            self.fan_control_mode_stack_switcher
                .set_sensitive(stats.fan.speed_current.is_some());

            let child_name = match stats.fan.control_mode {
                Some(mode) if stats.fan.control_enabled => match mode {
                    FanControlMode::Static => "static",
                    FanControlMode::Curve => "curve",
                },
                _ => "automatic",
            };

            self.fan_control_mode_stack
                .set_visible_child_name(child_name);

            if let Some(static_speed) = &stats.fan.static_speed {
                self.fan_static_speed_adjustment
                    .set_value(*static_speed * 100.0);
            }

            if let Some(curve) = &stats.fan.curve {
                self.fan_curve_frame.set_curve(curve);
            }

            if !stats.fan.control_enabled && self.fan_curve_frame.get_curve().is_empty() {
                self.fan_curve_frame.set_curve(&default_fan_curve());
            }

            if let Some(start_threshold) = stats.fan.start_threshold {
                self.fan_curve_frame.set_start_threshold(start_threshold);
            }
            if let Some(stop_threshold) = stats.fan.stop_threshold {
                self.fan_curve_frame.set_stop_threshold(stop_threshold);
            }

            self.pmfw_frame.set_info(&stats.fan.pmfw_info);
        }
    }

    pub fn connect_settings_changed<F: Fn() + 'static + Clone>(&self, f: F) {
        self.fan_control_mode_stack
            .connect_visible_child_name_notify(clone!(@strong f => move |_| {
                f();
            }));

        self.fan_static_speed_adjustment
            .connect_value_changed(clone!(@strong f => move |_| {
                f();
            }));

        self.pmfw_frame.connect_settings_changed(f.clone());

        self.fan_curve_frame.connect_adjusted(move || {
            f();
        });
    }

    pub fn get_thermals_settings(&self) -> Option<ThermalsSettings> {
        if self.fan_control_mode_stack_switcher.is_sensitive() {
            let name = self.fan_control_mode_stack.visible_child_name();
            let name = name
                .as_ref()
                .map(|name| name.as_str())
                .expect("No name on the visible child");
            let (manual_fan_control, mode) = match name {
                "automatic" => (false, None),
                "curve" => (true, Some(FanControlMode::Curve)),
                "static" => (true, Some(FanControlMode::Static)),
                _ => unreachable!(),
            };
            let static_speed = Some(self.fan_static_speed_adjustment.value() / 100.0);
            let curve = self.fan_curve_frame.get_curve();
            let curve = if curve.is_empty() { None } else { Some(curve) };
            let start_threshold = self.fan_curve_frame.get_start_threshold();
            let stop_threshold = self.fan_curve_frame.get_stop_threshold();

            let pmfw = self.pmfw_frame.get_pmfw_options();

            Some(ThermalsSettings {
                manual_fan_control,
                mode,
                static_speed,
                curve,
                pmfw,
                start_threshold,
                stop_threshold,
            })
        } else {
            None
        }
    }

    pub fn connect_reset_pmfw<F: Fn() + 'static + Clone>(&self, f: F) {
        self.pmfw_frame.connect_reset(f);
    }
}

fn static_speed_adj(parent_box: &Box) -> Adjustment {
    let label = Label::builder()
        .label("Speed (in %)")
        .halign(Align::Start)
        .build();

    let adjustment = Adjustment::new(0.0, 0.0, 100.0, 0.1, 1.0, 0.0);

    let scale = Scale::builder()
        .orientation(Orientation::Horizontal)
        .adjustment(&adjustment)
        .hexpand(true)
        .margin_start(5)
        .margin_end(5)
        .build();

    let value_selector = SpinButton::new(Some(&adjustment), 1.0, 1);
    let value_label = Label::new(None);

    let popover = Popover::builder().child(&value_selector).build();
    let value_button = MenuButton::builder()
        .popover(&popover)
        .child(&value_label)
        .build();

    adjustment.connect_value_changed(clone!(@strong value_label => move |adjustment| {
        let value = adjustment.value();
        value_label.set_text(&format!("{value:.1}"));
    }));

    adjustment.set_value(50.0);

    parent_box.append(&label);
    parent_box.append(&scale);
    parent_box.append(&value_button);

    adjustment
}

fn show_fan_control_warning() {
    let diag = MessageDialog::new(None::<&Window>, DialogFlags::empty(), MessageType::Warning, ButtonsType::Ok,
                        "Warning! Due to a driver bug, a reboot may be required for fan control to properly switch back to automatic.");
    diag.run_async(|diag, _| {
        diag.hide();
    })
}
