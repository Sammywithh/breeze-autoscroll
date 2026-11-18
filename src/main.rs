mod config;
mod input_manager;
mod window_detector;

fn main() {
    let _cfg = config::ConfigManager::new(None);
    let _tracker = window_detector::WindowTracker::new();
    let _ = input_manager::dummy_indicator_channel();
}
