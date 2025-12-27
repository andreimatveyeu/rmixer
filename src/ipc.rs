//! Inter-process communication types for rmixer
//!
//! Defines lock-free communication structures between the audio thread
//! and the UI thread for real-time safe operation.

use std::time::Instant;

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

    /// Timestamp when this measurement was taken
    pub timestamp: Instant,
}

impl MeterData {
    /// Create new meter data for a mono channel
    pub fn mono(channel_index: usize, peak: f32) -> Self {
        Self {
            channel_index,
            peaks: [peak, 0.0],
            port_count: 1,
            timestamp: Instant::now(),
        }
    }

    /// Create new meter data for a stereo channel
    pub fn stereo(channel_index: usize, peak_l: f32, peak_r: f32) -> Self {
        Self {
            channel_index,
            peaks: [peak_l, peak_r],
            port_count: 2,
            timestamp: Instant::now(),
        }
    }

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

/// Control message sent from UI thread to audio thread
#[derive(Debug, Clone, Copy)]
pub enum ControlMsg {
    /// Set volume for an input channel (index, volume in dB)
    SetInputVolume { channel: usize, volume_db: f32 },

    /// Set volume for an output channel (index, volume in dB)
    SetOutputVolume { channel: usize, volume_db: f32 },

    /// Toggle mute for an input channel
    ToggleInputMute { channel: usize },

    /// Toggle mute for an output channel
    ToggleOutputMute { channel: usize },

    /// Toggle solo for an input channel
    ToggleInputSolo { channel: usize },

    /// Request to quit the audio engine
    Quit,
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
            muted: false,
            soloed: false,
            current_peaks: [0.0; 2],
            peak_hold: [0.0; 2],
            peak_hold_time: [now; 2],
        }
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

    /// Adjust volume by delta, clamping to valid range
    pub fn adjust_volume(&mut self, delta_db: f32) {
        self.volume_db = (self.volume_db + delta_db).clamp(VOLUME_MIN_DB, VOLUME_MAX_DB);
    }

    /// Get volume as linear gain
    pub fn get_linear_gain(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            MeterData::db_to_linear(self.volume_db)
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
