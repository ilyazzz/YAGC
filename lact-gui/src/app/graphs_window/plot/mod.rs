mod cubic_spline;
mod imp;
mod render;

use std::cell::RefMut;

pub use imp::PlotData;

use gtk::glib::{self, subclass::types::ObjectSubclassIsExt, Object};

glib::wrapper! {
    pub struct Plot(ObjectSubclass<imp::Plot>)
        @extends gtk::Widget;
}

impl Default for Plot {
    fn default() -> Self {
        Object::builder().build()
    }
}

impl Plot {
    pub fn data_mut(&self) -> RefMut<'_, PlotData> {
        self.imp().dirty.set(true);
        self.imp().data.borrow_mut()
    }
}
