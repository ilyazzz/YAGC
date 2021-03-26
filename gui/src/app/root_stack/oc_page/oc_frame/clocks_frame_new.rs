use gtk::*;

#[derive(Clone)]
pub struct ClocksFrameNew {
    pub container: Box,
}

impl ClocksFrameNew {
    pub fn new() -> Self {
        let container = Box::new(Orientation::Vertical, 5);

        Self { container }
    }
}