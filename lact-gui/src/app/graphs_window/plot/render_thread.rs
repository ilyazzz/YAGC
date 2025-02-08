use super::cubic_spline::cubic_spline_interpolation;
use super::to_texture_ext::ToTextureExt;
use super::PlotData;
use anyhow::Context;
use cairo::{Context as CairoContext, ImageSurface};

use gtk::gdk::MemoryTexture;
use itertools::Itertools;
use plotters::prelude::*;
use plotters::style::colors::full_palette::DEEPORANGE_100;
use plotters::style::RelativeSize;
use std::cmp::{max, min};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use thread_priority::{ThreadBuilderExt, ThreadPriority};
use tracing::error;

enum Request {
    Terminate,
    Render(RenderRequest),
}

#[derive(Default)]
pub struct RenderRequest {
    pub title: String,
    pub value_suffix: String,
    pub secondary_value_suffix: String,
    pub y_label_area_relative_size: f64,
    pub secondary_y_label_relative_area_size: f64,

    pub data: PlotData,

    pub width: u32,
    pub height: u32,

    pub supersample_factor: u32,

    pub time_period_seconds: i64,
}

#[derive(Default)]
struct RenderThreadState {
    request_condition_variable: std::sync::Condvar,
    last_texture: Mutex<Option<MemoryTexture>>,
    current_request: Mutex<Option<Request>>,
}

/// A rendering thread that will listen for rendering requests and process them asynchronously.
/// Requests that weren't processed in time or resulted in error are dropped.
pub struct RenderThread {
    /// Shared state is between the main thread and the rendering thread.
    state: Arc<RenderThreadState>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

/// Ensure the rendering thread is terminated properly when the RenderThread object is dropped.
/// We send Request::Terminate to swiftly terminate rendering thread and then join to let it finish last render.
impl Drop for RenderThread {
    fn drop(&mut self) {
        self.state
            .current_request
            .lock()
            .unwrap()
            .replace(Request::Terminate);
        self.state.request_condition_variable.notify_all();

        self.thread_handle.take().map(|handle| handle.join().ok());
    }
}

impl RenderThread {
    pub fn new() -> Self {
        let state = Arc::new(RenderThreadState::default());

        let thread_handle = std::thread::Builder::new()
            .name("Plot-Renderer".to_owned())
            // Render thread is very unimportant, skipping frames and rendering slowly is ok
            .spawn_with_priority(ThreadPriority::Min, {
                let state = state.clone();
                move |_| loop {
                    let RenderThreadState {
                        request_condition_variable,
                        last_texture,
                        current_request,
                    } = &*state;

                    // Wait until there is a new request (blocking if there is none).
                    let mut current_request = request_condition_variable
                        .wait_while(current_request.lock().unwrap(), |pending_request| {
                            pending_request.is_none()
                        })
                        .unwrap();

                    match current_request.take() {
                        Some(Request::Render(render_request)) => {
                            process_request(render_request, last_texture);
                        }
                        // Terminate the thread if a Terminate request is received.
                        Some(Request::Terminate) => break,
                        None => {}
                    }
                }
            })
            .unwrap();

        Self {
            state,
            thread_handle: Some(thread_handle),
        }
    }

    /// Replace the current render request with a new one (effectively dropping possible pending frame)
    /// Returns dropped request if any
    pub fn replace_render_request(&self, request: RenderRequest) -> Option<RenderRequest> {
        let mut current_request = self.state.current_request.lock().unwrap();
        let result = current_request.replace(Request::Render(request));
        self.state.request_condition_variable.notify_one(); // Notify the thread to start rendering.

        match result? {
            Request::Render(render) => Some(render),
            Request::Terminate => None,
        }
    }

    /// Return the last texture.
    /// Requests that weren't processed in time or resulted in error are dropped.
    pub fn get_last_texture(&self) -> Option<MemoryTexture> {
        self.state.last_texture.lock().unwrap().deref().clone()
    }
}

fn process_request(render_request: RenderRequest, last_texture: &Mutex<Option<MemoryTexture>>) {
    /*// Create a new ImageSurface for Cairo rendering.
    let mut surface = ImageSurface::create(
        cairo::Format::ARgb32,
        (render_request.width * render_request.supersample_factor) as i32,
        (render_request.height * render_request.supersample_factor) as i32,
    )
    .unwrap();

    let cairo_context = CairoContext::new(&surface).unwrap();

    // Don't use Cairo's default antialiasing, it makes the lines look too blurry
    // Supersampling is our 2D anti-aliasing solution.
    if render_request.supersample_factor > 1 {
        cairo_context.set_antialias(cairo::Antialias::None);
    }

    let cairo_backend = CairoBackend::new(
        &cairo_context,
        // Supersample the rendering
        (
            render_request.width * render_request.supersample_factor,
            render_request.height * render_request.supersample_factor,
        ),
    )
    .unwrap();

    if let Err(err) = render_request.draw(cairo_backend) {
        error!("Failed to plot chart: {err:?}")
    }

    match (
        surface.to_texture(),
        last_texture.lock().unwrap().deref_mut(),
    ) {
        // Successfully generated a new texture, but the old texture is also there
        (Some(texture), Some(last_texture)) => {
            *last_texture = texture;
        }
        // If texture conversion failed, keep the old texture if it's present.
        (None, None) => {
            error!("Failed to convert cairo surface to gdk texture, not overwriting old one");
        }
        // Update the last texture, if The old texture wasn't ever generated (None),
        // No matter the result of conversion
        (result, last_texture) => {
            *last_texture = result;
        }
    };*/
}

// Implement the default constructor for RenderThread using the `new` method.
impl Default for RenderThread {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderRequest {
    pub fn relative_size(&self, ratio: f64) -> f64 {
        min(self.height, self.width) as f64 * ratio
    }

