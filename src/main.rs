mod config;
mod window_detector;

fn main() {
    let _cfg = config::ConfigManager::new(None);
    let _tracker = window_detector::WindowTracker::new();
}
