//! JACK audio engine implementation
//!
//! This module provides the core audio processing functionality using the JACK API.
//! JACK provides synchronized callbacks for all ports, eliminating timing issues.
//! Works with PipeWire's JACK compatibility layer.

use anyhow::{Context, Result};
use jack::{AudioIn, AudioOut, Client, ClientOptions, Control, Port, ProcessScope};
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::Config;
use crate::ipc::{ChannelState, ControlMsg, MeterData, MixerState};

/// Size of the ring buffer for meter data
const METER_RING_BUFFER_SIZE: usize = 1024;

/// Size of the ring buffer for control messages
const CONTROL_RING_BUFFER_SIZE: usize = 64;

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
}

impl AudioEngine {
    /// Create and start the audio engine
    pub fn new(config: Config) -> Result<Self> {
        // Create ring buffers for communication
        let (meter_producer, meter_consumer) = RingBuffer::new(METER_RING_BUFFER_SIZE);
        let (control_producer, control_consumer) = RingBuffer::new(CONTROL_RING_BUFFER_SIZE);

        let quit_flag = Arc::new(AtomicBool::new(false));

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

        // Create process handler
        let process_handler = ProcessHandler {
            input_ports,
            output_ports,
            input_port_counts,
            output_port_counts,
            mixer_state,
            meter_producer,
            control_consumer,
            quit_flag: quit_flag.clone(),
        };

        // Create notification handler
        let notifications = Notifications;

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
        })
    }

    /// Send a control message to the audio thread
    pub fn send_control(&mut self, msg: ControlMsg) -> Result<()> {
        self.control_producer
            .push(msg)
            .map_err(|_| anyhow::anyhow!("Control message queue full"))
    }

    /// Try to receive meter data from the audio thread
    pub fn try_recv_meter(&mut self) -> Option<MeterData> {
        self.meter_consumer.pop().ok()
    }

    /// Request the audio engine to quit
    pub fn quit(&mut self) {
        self.quit_flag.store(true, Ordering::SeqCst);
        let _ = self.send_control(ControlMsg::Quit);
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.quit();
    }
}

/// JACK notification handler
struct Notifications;

impl jack::NotificationHandler for Notifications {
    unsafe fn shutdown(&mut self, _status: jack::ClientStatus, reason: &str) {
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

    /// Mixer state with gains, mute, solo
    mixer_state: MixerState,

    /// Producer for sending meter data to UI
    meter_producer: Producer<MeterData>,

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
                        self.mixer_state.inputs[channel].volume_db = volume_db;
                    }
                }
                ControlMsg::SetOutputVolume { channel, volume_db } => {
                    if channel < self.mixer_state.outputs.len() {
                        self.mixer_state.outputs[channel].volume_db = volume_db;
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
}

impl jack::ProcessHandler for ProcessHandler {
    fn process(&mut self, _: &Client, ps: &ProcessScope) -> Control {
        // Process any pending control messages
        self.process_control_messages();

        if self.quit_flag.load(Ordering::Relaxed) {
            return Control::Quit;
        }

        let any_soloed = self.mixer_state.any_input_soloed();

        // First, zero all output buffers
        for port in &mut self.output_ports {
            let out = port.as_mut_slice(ps);
            for s in out.iter_mut() {
                *s = 0.0;
            }
        }

        // Process inputs and mix to outputs
        let mut in_port_idx = 0;
        for (ch_idx, &port_count) in self.input_port_counts.iter().enumerate() {
            let input_state = &self.mixer_state.inputs[ch_idx];
            
            // Calculate effective input gain
            let input_gain = if input_state.muted {
                0.0
            } else if any_soloed && !input_state.soloed {
                0.0
            } else {
                MeterData::db_to_linear(input_state.volume_db)
            };

            let mut peaks = [0.0f32; 2];

            // Process each port of this input channel
            for p in 0..port_count {
                let in_samples = self.input_ports[in_port_idx].as_slice(ps);
                peaks[p] = Self::compute_peak(in_samples);

                // Mix this input to all outputs
                let mut out_port_idx = 0;
                for (out_ch_idx, &out_port_count) in self.output_port_counts.iter().enumerate() {
                    let output_state = &self.mixer_state.outputs[out_ch_idx];
                    let output_gain = output_state.get_linear_gain();

                    for out_p in 0..out_port_count {
                        // Determine which input port maps to this output port
                        // For mono input -> stereo output: use same input for both
                        // For stereo input -> stereo output: use matching channels
                        let use_this_input = if port_count == 1 {
                            // Mono input goes to all output ports
                            true
                        } else {
                            // Stereo input: left->left, right->right
                            p == out_p || (p == 0 && out_p >= port_count)
                        };

                        if use_this_input {
                            let out_samples = self.output_ports[out_port_idx].as_mut_slice(ps);
                            let combined_gain = input_gain * output_gain;
                            
                            for (out_s, in_s) in out_samples.iter_mut().zip(in_samples.iter()) {
                                *out_s += in_s * combined_gain;
                            }
                        }
                        out_port_idx += 1;
                    }
                }

                in_port_idx += 1;
            }

            // Send meter data for this input channel
            let meter = MeterData {
                channel_index: ch_idx,
                peaks,
                port_count,
                timestamp: std::time::Instant::now(),
            };
            let _ = self.meter_producer.push(meter);
        }

        // Calculate and send output meters
        let num_inputs = self.mixer_state.inputs.len();
        let mut out_port_idx = 0;
        for (ch_idx, &port_count) in self.output_port_counts.iter().enumerate() {
            let mut peaks = [0.0f32; 2];
            
            for p in 0..port_count {
                let out_samples = self.output_ports[out_port_idx].as_mut_slice(ps);
                peaks[p] = Self::compute_peak(out_samples);
                out_port_idx += 1;
            }

            let meter = MeterData {
                channel_index: num_inputs + ch_idx,
                peaks,
                port_count,
                timestamp: std::time::Instant::now(),
            };
            let _ = self.meter_producer.push(meter);
        }

        Control::Continue
    }
}
