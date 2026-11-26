#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `args_os`: `args` panics on a non-Unicode argument; the CLI rejects those as usage errors.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let code = match args.first().map(|a| a.to_str()) {
        None => match borderless_shatter::tray::run() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("fatal: {e:#}");
                1
            }
        },
        Some(Some("gui")) => match borderless_shatter::gui::run() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("fatal: {e:#}");
                1
            }
        },
        Some(Some("mcp")) => match borderless_shatter::mcp::run() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("fatal: {e:#}");
                1
            }
        },
        Some(_) => borderless_shatter::cli::run(args),
    };
    std::process::exit(code);
}
