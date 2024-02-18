mod point_adjustment;

use self::point_adjustment::PointAdjustment;
use glib::clone;
use gtk::graphene::Point;
use gtk::gsk::Transform;
use gtk::prelude::*;
use gtk::*;
use lact_client::schema::{default_fan_curve, FanCurveMap};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(Clone)]
pub struct FanCurveFrame {
    pub container: Box,
    curve_container: Frame,
    points: Rc<RefCell<Vec<PointAdjustment>>>,
    start_threshold_adjustment: Adjustment,
    stop_threshold_adjustment: Adjustment,
}

impl FanCurveFrame {
    pub fn new() -> Self {
        let root_box = Box::new(Orientation::Vertical, 5);

        let hbox = Box::new(Orientation::Horizontal, 5);

        let curve_container = Frame::new(Some("Fan Curve"));
        curve_container.set_hexpand(true);

        curve_container.set_margin_start(10);
        curve_container.set_margin_end(10);
        curve_container.set_margin_top(10);

        let ratio_title_label = Label::builder().label("Fan speed (%)").build();

        let fixed = Fixed::new();
        fixed.put(&ratio_title_label, 0.0, 0.0);

        // This is a workaround to rotate the label that only looks good at the default window size
        // Unfortunately there's no other way to do this (short of implementing custom rendering for a widget) as gtk4 removed the `angle` property for labels
        let rotation_transform = Transform::new()
            .rotate(-90.0)
            .translate(&Point::new(-200.0, 10.0));
        fixed.set_child_transform(&ratio_title_label, Some(&rotation_transform));

        hbox.append(&fixed);
        hbox.append(&curve_container);

        let temperature_title_label = Label::new(Some("Temperature (°C)"));

        let buttons_box = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(5)
            .halign(Align::End)
            .build();

        let add_button = Button::builder().icon_name("list-add-symbolic").build();
        let remove_button = Button::builder().icon_name("list-remove-symbolic").build();
        let default_button = Button::builder().label("Default").build();

        buttons_box.append(&default_button);
        buttons_box.append(&remove_button);
        buttons_box.append(&add_button);

        root_box.append(&hbox);
        root_box.append(&temperature_title_label);
        root_box.append(&buttons_box);

        let start_threshold_adjustment = basic_adj(&root_box, "Fan start threshold");
        let stop_threshold_adjustment = basic_adj(&root_box, "Fan stop threshold");

        let points = Rc::new(RefCell::new(Vec::new()));

        let curve_frame = Self {
            container: root_box,
            curve_container,
            points,
            start_threshold_adjustment,
            stop_threshold_adjustment,
        };

        default_button.connect_clicked(clone!(@strong curve_frame => move |_| {
            let curve = default_fan_curve();
            curve_frame.set_curve(&curve);
        }));

        add_button.connect_clicked(clone!(@strong curve_frame  => move |_| {
            curve_frame.add_point();
        }));

        remove_button.connect_clicked(clone!(@strong curve_frame  => move |_| {
            curve_frame.remove_point();
        }));

        curve_frame
    }

    fn add_point(&self) {
        let mut curve = self.get_curve();
        if let Some((temperature, ratio)) = curve.iter().last() {
            curve.insert(temperature + 5, *ratio);
            self.set_curve(&curve);
        } else {
            curve.insert(50, 0.5);
            self.set_curve(&curve);
        }
    }

    fn remove_point(&self) {
        let mut curve = self.get_curve();
        curve.pop_last();
        self.set_curve(&curve);
    }

    fn notify_changed(&self) {
        if let Some(point) = self.points.borrow().first() {
            point.ratio.emit_by_name::<()>("value-changed", &[]);
        }
    }

    pub fn set_curve(&self, curve: &FanCurveMap) {
        // Notify that the values were changed when the entire curve is overwritten, e.g. when resetting to default
        self.notify_changed();

        let points_container = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(5)
            .vexpand(true)
            .build();

        let mut adjustments = Vec::with_capacity(curve.len());

        for (temperature, ratio) in curve {
            let adjustment = PointAdjustment::new(&points_container, *ratio, *temperature);
            adjustments.push(adjustment);
        }

        self.points.replace(adjustments);
        self.curve_container.set_child(Some(&points_container));
    }

    pub fn get_curve(&self) -> FanCurveMap {
        let mut curve = BTreeMap::new();

        let points = self.points.borrow();
        for point in &*points {
            let temperature = point.temperature.value() as i32;
            let ratio = point.ratio.value() as f32;
            curve.insert(temperature, ratio);
        }

        curve
    }

    pub fn set_start_threshold(&self, value: f64) {
        self.start_threshold_adjustment.set_value(value * 100.0);
    }

    pub fn set_stop_threshold(&self, value: f64) {
        self.stop_threshold_adjustment.set_value(value * 100.0);
    }

    pub fn get_start_threshold(&self) -> f64 {
        self.start_threshold_adjustment.value() / 100.0
    }

    pub fn get_stop_threshold(&self) -> f64 {
        self.stop_threshold_adjustment.value() / 100.0
    }

    pub fn connect_adjusted<F: Fn() + 'static + Clone>(&self, f: F) {
        let closure = clone!(@strong f => move |_: &Adjustment| {
            f();
        });

        for point in &*self.points.borrow() {
            point.ratio.connect_value_changed(closure.clone());
            point.temperature.connect_value_changed(closure.clone());
        }

        self.start_threshold_adjustment
            .connect_value_changed(closure.clone());
        self.stop_threshold_adjustment
            .connect_value_changed(closure);
    }
}

fn basic_adj(parent_box: &Box, label: &str) -> Adjustment {
    let hbox = Box::new(Orientation::Horizontal, 5);

    let label = Label::builder().label(label).halign(Align::Start).build();

    let adjustment = Adjustment::new(0.0, 0.0, 100.0, 0.1, 1.0, 0.0);

    let scale = Scale::builder()
        .orientation(Orientation::Horizontal)
        .adjustment(&adjustment)
        .hexpand(true)
        .margin_start(5)
        .margin_end(5)
        .build();

    let value_selector = SpinButton::new(Some(&adjustment), 1.0, 1);
    let value_label = Label::new(Some("0.0"));

    let popover = Popover::builder().child(&value_selector).build();
    let value_button = MenuButton::builder()
        .popover(&popover)
        .child(&value_label)
        .build();

    adjustment.connect_value_changed(clone!(@strong value_label => move |adjustment| {
        let value = adjustment.value();
        value_label.set_text(&format!("{value:.1}"));
    }));

    hbox.append(&label);
    hbox.append(&scale);
    hbox.append(&value_button);

    parent_box.append(&hbox);

    adjustment
}

#[cfg(all(test, feature = "gtk-tests"))]
mod tests {
    use super::FanCurveFrame;
    use lact_client::schema::default_fan_curve;
    use pretty_assertions::assert_eq;

    #[test]
    fn set_get_curve() {
        gtk::init().unwrap();

        let curve = default_fan_curve();
        let frame = FanCurveFrame::new();
        frame.set_curve(&curve);
        let received_curve = frame.get_curve();
        assert_eq!(received_curve, curve);
    }
}
