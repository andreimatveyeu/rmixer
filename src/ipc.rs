//! Inter-process communication types for rmixer
//!
//! Defines lock-free communication structures between the audio thread
//! and the mixer core and UI, for real-time safe operation.

use std::time::Instant;

use crate::config::{ChannelConfig, Config};

/// Volume limits in dB
pub const VOLUME_MIN_DB: f32 = -60.0;
pub const VOLUME_MAX_DB: f32 = 12.0;
pub const VOLUME_STEP_DB: f32 = 0.5;

/// Default volume in dB
pub const VOLUME_DEFAULT_DB: f32 = 0.0;

/// Meter data sent from audio thread to UI thread
#[derive(Debug, Clone, Copy)]
pub struct MeterData {
    /// Channel index this meter data belongs to
    pub channel_index: usize,

    /// Peak levels for each port (up to 2 for stereo)
    /// Values are in linear scale (0.0 to 1.0+, can exceed 1.0 for clipping)
    pub peaks: [f32; 2],

    /// Number of valid peaks (1 for mono, 2 for stereo)
    pub port_count: usize,
}

impl MeterData {
    /// Convert a linear peak value to dB
    pub fn linear_to_db(linear: f32) -> f32 {
        if linear <= 0.0 {
            VOLUME_MIN_DB
        } else {
            20.0 * linear.log10()
        }
    }

    /// Convert dB to linear scale
    pub fn db_to_linear(db: f32) -> f32 {
        10.0_f32.powf(db / 20.0)
    }
}

/// Whether a channel is an input or an output
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Input,
    Output,
}

/// Control message sent from the mixer core to the audio thread.
///
/// Every message carries an absolute value, so its effect does not depend on
/// the state of the audio thread when it arrives.
#[derive(Debug, Clone, Copy)]
pub enum ControlMsg {
    /// Set volume for an input channel (index, volume in dB)
    SetInputVolume { channel: usize, volume_db: f32 },

    /// Set volume for an output channel (index, volume in dB)
    SetOutputVolume { channel: usize, volume_db: f32 },

    /// Set mute for an input channel
    SetInputMute { channel: usize, muted: bool },

    /// Set mute for an output channel
    SetOutputMute { channel: usize, muted: bool },

    /// Set solo for an input channel
    SetInputSolo { channel: usize, soloed: bool },
}

/// State of a single channel (shared representation for UI)
#[derive(Debug, Clone)]
pub struct ChannelState {
    /// Channel name from config
    pub name: String,

    /// Number of ports (1=mono, 2=stereo)
    pub port_count: usize,

    /// Current volume in dB (-60 to +12)
    pub volume_db: f32,

    /// Cached linear gain matching `volume_db`, so the real-time audio
    /// callback never has to call `powf`. Kept in sync via `set_volume_db`.
    volume_linear: f32,

    /// Whether the channel is muted
    pub muted: bool,

    /// Whether the channel is soloed
    pub soloed: bool,

    /// Current peak levels (linear, 0.0-1.0+)
    pub current_peaks: [f32; 2],

    /// Peak hold levels (linear, 0.0-1.0+)
    pub peak_hold: [f32; 2],

    /// Timestamp of last peak hold update
    pub peak_hold_time: [Instant; 2],
}

impl ChannelState {
    /// Create a new channel state
    pub fn new(name: String, port_count: usize) -> Self {
        let now = Instant::now();
        Self {
            name,
            port_count,
            volume_db: VOLUME_DEFAULT_DB,
            volume_linear: MeterData::db_to_linear(VOLUME_DEFAULT_DB),
            muted: false,
            soloed: false,
            current_peaks: [0.0; 2],
            peak_hold: [0.0; 2],
            peak_hold_time: [now; 2],
        }
    }

    /// Create a channel state from its configuration, restoring the saved
    /// volume if there is one.
    fn from_config(config: &ChannelConfig) -> Self {
        let mut state = Self::new(config.name.clone(), config.port_count());
        if let Some(volume_db) = config.volume_db {
            state.set_volume_db(volume_db);
        }
        state
    }

