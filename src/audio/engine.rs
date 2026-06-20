//! JACK audio engine implementation
//!
//! This module provides the core audio processing functionality using the JACK API.
//! JACK provides synchronized callbacks for all ports, eliminating timing issues.
//! Works with PipeWire's JACK compatibility layer.

use anyhow::{Context, Result};
use jack::{AudioIn, AudioOut, Client, ClientOptions, Control, Frames, Port, ProcessScope};
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::Config;
use crate::ipc::{ChannelState, ControlMsg, MeterData, MixerState};

/// Size of the ring buffer for meter data
const METER_RING_BUFFER_SIZE: usize = 1024;

/// Size of the ring buffer for control messages
const CONTROL_RING_BUFFER_SIZE: usize = 64;

/// How often meter data is flushed to the UI, in Hz. The process callback
/// runs far more often than this (hundreds of times per second at small
/// buffer sizes), so peaks are accumulated between flushes and pushed at
/// roughly this rate, comfortably above the UI's redraw rate.
const METER_UPDATE_HZ: usize = 60;

/// Audio engine that manages JACK connections and processing
pub struct AudioEngine {
    /// JACK async client handle
    _async_client: jack::AsyncClient<Notifications, ProcessHandler>,

    /// Producer for sending control messages to audio thread
    control_producer: Producer<ControlMsg>,

    /// Consumer for receiving meter data from audio thread
    meter_consumer: Consumer<MeterData>,

    /// Flag to signal the audio thread to quit
    quit_flag: Arc<AtomicBool>,

    /// Set when the JACK server shuts the client down
    dead_flag: Arc<AtomicBool>,
}

impl AudioEngine {
    /// Create and start the audio engine
    pub fn new(config: Config) -> Result<Self> {
        // Create ring buffers for communication
        let (meter_producer, meter_consumer) = RingBuffer::new(METER_RING_BUFFER_SIZE);
        let (control_producer, control_consumer) = RingBuffer::new(CONTROL_RING_BUFFER_SIZE);

        let quit_flag = Arc::new(AtomicBool::new(false));
        let dead_flag = Arc::new(AtomicBool::new(false));

        // Create JACK client
        let (client, _status) = Client::new(&config.client_name, ClientOptions::NO_START_SERVER)
            .context("Failed to create JACK client. Is JACK/PipeWire running?")?;

        log::info!(
            "Created JACK client '{}' with sample rate {} Hz, buffer size {}",
            client.name(),
            client.sample_rate(),
            client.buffer_size()
        );

        // Create input ports
        let mut input_ports: Vec<Port<AudioIn>> = Vec::new();
        for input_cfg in &config.inputs {
            for port_name in &input_cfg.ports {
                let port = client
                    .register_port(port_name, AudioIn::default())
                    .with_context(|| format!("Failed to register input port '{}'", port_name))?;
                input_ports.push(port);
            }
        }

        // Create output ports
        let mut output_ports: Vec<Port<AudioOut>> = Vec::new();
        for output_cfg in &config.outputs {
            for port_name in &output_cfg.ports {
                let port = client
                    .register_port(port_name, AudioOut::default())
                    .with_context(|| format!("Failed to register output port '{}'", port_name))?;
                output_ports.push(port);
            }
        }

        log::info!(
            "Registered {} input ports and {} output ports",
            input_ports.len(),
            output_ports.len()
        );

        // Build mixer state
        let inputs: Vec<ChannelState> = config
            .inputs
            .iter()
            .map(|c| ChannelState::new(c.name.clone(), c.port_count()))
            .collect();

        let outputs: Vec<ChannelState> = config
            .outputs
            .iter()
            .map(|c| ChannelState::new(c.name.clone(), c.port_count()))
            .collect();

        let mixer_state = MixerState { inputs, outputs };

        // Build port mapping info
        let input_port_counts: Vec<usize> = config.inputs.iter().map(|c| c.port_count()).collect();
        let output_port_counts: Vec<usize> = config.outputs.iter().map(|c| c.port_count()).collect();

        // Precompute the static routing once: for each global input port,
        // the list of global output port indices it feeds. This mapping never
        // changes at runtime, so deriving it here keeps the per-cycle mixing
        // loop free of the branchy mono/stereo fan-out logic.
        let input_routing = build_input_routing(&input_port_counts, &output_port_counts);

        // Pre-allocate one local mix buffer per output port so the process
        // callback never reads a JACK input buffer and writes a JACK output
        // buffer at the same time (JACK may alias them for zero-copy).
        let buffer_size = client.buffer_size() as usize;
        let mix_buffers: Vec<Vec<f32>> = (0..output_ports.len())
            .map(|_| vec![0.0; buffer_size])
            .collect();

        // Meter accumulation state. Channels are indexed inputs-then-outputs,
        // matching `MeterData::channel_index`. Peaks are folded in every cycle
        // and flushed to the UI every `meter_interval_frames` frames.
        let meter_port_counts: Vec<usize> = input_port_counts
            .iter()
            .chain(output_port_counts.iter())
            .copied()
            .collect();
        let meter_accum = vec![[0.0f32; 2]; meter_port_counts.len()];
        let meter_interval_frames = (client.sample_rate() as usize / METER_UPDATE_HZ).max(1);

        // Create process handler
        let process_handler = ProcessHandler {
            input_ports,
            output_ports,
            input_port_counts,
            output_port_counts,
            input_routing,
            mixer_state,
            mix_buffers,
            meter_producer,
            meter_accum,
            meter_port_counts,
            meter_interval_frames,
            frames_since_meter: 0,
            control_consumer,
            quit_flag: quit_flag.clone(),
        };

        // Create notification handler
        let notifications = Notifications {
            dead_flag: dead_flag.clone(),
        };

        // Activate client
        let async_client = client
            .activate_async(notifications, process_handler)
            .context("Failed to activate JACK client")?;

        log::info!("JACK client activated");

        Ok(Self {
            _async_client: async_client,
            control_producer,
            meter_consumer,
            quit_flag,
            dead_flag,
        })
    }

