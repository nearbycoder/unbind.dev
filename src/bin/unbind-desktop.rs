#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[path = "desktop/platform.rs"]
mod platform;
#[path = "desktop/state.rs"]
mod state;
#[path = "desktop/view.rs"]
mod view;
#[path = "desktop/worker.rs"]
mod worker;

use anyhow::Result;
use rune::{App, AppSpec};
use state::{Page, State};

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [flag] if flag == "--version" || flag == "-V") {
        println!("unbind-desktop {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let incoming = args
        .first()
        .filter(|value| {
            value.starts_with("unbind:")
                || value.starts_with("https://")
                || value.starts_with("http://")
        })
        .map(|_| ());
    let incoming = incoming.map(|()| args.remove(0));
    let headless = !args.is_empty();

    rune::set_app_id(platform::APP_ID);
    // The networking runtime drains sessions after the native event loop returns.
    rune::orderly_exit();
    let _ = rune::set_app_icon(&include_bytes!("../../site/apple-touch-icon.png")[..]);
    let app = App::new(AppSpec {
        min_size: Some((760.0, 590.0)),
        setup: Some(State::setup),
        scenes: &[
            ("share", |s| s.page = Page::Share),
            ("connect", |s| s.page = Page::Connect),
            ("files", |s| s.page = Page::Files),
            ("sessions", |s| s.page = Page::Sessions),
            ("settings", |s| s.page = Page::Settings),
        ],
        ..AppSpec::new("Unbind", (1100.0, 760.0), State::default, view::view)
    })
    .args(args)
    .on_open_url(|s, link, _| s.open_invite(link));

    if headless {
        return app
            .try_run()
            .map_err(|error| anyhow::anyhow!("Open Unbind: {error}"));
    }
    let (engine, commands) = worker::Engine::start(app.handle())?;
    let result = app
        .init(move || State::live(commands, incoming))
        .try_run()
        .map_err(|error| anyhow::anyhow!("Open Unbind: {error}"));
    engine.finish();
    result
}
