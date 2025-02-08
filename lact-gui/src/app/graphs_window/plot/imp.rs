use chrono::NaiveDateTime;
use glib::Properties;

use gtk::{glib, prelude::*, subclass::prelude::*};
use plotters_gtk4::SnapshotBackend;

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;

use super::render_thread::{RenderRequest, RenderThread};

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
    // render_thread: RenderThread,
    #[property(get, set)]
    time_period_seconds: Cell<i64>,
}

#[glib::object_subclass]
impl ObjectSubclass for Plot {
    const NAME: &'static str = "Plot";
    type Type = super::Plot;
    type ParentType = gtk::Widget;
}

#[glib::derived_properties]
impl ObjectImpl for Plot {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_height_request(250);
        obj.set_hexpand(true);
        obj.set_vexpand(true);
    }
}

impl WidgetImpl for Plot {
    fn snapshot(&self, snapshot: &gtk::Snapshot) {
        snapshot.scale(0.25, 0.25);

        let width = self.obj().width() as u32;
        let height = self.obj().height() as u32;

        if width == 0 || height == 0 {
            return;
        }
        let request = RenderRequest {
            data: self.data.borrow().clone(),
            width,
            height,
            title: self.title.borrow().clone(),
            value_suffix: self.value_suffix.borrow().clone(),
            secondary_value_suffix: self.secondary_value_suffix.borrow().clone(),
            y_label_area_relative_size: self.y_label_area_relative_size.get(),
            secondary_y_label_relative_area_size: self.secondary_y_label_area_relative_size.get(),
            supersample_factor: 4,
            time_period_seconds: self.time_period_seconds.get(),
        };
        let backend = SnapshotBackend::new(snapshot, (width * 4, height * 4));
        request.draw(backend).unwrap();

        // let last_texture = self.render_thread.get_last_texture();
        // let size_changed = last_texture
        //     .as_ref()
        //     .map(|texture| (texture.width() as u32, texture.height() as u32) != (width, height))
        //     .unwrap_or(true);

        // if self.dirty.replace(false) || size_changed {
        //     self.render_thread.replace_render_request(RenderRequest {
        //         data: self.data.borrow().clone(),
        //         width,
        //         height,
        //         title: self.title.borrow().clone(),
        //         value_suffix: self.value_suffix.borrow().clone(),
        //         secondary_value_suffix: self.secondary_value_suffix.borrow().clone(),
        //         y_label_area_relative_size: self.y_label_area_relative_size.get(),
        //         secondary_y_label_relative_area_size: self
        //             .secondary_y_label_area_relative_size
        //             .get(),
        //         supersample_factor: 4,
        //         time_period_seconds: self.time_period_seconds.get(),
        //     });
        // }

        // // Rendering is always behind by at least one frame, but it's not an issue
        // if let Some(texture) = last_texture {
        //     let bounds = gtk::graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
        //     // Uses by default Trillinear texture filtering, which is quite good at 4x supersampling
        //     snapshot.append_texture(&texture, &bounds);
        // }
    }
}

#[derive(Default, Clone)]
pub struct PlotData {
    pub(super) line_series: BTreeMap<String, Vec<(i64, f64)>>,
    pub(super) secondary_line_series: BTreeMap<String, Vec<(i64, f64)>>,
    pub(super) throttling: Vec<(i64, (String, bool))>,
}

impl PlotData {
    pub fn push_line_series(&mut self, name: &str, point: f64) {
        self.push_line_series_with_time(name, point, chrono::Local::now().naive_local());
    }

    pub fn push_secondary_line_series(&mut self, name: &str, point: f64) {
        self.push_secondary_line_series_with_time(name, point, chrono::Local::now().naive_local());
    }

    pub(super) fn push_line_series_with_time(
        &mut self,
        name: &str,
        point: f64,
        time: NaiveDateTime,
    ) {
        self.line_series
            .entry(name.to_owned())
            .or_default()
            .push((time.and_utc().timestamp_millis(), point));
    }

    pub(super) fn push_secondary_line_series_with_time(
        &mut self,
        name: &str,
        point: f64,
        time: NaiveDateTime,
    ) {
        self.secondary_line_series
            .entry(name.to_owned())
            .or_default()
            .push((time.and_utc().timestamp_millis(), point));
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

    pub fn line_series_iter(&self) -> impl Iterator<Item = (&String, &Vec<(i64, f64)>)> {
        self.line_series.iter()
    }

    pub fn secondary_line_series_iter(&self) -> impl Iterator<Item = (&String, &Vec<(i64, f64)>)> {
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
            let maximum_point = data
                .last()
                .map(|(date_time, _)| *date_time)
                .unwrap_or_default();

            data.retain(|(time_point, _)| ((maximum_point - *time_point) / 1000) < last_seconds);
        }

        self.line_series.retain(|_, data| !data.is_empty());

        for data in self.secondary_line_series.values_mut() {
            let maximum_point = data
                .last()
                .map(|(date_time, _)| *date_time)
                .unwrap_or_default();

            data.retain(|(time_point, _)| ((maximum_point - *time_point) / 1000) < last_seconds);
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
