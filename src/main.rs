//! RMixer - A Pipewire audio mixer with TUI
//!
//! A low-latency audio mixer application that creates a Pipewire filter node
//! with configurable input and output ports. Features include:
//! - YAML-based configuration for port naming
//! - Real-time level meters with peak hold
//! - Per-channel volume, mute, and solo controls
//! - Terminal-based user interface
//! - Optional HTTP control API for external clients

mod api;
mod audio;
mod config;
mod ipc;
mod mixer;
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

    /// Enable the control API, listening on ADDR (IP:PORT, or PORT for
    /// loopback). Without a value it listens on 127.0.0.1:7700. Requires
    /// --api-key. Traffic is not encrypted, so only bind other addresses on a
    /// trusted network.
    #[arg(
        long,
        value_name = "ADDR",
        num_args = 0..=1,
        default_missing_value = api::DEFAULT_LISTEN
    )]
    api_listen: Option<String>,

    /// Key that clients of the control API must send as `Authorization: Bearer
    /// <KEY>`. Required when the API is enabled. Other local users can see it
    /// in the process list.
    // Values may start with a hyphen: clap would otherwise refuse such a key
    // with an error that prints the start of it.
    #[arg(long, value_name = "KEY", allow_hyphen_values = true)]
    api_key: Option<api::ApiKey>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    // Parse command line arguments
    let args = Args::parse();

    // Initialize logging
    let default_filter = if args.verbose { "debug" } else { "warn" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        // tiny_http logs from its own threads, which would garble the TUI
        .filter_module("tiny_http", log::LevelFilter::Off)
        .init();

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

    // Any problem with the API is a startup error, reported here while the
    // terminal is still in its normal state. The CLI flag takes precedence
    // for this run only: it is not written to the config file. Nor is the
    // key, which goes to the API server alone.
    let api_listen = args.api_listen.as_deref().or(config.api_listen.as_deref());
    let api_server = api::check_api_settings(api_listen, args.api_key)?
        .map(|(listen, key)| {
            config.ensure_unique_channel_names()?;
            api::ApiServer::bind(api::parse_listen_addr(listen)?, key)
        })
        .transpose()?;

    // Create and run the application
    let app = ui::App::new(config)?;
    if let Some(server) = api_server {
        server.spawn(app.core())?;
    }
    app.run()?;

    log::info!("RMixer exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(extra: &[&str]) -> Result<Args, clap::Error> {
        let base = ["rmixer", "--config", "config.yaml"];
        Args::try_parse_from(base.iter().chain(extra))
    }

    #[test]
    fn api_key_is_not_in_the_debug_output_of_args() {
        let args = parse(&["--api-key", "test-key"]).unwrap();
        assert!(args.api_key.is_some());
        assert!(!format!("{args:?}").contains("test-key"));
    }

    #[test]
    fn api_key_may_start_with_a_hyphen() {
        assert!(parse(&["--api-key", "-test-key"])
            .unwrap()
            .api_key
            .is_some());
    }

    #[test]
    fn api_key_is_optional_on_the_command_line() {
        assert!(parse(&[]).unwrap().api_key.is_none());
    }

    #[test]
    fn repeated_api_key_error_does_not_print_it() {
        let error = parse(&["--api-key", "test-key", "--api-key", "test-key"]).unwrap_err();
        assert!(!error.to_string().contains("test-key"));
    }
}
