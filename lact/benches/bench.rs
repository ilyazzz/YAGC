fn main() {
    // Include crates in the binary
    let _ = lact_daemon::run;
    let _ = lact_gui::run;

    let _ = gtk::init();

    divan::main();
}
