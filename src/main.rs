//! RMixer - A Pipewire audio mixer with TUI
//!
//! A low-latency audio mixer application that creates a Pipewire filter node
//! with configurable input and output ports. Features include:
//! - YAML-based configuration for port naming
//! - Real-time level meters with peak hold
//! - Per-channel volume, mute, and solo controls
//! - Terminal-based user interface

mod audio;
mod config;
mod ipc;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

/// RMixer - Pipewire Audio Mixer
#[derive(Parser, Debug)]
#[command(name = "rmixer")]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the configuration file (YAML)
    #[arg(short, long)]
    config: PathBuf,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    // Parse command line arguments
    let args = Args::parse();

    // Initialize logging
    if args.verbose {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    }

    log::info!("Starting RMixer");

    // Load configuration
    let config = config::Config::load(&args.config)
        .with_context(|| format!("Failed to load config from {:?}", args.config))?;

    log::info!(
        "Loaded config: client='{}', {} inputs, {} outputs",
        config.client_name,
        config.inputs.len(),
        config.outputs.len()
    );

    // Create and run the application
    let app = ui::App::new(config)?;
    app.run()?;

    log::info!("RMixer exiting");
    Ok(())
}

