use daemon::gpu_controller::PowerProfile;
use gtk::*;
use prelude::ComboBoxExtManual;

#[derive(Clone)]
pub struct PowerProfileFrame {
    pub container: Frame,
    combo_box: ComboBoxText,
    description_label: Label,
}

impl PowerProfileFrame {
    pub fn new() -> Self {
        let container = Frame::new(None);

        container.set_shadow_type(ShadowType::None);

        container.set_label_widget(Some(&{
            let label = Label::new(None);
            label.set_markup("<span font_desc='11'><b>Power Profile</b></span>");
            label
        }));
        container.set_label_align(0.2, 0.0);

        let root_box = Box::new(Orientation::Horizontal, 5);

        let combo_box = ComboBoxText::new();

        combo_box.append(Some("0"), "Automatic");
        combo_box.append(Some("1"), "Highest clocks");
        combo_box.append(Some("2"), "Lowest clocks");
        combo_box.append(Some("3"), "Manual");

        root_box.pack_start(&combo_box, false, true, 5);

        let description_label = Label::new(Some("A description is supposed to be here"));

        description_label.set_line_wrap(true);
        //description_label.set_line_wrap_mode(pango::WrapMode::Word);

        root_box.pack_start(&description_label, false, true, 5);

        {
            let description_label = description_label.clone();
            combo_box.connect_changed(move |combobox| match combobox.get_active().unwrap() {
                0 => description_label
                    .set_text("Automatically adjust GPU and VRAM clocks. (Default)"),
                1 => description_label
                    .set_text("Always use the highest clockspeeds for GPU and VRAM."),
                2 => description_label
                    .set_text("Always use the lowest clockspeeds for GPU and VRAM."),
                3 => description_label
                    .set_text("This setting allow you to manually choose enabled power states."),
                _ => unreachable!(),
            });
        }

        container.add(&root_box);
        Self {
            container,
            combo_box,
            description_label,
        }
    }

    pub fn set_active_profile(&self, profile: &PowerProfile) {
        match profile {
            PowerProfile::Auto => self.combo_box.set_active_id(Some("0")),
            PowerProfile::High => self.combo_box.set_active_id(Some("1")),
            PowerProfile::Low => self.combo_box.set_active_id(Some("2")),
            PowerProfile::Manual => self.combo_box.set_active_id(Some("3")),
        };
    }

    pub fn connect_power_profile_changed<F: Fn() + 'static>(&self, f: F) {
        self.combo_box.connect_changed(move |_| {
            f();
        });
    }

    pub fn get_selected_power_profile(&self) -> PowerProfile {
        match self.combo_box.get_active().unwrap() {
            0 => PowerProfile::Auto,
            1 => PowerProfile::High,
            2 => PowerProfile::Low,
            3 => PowerProfile::Manual,
            _ => unreachable!(),
        }
    }

    pub fn show(&self) {
        self.container.set_visible(true);
    }

    pub fn hide(&self) {
        self.container.set_visible(false);
    }

    pub fn get_visibility(&self) -> bool {
        self.container.get_visible()
    }
}
