//! newTerm application: window, input, sidebar.
#![deny(unsafe_code)]

mod app;
mod geometry;
mod keys;
mod log;
mod pane;

use std::time::Instant;

use winit::event_loop::EventLoop;

fn main() {
    let t0 = Instant::now();
    // The driver's instance + device creation is the largest startup cost (D-T04-1): start it
    // first so the event loop, window, PTY and font load all overlap it.
    let gpu = nt_render::GpuStart::spawn();
    log::init();
    // Release builds abort on panic (D-T08-2): say where, then die loudly.
    std::panic::set_hook(Box::new(|info| {
        #[expect(clippy::print_stderr, reason = "last words before abort")]
        {
            eprintln!("newterm: panic: {info}");
        }
    }));
    let event_loop = match EventLoop::<app::UserEvent>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            ::log::error!("event loop: {e}");
            std::process::exit(1);
        }
    };
    let mut app = app::App::new(t0, gpu, event_loop.create_proxy());
    if let Err(e) = event_loop.run_app(&mut app) {
        ::log::error!("event loop: {e}");
        std::process::exit(1);
    }
    // Skip GPU and window teardown: the NVIDIA driver spends ~27 ms in it (D-T04-1).
    std::process::exit(app.exit_code);
}
