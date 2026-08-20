//! Binary entry point. Everything of substance lives in the library half of
//! this crate so it can be tested without a window.

fn main() -> eframe::Result {
    init_logging();
    firstlight_view::run(firstlight_view::default_registry())
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("FIRSTLIGHT_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn,firstlight_core=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