    /// Send a control message to the audio thread.
    ///
    /// Failures are logged rather than fatal: the queue can only fill up if
    /// the audio thread stopped draining it (e.g. the JACK client died), and
    /// dropping a UI control message is harmless in that situation.
    pub fn send_control(&mut self, msg: ControlMsg) {
        if self.control_producer.push(msg).is_err() {
            log::warn!("Control message queue full; dropping message (audio engine stalled?)");
        }
    }

    /// Try to receive meter data from the audio thread
    pub fn try_recv_meter(&mut self) -> Option<MeterData> {
        self.meter_consumer.pop().ok()
    }

    /// Returns true if the JACK server has shut this client down
    pub fn is_dead(&self) -> bool {
        self.dead_flag.load(Ordering::Relaxed)
    }

    /// Request the audio engine to quit
    pub fn quit(&mut self) {
        self.quit_flag.store(true, Ordering::SeqCst);
        self.send_control(ControlMsg::Quit);
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.quit();
    }
}

/// JACK notification handler
struct Notifications {
    /// Set when the server shuts the client down, so the UI can report it
    dead_flag: Arc<AtomicBool>,
}

impl jack::NotificationHandler for Notifications {
    unsafe fn shutdown(&mut self, _status: jack::ClientStatus, reason: &str) {
        self.dead_flag.store(true, Ordering::SeqCst);
        log::error!("JACK client shutdown: {}", reason);
    }

    fn sample_rate(&mut self, _: &Client, srate: jack::Frames) -> Control {
        log::info!("Sample rate changed to {}", srate);
        Control::Continue
    }

    fn xrun(&mut self, _: &Client) -> Control {
        // Silently ignore xruns to avoid garbling the TUI
        Control::Continue
    }
}