    // Method to handle the actual drawing of the chart.
    pub fn draw<'a, DB>(&self, backend: DB) -> anyhow::Result<()>
    where
        DB: DrawingBackend + 'a,
        <DB as plotters::prelude::DrawingBackend>::ErrorType: 'static,
    {
        let root = backend.into_drawing_area(); // Create the drawing area.

        let data = &self.data;

        // Determine the start and end dates of the data series.
        let start_date_main = data
            .line_series_iter()
            .filter_map(|(_, data)| Some(data.first()?.0))
            .min()
            .unwrap_or_default();
        let start_date_secondary = data
            .secondary_line_series_iter()
            .filter_map(|(_, data)| Some(data.first()?.0))
            .min()
            .unwrap_or_default();
        let end_date_main = data
            .line_series_iter()
            .map(|(_, value)| value)
            .filter_map(|data| Some(data.first()?.0))
            .max()
            .unwrap_or_default();
        let end_date_secondary = data
            .secondary_line_series_iter()
            .map(|(_, value)| value)
            .filter_map(|data| Some(data.first()?.0))
            .max()
            .unwrap_or_default();

        let start_date = max(start_date_main, start_date_secondary);
        let end_date = max(end_date_main, end_date_secondary);

        // Calculate the maximum value for the y-axis.
        let mut maximum_value = data
            .line_series_iter()
            .flat_map(|(_, data)| data.iter().map(|(_, value)| value))
            .max_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal))
            .cloned()
            .unwrap_or_default();

        // Ensure that the maximum value is at least 100 for better visualization.
        if maximum_value < 100.0f64 {
            maximum_value = 100.0f64;
        }

        root.fill(&WHITE)?; // Fill the background with white color.

        let y_label_area_relative_size =
            if data.line_series.is_empty() && !data.secondary_line_series.is_empty() {
                0.0
            } else {
                self.y_label_area_relative_size
            };

        // Set up the main chart with axes and labels.
        let mut chart = ChartBuilder::on(&root)
            .x_label_area_size(RelativeSize::Smaller(0.05))
            .y_label_area_size(RelativeSize::Smaller(y_label_area_relative_size))
            .right_y_label_area_size(RelativeSize::Smaller(
                self.secondary_y_label_relative_area_size,
            ))
            .margin(RelativeSize::Smaller(0.045))
            .caption(
                self.title.as_str(),
                ("sans-serif", RelativeSize::Smaller(0.08)),
            )
            .build_cartesian_2d(
                start_date..max(end_date, start_date + self.time_period_seconds * 1000),
                0f64..maximum_value,
            )?
            .set_secondary_coord(
                start_date..max(end_date, start_date + self.time_period_seconds * 1000),
                0.0..100.0,
            );

        // Configure the x-axis and y-axis mesh.
        chart
            .configure_mesh()
            .x_label_formatter(&|date_time| {
                let date_time = chrono::DateTime::from_timestamp_millis(*date_time).unwrap();
                date_time.format("%H:%M:%S").to_string()
            })
            .y_label_formatter(&|x| format!("{x}{}", &self.value_suffix))
            .x_labels(5)
            .y_labels(10)
            .label_style(("sans-serif", RelativeSize::Smaller(0.08)))
            .draw()
            .context("Failed to draw mesh")?;

        // Configure the secondary axes (for the secondary y-axis).
        chart
            .configure_secondary_axes()
            .y_label_formatter(&|x: &f64| format!("{x}{}", self.secondary_value_suffix.as_str()))
            .y_labels(10)
            .label_style(("sans-serif", RelativeSize::Smaller(0.08)))
            .draw()
            .context("Failed to draw mesh")?;

