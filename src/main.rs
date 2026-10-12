mod config;
mod indexer;
mod ui;

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;
use std::os::unix::net::UnixStream;

use clap::Parser;
use gtk::glib;

use crate::config::Config;

/// A fast, floating file launcher for Wayland.
#[derive(Debug, Parser)]
#[command(name = "trandex", version)]
struct Cli {
    /// Run as the resident GTK daemon.
    #[arg(short, long)]
    daemon: bool,
    /// Path to the configuration file.
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,
}

fn main() -> glib::ExitCode {
    let origin = Instant::now();
    let cli = Cli::parse();
    let socket_path = socket_path();
    if !cli.daemon {
        match UnixStream::connect(&socket_path) {
            Ok(mut stream) => {
                use std::io::Write;
                let _ = stream.write_all(b"toggle\n");
                return glib::ExitCode::SUCCESS;
            }
            Err(_) => {
                eprintln!("trandex: daemon not running, start it with: trandex -d");
                return glib::ExitCode::FAILURE;
            }
        }
    }

    if let Err(error) = gtk::init() {
        eprintln!("error: could not initialize GTK: {error}");
        return glib::ExitCode::FAILURE;
    }

    let config_path = match cli.config.or_else(default_config_path) {
        Some(path) => path,
        None => {
            eprintln!("error: could not determine a config path; pass --config <PATH>");
            return glib::ExitCode::FAILURE;
        }
    };

    let config = match Config::load(&config_path) {
        Ok(config) => Rc::new(config),
        Err(error) => {
            eprintln!("error: {error}");
            return glib::ExitCode::FAILURE;
        }
    };

    // A simple overlay does not need the GPU renderer, and skipping GPU
    // initialization shaves a noticeable chunk off cold start. Respect an
    // explicit override from the environment.
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: called before any threads are spawned.
        unsafe { std::env::set_var("GSK_RENDERER", "cairo") };
    }

    let main_loop = glib::MainLoop::new(None, false);
    ui::build(config, config_path, socket_path, origin, main_loop.clone());
    main_loop.run();
    glib::ExitCode::SUCCESS
}

fn socket_path() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(runtime).join("trandex.sock");
    }
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|uid| uid.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    PathBuf::from(format!("/tmp/trandex-{uid}.sock"))
}

/// Default config location: `$XDG_CONFIG_HOME/trandex/config.yaml`, falling
/// back to `$HOME/.config/trandex/config.yaml`.
fn default_config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(xdg).join("trandex").join("config.yaml"));
    }

    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("trandex")
            .join("config.yaml")
    })
}