/// Get an input port's buffer as a slice, or `None` if JACK returned no
/// buffer (which can happen transiently while ports are being connected or
/// disconnected, especially under PipeWire's JACK layer).
fn input_buffer<'a>(port: &'a Port<AudioIn>, ps: &'a ProcessScope) -> Option<&'a [f32]> {
    let n_frames = ps.n_frames() as usize;
    // SAFETY: inside the process callback JACK guarantees a non-null buffer
    // is valid for `n_frames` samples; the lifetime is tied to both the port
    // borrow and the process scope. This mirrors `Port::as_slice` but adds
    // the null check that the jack crate omits.
    unsafe {
        let ptr = port.buffer(ps.n_frames()) as *const f32;
        if ptr.is_null() {
            None
        } else {
            Some(std::slice::from_raw_parts(ptr, n_frames))
        }
    }
}

/// Get an output port's buffer as a mutable slice, or `None` if JACK
/// returned no buffer (see [`input_buffer`]).
fn output_buffer<'a>(port: &'a mut Port<AudioOut>, ps: &'a ProcessScope) -> Option<&'a mut [f32]> {
    let n_frames = ps.n_frames() as usize;
    // SAFETY: as above, plus exclusivity: we hold the only `&mut` borrow of
    // this port, each output port has a distinct buffer, and no input buffer
    // slice is alive while output buffers are written (see `process`).
    unsafe {
        let ptr = port.buffer(ps.n_frames()) as *mut f32;
        if ptr.is_null() {
            None
        } else {
            Some(std::slice::from_raw_parts_mut(ptr, n_frames))
        }
    }
}

/// Build the static input→output port routing table.
///
/// Returns, for each global input port index, the global output port indices
/// it feeds. The fan-out rules mirror the original per-cycle logic:
/// a mono input port feeds every output port; a stereo input port feeds the
/// matching-position output port of each output channel.
fn build_input_routing(
    input_port_counts: &[usize],
    output_port_counts: &[usize],
) -> Vec<Vec<usize>> {
    let mut routing = Vec::new();
    for &port_count in input_port_counts {
        for p in 0..port_count {
            let mut targets = Vec::new();
            let mut out_port_idx = 0;
            for &out_port_count in output_port_counts {
                for out_p in 0..out_port_count {
                    let use_this_input = if port_count == 1 {
                        true
                    } else {
                        p == out_p || (p == 0 && out_p >= port_count)
                    };
                    if use_this_input {
                        targets.push(out_port_idx);
                    }
                    out_port_idx += 1;
                }
            }
            routing.push(targets);
        }
    }
    routing
}

/// JACK process handler - runs in the real-time audio thread
struct ProcessHandler {
    /// Input ports
    input_ports: Vec<Port<AudioIn>>,

    /// Output ports
    output_ports: Vec<Port<AudioOut>>,

    /// Number of ports per input channel
    input_port_counts: Vec<usize>,

    /// Number of ports per output channel
    output_port_counts: Vec<usize>,

    /// For each global input port index, the global output port indices it
    /// contributes to. Precomputed once since the routing is static.
    input_routing: Vec<Vec<usize>>,

    /// Mixer state with gains, mute, solo
    mixer_state: MixerState,

    /// Local mix buffer per output port. Inputs are accumulated here first,
    /// then copied to the JACK output buffers, so JACK input and output
    /// buffers are never borrowed at the same time.
    mix_buffers: Vec<Vec<f32>>,

    /// Producer for sending meter data to UI
    meter_producer: Producer<MeterData>,

    /// Per-channel peak accumulator (inputs then outputs), holding the max
    /// peak seen since the last flush so transients between flushes survive.
    meter_accum: Vec<[f32; 2]>,

    /// Port count per channel (inputs then outputs), for building MeterData.
    meter_port_counts: Vec<usize>,

    /// Flush accumulated meters to the UI after this many frames.
    meter_interval_frames: usize,