    /// Copy volume, mute and solo from `other`, leaving the meters alone.
    pub fn copy_settings_from(&mut self, other: &ChannelState) {
        self.volume_db = other.volume_db;
        self.volume_linear = other.volume_linear;
        self.muted = other.muted;
        self.soloed = other.soloed;
    }

    /// Update meter data with new peaks
    pub fn update_meter(&mut self, peaks: [f32; 2], peak_hold_duration_secs: f32) {
        let now = Instant::now();

        for i in 0..self.port_count {
            self.current_peaks[i] = peaks[i];

            // Update peak hold if new peak is higher or hold has expired
            if peaks[i] > self.peak_hold[i] {
                self.peak_hold[i] = peaks[i];
                self.peak_hold_time[i] = now;
            } else if now.duration_since(self.peak_hold_time[i]).as_secs_f32()
                > peak_hold_duration_secs
            {
                // Decay peak hold
                self.peak_hold[i] = peaks[i];
                self.peak_hold_time[i] = now;
            }
        }
    }

    /// Set volume in dB (clamped), refreshing the cached linear gain.
    pub fn set_volume_db(&mut self, db: f32) {
        self.volume_db = db.clamp(VOLUME_MIN_DB, VOLUME_MAX_DB);
        self.volume_linear = MeterData::db_to_linear(self.volume_db);
    }

    /// Cached linear gain for `volume_db` (ignores mute/solo).
    pub fn volume_linear(&self) -> f32 {
        self.volume_linear
    }

    /// Get volume as linear gain
    pub fn get_linear_gain(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.volume_linear
        }
    }
}

/// Mixer state containing all channel states
#[derive(Debug, Clone)]
pub struct MixerState {
    pub inputs: Vec<ChannelState>,
    pub outputs: Vec<ChannelState>,
}

impl MixerState {
    /// Build the initial state described by a configuration.
    pub fn from_config(config: &Config) -> Self {
        Self {
            inputs: config.inputs.iter().map(ChannelState::from_config).collect(),
            outputs: config.outputs.iter().map(ChannelState::from_config).collect(),
        }
    }

    /// All channels of one kind.
    pub fn channels(&self, kind: ChannelKind) -> &[ChannelState] {
        match kind {
            ChannelKind::Input => &self.inputs,
            ChannelKind::Output => &self.outputs,
        }
    }

    /// Apply a control message. Runs on the audio thread, so it must stay
    /// lock-free and allocation-free. Invalid channel indices are ignored.
    pub fn apply_control(&mut self, msg: ControlMsg) {
        match msg {
            ControlMsg::SetInputVolume { channel, volume_db } => {
                if let Some(state) = self.inputs.get_mut(channel) {
                    state.set_volume_db(volume_db);
                }
            }
            ControlMsg::SetOutputVolume { channel, volume_db } => {
                if let Some(state) = self.outputs.get_mut(channel) {
                    state.set_volume_db(volume_db);
                }
            }
            ControlMsg::SetInputMute { channel, muted } => {
                if let Some(state) = self.inputs.get_mut(channel) {
                    state.muted = muted;
                }
            }
            ControlMsg::SetOutputMute { channel, muted } => {
                if let Some(state) = self.outputs.get_mut(channel) {
                    state.muted = muted;
                }
            }
            ControlMsg::SetInputSolo { channel, soloed } => {
                if let Some(state) = self.inputs.get_mut(channel) {
                    state.soloed = soloed;
                }
            }
        }
    }

    /// Check if any input channel is soloed
    pub fn any_input_soloed(&self) -> bool {
        self.inputs.iter().any(|ch| ch.soloed)
    }

    /// Get effective gain for an input channel (considering solo state)
    pub fn get_input_effective_gain(&self, index: usize) -> f32 {
        let channel = &self.inputs[index];
        if channel.muted {
            return 0.0;
        }

        let any_soloed = self.any_input_soloed();
        if any_soloed && !channel.soloed {
            return 0.0;
        }

        MeterData::db_to_linear(channel.volume_db)
    }
}
