//! Configuration module for rmixer
//!
//! Handles loading and parsing YAML configuration files that define
//! the mixer's client name, input channels, and output channels.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Main configuration structure
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// The Pipewire/JACK client name (e.g., "Mixer")
    pub client_name: String,

    /// Input channel configurations
    pub inputs: Vec<ChannelConfig>,

    /// Output channel configurations
    pub outputs: Vec<ChannelConfig>,
    
    /// Path to the config file (not serialized)
    #[serde(skip)]
    pub config_path: Option<String>,
}

/// Configuration for a single channel (input or output)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelConfig {
    /// Display name for the channel
    pub name: String,

    /// Port names to create. Length determines mono (1) or stereo (2)
    /// Ports will be exposed as "{client_name}:{port_name}"
    pub ports: Vec<String>,
    
    /// Volume level in dB (optional, defaults to 0.0)
    #[serde(default)]
    pub volume_db: Option<f32>,
}

impl ChannelConfig {
    /// Returns true if this is a stereo channel (2 ports)
    pub fn is_stereo(&self) -> bool {
        self.ports.len() >= 2
    }

    /// Returns the number of ports (1 for mono, 2 for stereo)
    pub fn port_count(&self) -> usize {
        self.ports.len().min(2)
    }
}

impl Config {
    /// Load configuration from a YAML file
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let mut config: Config = serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        config.config_path = Some(path.to_string_lossy().to_string());
        config.validate()?;
        Ok(config)
    }
    
    /// Save configuration to a YAML file
    pub fn save(&self) -> Result<()> {
        if let Some(ref path) = self.config_path {
            let contents = serde_yaml::to_string(self)
                .context("Failed to serialize config")?;
            fs::write(path, contents)
                .with_context(|| format!("Failed to write config file: {}", path))?;
        }
        Ok(())
    }
    
    /// Update volume levels from mixer state
    pub fn update_volumes(&mut self, input_volumes: &[f32], output_volumes: &[f32]) {
        for (i, vol) in input_volumes.iter().enumerate() {
            if i < self.inputs.len() {
                self.inputs[i].volume_db = Some(*vol);
            }
        }
        for (i, vol) in output_volumes.iter().enumerate() {
            if i < self.outputs.len() {
                self.outputs[i].volume_db = Some(*vol);
            }
        }
    }

    /// Validate the configuration
    fn validate(&self) -> Result<()> {
        if self.client_name.is_empty() {
            anyhow::bail!("client_name cannot be empty");
        }

        if self.inputs.is_empty() {
            anyhow::bail!("At least one input channel is required");
        }

        if self.outputs.is_empty() {
            anyhow::bail!("At least one output channel is required");
        }

        for (i, input) in self.inputs.iter().enumerate() {
            if input.name.is_empty() {
                anyhow::bail!("Input channel {} has empty name", i);
            }
            if input.ports.is_empty() {
                anyhow::bail!("Input channel '{}' has no ports defined", input.name);
            }
            if input.ports.len() > 2 {
                anyhow::bail!(
                    "Input channel '{}' has {} ports, max 2 supported",
                    input.name,
                    input.ports.len()
                );
            }
        }

        for (i, output) in self.outputs.iter().enumerate() {
            if output.name.is_empty() {
                anyhow::bail!("Output channel {} has empty name", i);
            }
            if output.ports.is_empty() {
                anyhow::bail!("Output channel '{}' has no ports defined", output.name);
            }
            if output.ports.len() > 2 {
                anyhow::bail!(
                    "Output channel '{}' has {} ports, max 2 supported",
                    output.name,
                    output.ports.len()
                );
            }
        }

        Ok(())
    }

    /// Get total number of input ports
    pub fn total_input_ports(&self) -> usize {
        self.inputs.iter().map(|c| c.port_count()).sum()
    }

    /// Get total number of output ports
    pub fn total_output_ports(&self) -> usize {
        self.outputs.iter().map(|c| c.port_count()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config() {
        let yaml = r#"
client_name: "Mixer"
inputs:
  - name: "Mic"
    ports: ["capture_1"]
  - name: "Music"
    ports: ["capture_2", "capture_3"]
outputs:
  - name: "Main"
    ports: ["playback_1", "playback_2"]
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.client_name, "Mixer");
        assert_eq!(config.inputs.len(), 2);
        assert_eq!(config.outputs.len(), 1);
        assert!(!config.inputs[0].is_stereo());
        assert!(config.inputs[1].is_stereo());
        assert!(config.outputs[0].is_stereo());
    }
}
