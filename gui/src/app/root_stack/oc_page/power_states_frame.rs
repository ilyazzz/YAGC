use gtk::*;

#[derive(Clone)]
pub struct PowerStatesFrame {
    pub container: Frame,
}

impl PowerStatesFrame {
    pub fn new() -> Self {
        let container = Frame::new(None);

        container.set_shadow_type(ShadowType::None);

        container.set_label_widget(Some(&{
            let label = Label::new(None);
            label.set_markup("<span font_desc='11'><b>Power States</b></span>");
            label
        }));
        container.set_label_align(0.2, 0.0);

        Self { container }
    }

    pub fn hide(&self) {
        self.container.set_visible(false);
    }

    pub fn show(&self) {
        self.container.set_visible(true);
    }
}
