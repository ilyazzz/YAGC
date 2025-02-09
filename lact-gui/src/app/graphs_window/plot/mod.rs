// mod cubic_spline;
mod imp;
// mod render_thread;
// mod to_texture_ext;

pub use imp::PlotData;
use std::cell::RefMut;

use gtk::glib::{self, subclass::types::ObjectSubclassIsExt, Object};

glib::wrapper! {
    pub struct Plot(ObjectSubclass<imp::Plot>)
        @extends gtk::Widget, gtk::Box;
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
