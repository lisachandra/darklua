mod cli;

use std::process;

use anstyle::{AnsiColor, Color, Style};
use clap::Parser;
use cli::Darklua;
use env_logger::Builder;
use log::Level;

fn main() {
    // Bundling a large project inlines requires recursively, so the depth of the
    // call stack scales with the module graph, not with anything the caller can
    // bound. The main thread's stack is 1 MiB on Windows and 8 MiB on Linux, which
    // is not enough for a project of a few hundred modules: the bundler overflowed
    // partway through. Run the CLI on a thread with a stack large enough that the
    // graph depth stops being the limit.
    const STACK_SIZE: usize = 256 * 1024 * 1024;

    let worker = std::thread::Builder::new()
        .name("darklua".to_owned())
        .stack_size(STACK_SIZE)
        .spawn(run)
        .expect("unable to spawn the darklua worker thread");

    match worker.join() {
        Ok(()) => {}
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn run() {
    let darklua = Darklua::parse();

    let filter = darklua.get_log_level_filter();

    formatted_logger().filter_module("darklua", filter).init();

    match darklua.run() {
        Ok(()) => {}
        Err(err) => {
            process::exit(err.exit_code());
        }
    }
}

fn formatted_logger() -> Builder {
    let mut builder = Builder::new();
    builder.format(|f, record| {
        use std::io::Write;

        let level = record.level();
        let (style, text) = colored_level(level);

        writeln!(f, " {style}{text}{style:#} > {}", record.args())
    });
    builder
}

fn colored_level(level: Level) -> (Style, &'static str) {
    let (color, text) = match level {
        Level::Trace => (AnsiColor::Magenta, "TRACE"),
        Level::Debug => (AnsiColor::Blue, "DEBUG"),
        Level::Info => (AnsiColor::Green, "INFO"),
        Level::Warn => (AnsiColor::Yellow, "WARN"),
        Level::Error => (AnsiColor::Red, "ERROR"),
    };
    (Style::new().fg_color(Some(Color::Ansi(color))), text)
}