    /// Frames accumulated since the last meter flush.
    frames_since_meter: usize,

    /// Consumer for receiving control messages from UI
    control_consumer: Consumer<ControlMsg>,

    /// Quit flag reference
    quit_flag: Arc<AtomicBool>,
}

impl ProcessHandler {
    /// Process control messages from UI
    fn process_control_messages(&mut self) {
        while let Ok(msg) = self.control_consumer.pop() {
            match msg {
                ControlMsg::SetInputVolume { channel, volume_db } => {
                    if channel < self.mixer_state.inputs.len() {
                        self.mixer_state.inputs[channel].set_volume_db(volume_db);
                    }
                }
                ControlMsg::SetOutputVolume { channel, volume_db } => {
                    if channel < self.mixer_state.outputs.len() {
                        self.mixer_state.outputs[channel].set_volume_db(volume_db);
                    }
                }
                ControlMsg::ToggleInputMute { channel } => {
                    if channel < self.mixer_state.inputs.len() {
                        self.mixer_state.inputs[channel].muted =
                            !self.mixer_state.inputs[channel].muted;
                    }
                }
                ControlMsg::ToggleOutputMute { channel } => {
                    if channel < self.mixer_state.outputs.len() {
                        self.mixer_state.outputs[channel].muted =
                            !self.mixer_state.outputs[channel].muted;
                    }
                }
                ControlMsg::ToggleInputSolo { channel } => {
                    if channel < self.mixer_state.inputs.len() {
                        self.mixer_state.inputs[channel].soloed =
                            !self.mixer_state.inputs[channel].soloed;
                    }
                }
                ControlMsg::Quit => {
                    self.quit_flag.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    /// Compute peak level of samples (linear scale)
    fn compute_peak(samples: &[f32]) -> f32 {
        samples
            .iter()
            .map(|s| s.abs())
            .fold(0.0_f32, |a, b| a.max(b))
    }

    /// Push accumulated per-channel peaks to the UI and reset the accumulator.
    fn flush_meters(&mut self) {
        for (channel_index, (peaks, &port_count)) in self
            .meter_accum
            .iter_mut()
            .zip(self.meter_port_counts.iter())
            .enumerate()
        {
            let _ = self.meter_producer.push(MeterData {
                channel_index,
                peaks: *peaks,
                port_count,
            });
            *peaks = [0.0; 2];
        }
        self.frames_since_meter = 0;
    }
}

impl jack::ProcessHandler for ProcessHandler {
    fn process(&mut self, _: &Client, ps: &ProcessScope) -> Control {
        // Process any pending control messages
        self.process_control_messages();

        if self.quit_flag.load(Ordering::Relaxed) {
            return Control::Quit;
        }

        let n_frames = ps.n_frames() as usize;

        // Zero the local mix buffers. They are sized by `buffer_size()`, so
        // the resize is a safety net that should never allocate in practice.
        for buf in &mut self.mix_buffers {
            if buf.len() < n_frames {
                buf.resize(n_frames, 0.0);
            }
            buf[..n_frames].fill(0.0);
        }

        let any_soloed = self.mixer_state.any_input_soloed();

        // Phase 1: read inputs, meter them, and accumulate into the local
        // mix buffers. Each JACK input buffer is fetched exactly once per
        // cycle, and no JACK output buffer is touched in this phase.
        let mut in_port_idx = 0;
        for (ch_idx, &port_count) in self.input_port_counts.iter().enumerate() {
            let input_state = &self.mixer_state.inputs[ch_idx];

            // Calculate effective input gain
            let input_gain = if input_state.muted {
                0.0
            } else if any_soloed && !input_state.soloed {
                0.0
            } else {
                input_state.volume_linear()
            };

            let mut peaks = [0.0f32; 2];

            // Process each port of this input channel
            for p in 0..port_count {
                let Some(in_samples) = input_buffer(&self.input_ports[in_port_idx], ps) else {
                    // Buffer unavailable (port renegotiation in progress)
                    in_port_idx += 1;
                    continue;
                };
                peaks[p] = Self::compute_peak(in_samples);

                if input_gain != 0.0 {
                    // Accumulate this input into the mix buffer of every
                    // output port it maps to, using the precomputed routing.
                    for &out_port_idx in &self.input_routing[in_port_idx] {
                        let mix = &mut self.mix_buffers[out_port_idx];
                        for (m, in_s) in mix[..n_frames].iter_mut().zip(in_samples.iter()) {
                            *m += in_s * input_gain;
                        }
                    }
                }

                in_port_idx += 1;
            }

            // Accumulate this input channel's peaks until the next flush.
            for p in 0..port_count {
                self.meter_accum[ch_idx][p] = self.meter_accum[ch_idx][p].max(peaks[p]);
            }
        }

        // Phase 2: apply output gains, copy the mix buffers to the JACK
        // output buffers, and meter the result. Each JACK output buffer is
        // fetched exactly once per cycle, and no input slice is alive here.
        let num_inputs = self.mixer_state.inputs.len();
        let mut out_port_idx = 0;
        for (ch_idx, &port_count) in self.output_port_counts.iter().enumerate() {
            let output_gain = self.mixer_state.outputs[ch_idx].get_linear_gain();
            let mut peaks = [0.0f32; 2];

            for p in 0..port_count {
                let mix = &self.mix_buffers[out_port_idx];
                if let Some(out_samples) = output_buffer(&mut self.output_ports[out_port_idx], ps)
                {
                    let mut peak = 0.0f32;
                    for (out_s, m) in out_samples.iter_mut().zip(mix[..n_frames].iter()) {
                        let v = m * output_gain;
                        *out_s = v;
                        peak = peak.max(v.abs());
                    }
                    peaks[p] = peak;
                }
                out_port_idx += 1;
            }

            // Accumulate this output channel's peaks until the next flush.
            for p in 0..port_count {
                self.meter_accum[num_inputs + ch_idx][p] =
                    self.meter_accum[num_inputs + ch_idx][p].max(peaks[p]);
            }
        }

        // Flush accumulated meters to the UI at roughly METER_UPDATE_HZ,
        // instead of pushing every (much shorter) process cycle.
        self.frames_since_meter += n_frames;
        if self.frames_since_meter >= self.meter_interval_frames {
            self.flush_meters();
        }

        Control::Continue
    }

    fn buffer_size(&mut self, _: &Client, size: Frames) -> Control {
        // Non-realtime callback: resize the local mix buffers so `process`
        // never has to allocate
        for buf in &mut self.mix_buffers {
            buf.resize(size as usize, 0.0);
        }
        Control::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::build_input_routing;

    #[test]
    fn mono_input_feeds_every_output_port() {
        // One mono input, one stereo output: the single input port feeds both.
        assert_eq!(build_input_routing(&[1], &[2]), vec![vec![0, 1]]);
    }

    #[test]
    fn stereo_input_feeds_matching_output_ports() {
        // Stereo input into stereo output: left->left, right->right.
        assert_eq!(build_input_routing(&[2], &[2]), vec![vec![0], vec![1]]);
    }

    #[test]
    fn stereo_input_into_mono_output_uses_left_only() {
        // Preserves existing behavior: only the left port reaches a mono output.
        assert_eq!(build_input_routing(&[2], &[1]), vec![vec![0], vec![]]);
    }

    #[test]
    fn mixed_channels_index_output_ports_globally() {
        // Mono + stereo inputs feeding two stereo outputs (ports 0,1 and 2,3).
        let routing = build_input_routing(&[1, 2], &[2, 2]);
        assert_eq!(
            routing,
            vec![
                vec![0, 1, 2, 3], // mono input -> all output ports
                vec![0, 2],       // stereo-left -> left of each output
                vec![1, 3],       // stereo-right -> right of each output
            ]
        );
    }
}
