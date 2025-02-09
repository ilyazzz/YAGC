use chrono::NaiveDateTime;
use egui_plot::AxisHints;
use egui_plot::PlotBounds;
use egui_plot::PlotPoint;
use egui_plot::PlotPoints;
use glib::Properties;

use gtk::{glib, prelude::*, subclass::prelude::*};
use gtk_egui_area::egui;
use gtk_egui_area::EguiArea;

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;

#[derive(Properties, Default)]
#[properties(wrapper_type = super::Plot)]
pub struct Plot {
    #[property(get, set)]
    title: RefCell<String>,
    #[property(get, set)]
    value_suffix: RefCell<String>,
    #[property(get, set)]
    secondary_value_suffix: RefCell<String>,
    #[property(get, set)]
    y_label_area_relative_size: Cell<f64>,
    #[property(get, set)]
    secondary_y_label_area_relative_size: Cell<f64>,
    pub(super) data: RefCell<PlotData>,
    pub(super) dirty: Cell<bool>,
    #[property(get, set)]
    time_period_seconds: Cell<i64>,
}

#[glib::object_subclass]
impl ObjectSubclass for Plot {
    const NAME: &'static str = "Plot";
    type Type = super::Plot;
    type ParentType = gtk::Box;
}

#[glib::derived_properties]
impl ObjectImpl for Plot {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj().clone();
        obj.set_height_request(250);
        obj.set_hexpand(true);
        obj.set_vexpand(true);

        let area = EguiArea::new(move |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let data = obj.imp().data.borrow();

                egui_plot::Plot::new(obj.title())
                    .legend(egui_plot::Legend::default().position(egui_plot::Corner::RightBottom))
                    .custom_x_axes(vec![AxisHints::new_x().label("Time").formatter(
                        |point, _| {
                            let time = chrono::DateTime::from_timestamp_millis(point.value as i64)
                                .unwrap()
                                .naive_local()
                                .time();
                            time.to_string()
                        },
                    )])
                    .show(ui, |plot_ui| {
                        let max_x = data
                            .line_series_iter()
                            .next()
                            .and_then(|(_, points)| points.last())
                            .map(|point| point.x)
                            .unwrap_or_default();
                        let min_x = max_x - (obj.time_period_seconds() * 1000) as f64;

                        let bounds = PlotBounds::from_min_max(
                            [min_x, -f64::INFINITY],
                            [max_x, f64::INFINITY],
                        );
                        plot_ui.set_plot_bounds(bounds);
                        plot_ui.set_auto_bounds([false, true]);

                        for (name, points) in data.line_series_iter() {
                            plot_ui.line(
                                egui_plot::Line::new(PlotPoints::Borrowed(points)).name(name),
                            );
                        }
                    })
            });
        });
        self.obj().append(&area);
    }
}

impl WidgetImpl for Plot {}

impl BoxImpl for Plot {}

#[derive(Default, Clone)]
pub struct PlotData {
    pub(super) line_series: BTreeMap<String, Vec<PlotPoint>>,
    pub(super) secondary_line_series: BTreeMap<String, Vec<PlotPoint>>,
    pub(super) throttling: Vec<(i64, (String, bool))>,
}

impl PlotData {
    pub fn push_line_series(&mut self, name: &str, point: f64) {
        self.push_line_series_with_time(name, point, chrono::Local::now().naive_local());
    }

    pub fn push_secondary_line_series(&mut self, name: &str, point: f64) {
        self.push_secondary_line_series_with_time(name, point, chrono::Local::now().naive_local());
    }

    fn push_line_series_with_time(&mut self, name: &str, point: f64, time: NaiveDateTime) {
        self.line_series
            .entry(name.to_owned())
            .or_default()
            .push(PlotPoint::new(
                time.and_utc().timestamp_millis() as f64,
                point,
            ));
    }

    pub fn push_secondary_line_series_with_time(
        &mut self,
        name: &str,
        point: f64,
        time: NaiveDateTime,
    ) {
        self.secondary_line_series
            .entry(name.to_owned())
            .or_default()
            .push(PlotPoint::new(
                time.and_utc().timestamp_millis() as f64,
                point,
            ));
    }

    pub fn push_throttling(&mut self, name: &str, point: bool) {
        self.throttling.push((
            chrono::Local::now()
                .naive_local()
                .and_utc()
                .timestamp_millis(),
            (name.to_owned(), point),
        ));
    }

    pub fn line_series_iter(&self) -> impl Iterator<Item = (&String, &Vec<PlotPoint>)> {
        self.line_series.iter()
    }

    pub fn secondary_line_series_iter(&self) -> impl Iterator<Item = (&String, &Vec<PlotPoint>)> {
        self.secondary_line_series.iter()
    }

    pub fn throttling_iter(&self) -> impl Iterator<Item = (i64, &str, bool)> {
        self.throttling
            .iter()
            .map(|(time, (name, point))| (*time, name.as_str(), *point))
    }

    pub fn trim_data(&mut self, last_seconds: i64) {
        // Limit data to N seconds
        for data in self.line_series.values_mut() {
            let maximum_point = data.last().map(|point| point.x as i64).unwrap_or_default();

            data.retain(|point| ((maximum_point - point.x as i64) / 1000) < last_seconds);
        }

        self.line_series.retain(|_, data| !data.is_empty());

        for data in self.secondary_line_series.values_mut() {
            let maximum_point = data.last().map(|point| point.x as i64).unwrap_or_default();

            data.retain(|point| ((maximum_point - point.x as i64) / 1000) < last_seconds);
        }

        self.secondary_line_series
            .retain(|_, data| !data.is_empty());

        // Limit data to N seconds
        let maximum_point = self
            .throttling
            .last()
            .map(|(date_time, _)| *date_time)
            .unwrap_or_default();

        self.throttling
            .retain(|(time_point, _)| ((maximum_point - *time_point) / 1000) < last_seconds);
    }

    pub fn is_empty(&self) -> bool {
        self.line_series.is_empty() && self.secondary_line_series.is_empty()
    }
}