        // Draw the throttling histogram as a series of bars.
        if !data.is_empty() {
            chart
                .draw_series(
                    data.throttling_iter()
                        .chunk_by(|(_, _, point)| *point)
                        .into_iter()
                        .filter_map(|(point, group_iter)| point.then_some(group_iter))
                        .filter_map(|mut group_iter| {
                            let first = group_iter.next()?;
                            Some((first, group_iter.last().unwrap_or(first)))
                        })
                        .map(|((start, name, _), (end, _, _))| ((start, end), name))
                        .map(|((start_time, end_time), _)| (start_time, end_time))
                        .sorted_by_key(|&(start_time, _)| start_time)
                        .coalesce(|(start1, end1), (start2, end2)| {
                            if end1 >= start2 {
                                Ok((start1, std::cmp::max(end1, end2)))
                            } else {
                                Err(((start1, end1), (start2, end2)))
                            }
                        })
                        .map(|(start_time, end_time)| {
                            Rectangle::new(
                                [(start_time, 0f64), (end_time, maximum_value)],
                                DEEPORANGE_100.filled(),
                            )
                        }),
                )
                .context("Failed to draw throttling histogram")?;
        }

        // Draw the main line series using cubic spline interpolation.
        for (idx, (caption, data)) in (0..).zip(data.line_series_iter()) {
            chart
                .draw_series(LineSeries::new(
                    cubic_spline_interpolation(data.iter())
                        .into_iter()
                        .flat_map(|((first_time, second_time), segment)| {
                            // Interpolate in intervals of one millisecond.
                            (first_time..second_time).map(move |current_date| {
                                (current_date, segment.evaluate(current_date))
                            })
                        }),
                    Palette99::pick(idx).stroke_width(8),
                ))
                .context("Failed to draw series")?
                .label(caption)
                .legend(move |(x, y)| {
                    let offset = self.relative_size(0.04) as i32;
                    Rectangle::new(
                        [(x - offset, y - offset), (x + offset, y + offset)],
                        Palette99::pick(idx).filled(),
                    )
                });
        }

        // Draw the secondary line series on the secondary y-axis.
        for (idx, (caption, data)) in (0..).zip(data.secondary_line_series_iter()) {
            chart
                .draw_secondary_series(LineSeries::new(
                    cubic_spline_interpolation(data.iter())
                        .into_iter()
                        .flat_map(|((first_time, second_time), segment)| {
                            (first_time..second_time).map(move |current_date| {
                                (current_date, segment.evaluate(current_date))
                            })
                        }),
                    Palette99::pick(idx + 10).stroke_width(8),
                ))
                .context("Failed to draw series")?
                .label(caption)
                .legend(move |(x, y)| {
                    let offset = self.relative_size(0.04) as i32;
                    Rectangle::new(
                        [(x - offset, y - offset), (x + offset, y + offset)],
                        Palette99::pick(idx + 10).filled(),
                    )
                });
        }

        // Configure and draw series labels (the legend).
        chart
            .configure_series_labels()
            .margin(RelativeSize::Smaller(0.10))
            .label_font(("sans-serif", RelativeSize::Smaller(0.08)))
            .position(SeriesLabelPosition::LowerRight)
            .legend_area_size(RelativeSize::Smaller(0.045))
            .background_style(WHITE.mix(0.8))
            .border_style(BLACK)
            .draw()
            .context("Failed to draw series labels")?;

        root.present()?; // Present the final image.
        Ok(())
    }
}

#[cfg(feature = "bench")]
mod benches {
    use super::{process_request, RenderRequest};
    use crate::app::graphs_window::plot::PlotData;
    use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
    use divan::{counter::ItemsCount, Bencher};
    use gtk::prelude::SnapshotExt;
    use plotters_gtk4::SnapshotBackend;
    use std::sync::Mutex;

    #[divan::bench]
    fn render_plot(bencher: Bencher) {
        // let last_texture = &Mutex::new(None);

        bencher
            .with_inputs(sample_plot_data)
            .input_counter(|_| ItemsCount::new(1usize))
            .bench_values(|data| {
                let request = RenderRequest {
                    title: "bench render".into(),
                    value_suffix: "%".into(),
                    secondary_value_suffix: "".into(),
                    y_label_area_relative_size: 1.0,
                    secondary_y_label_relative_area_size: 1.0,
                    data,
                    width: 1920,
                    height: 1080,
                    supersample_factor: 4,
                    time_period_seconds: 60,
                };

                let snapshot = gtk::Snapshot::new();
                snapshot.scale(
                    1.0 / request.supersample_factor as f32,
                    1.0 / request.supersample_factor as f32,
                );
                let backend = SnapshotBackend::new(
                    &snapshot,
                    (
                        request.width * request.supersample_factor,
                        request.height * request.supersample_factor,
                    ),
                );
                request.draw(backend).unwrap();
                // process_request(request, last_texture)
            });
    }

    fn sample_plot_data() -> PlotData {
        let mut data = PlotData::default();

        // Simulate 1 minute plot with 4 values per second
        for sec in 0..60 {
            for milli in [0, 250, 500, 750] {
                let datetime = NaiveDateTime::new(
                    NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
                    NaiveTime::from_hms_milli_opt(0, 0, sec, milli).unwrap(),
                );

                data.push_line_series_with_time("GPU", 100.0, datetime);
                data.push_secondary_line_series_with_time("GPU Secondary", 10.0, datetime);
            }
        }

        data
    }
}
