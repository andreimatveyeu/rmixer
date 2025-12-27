//! Main application state and UI rendering
//!
//! Manages the TUI application lifecycle and rendering.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};

use crate::audio::AudioEngine;
use crate::config::Config;
use crate::ipc::{ChannelState, ControlMsg, MixerState, VOLUME_STEP_DB};

use super::widgets::ChannelStrip;


/// Peak hold duration in seconds
const PEAK_HOLD_DURATION: f32 = 5.0;

/// Target frame rate
const TARGET_FPS: u64 = 60;

/// Selection type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionType {
    Input,
    Output,
}

/// Main application state
pub struct App {
    /// Audio engine handle
    audio_engine: AudioEngine,

    /// Mixer state (mirrors audio thread state for UI)
    mixer_state: MixerState,

    /// Currently selected channel index
    selected_channel: usize,

    /// Selection type (input or output)
    selection_type: SelectionType,

    /// Whether the app should quit
    should_quit: bool,

    /// Last frame time
    last_frame: Instant,

    /// Client name for display
    client_name: String,
    
    /// Configuration (for saving volumes on exit)
    config: Config,
}

impl App {
    /// Create a new application
    pub fn new(config: Config) -> Result<Self> {
        let client_name = config.client_name.clone();

        // Initialize channel states with saved volumes
        let inputs: Vec<ChannelState> = config
            .inputs
            .iter()
            .map(|c| {
                let mut state = ChannelState::new(c.name.clone(), c.port_count());
                if let Some(vol) = c.volume_db {
                    state.volume_db = vol.clamp(-60.0, 12.0);
                }
                state
            })
            .collect();

        let outputs: Vec<ChannelState> = config
            .outputs
            .iter()
            .map(|c| {
                let mut state = ChannelState::new(c.name.clone(), c.port_count());
                if let Some(vol) = c.volume_db {
                    state.volume_db = vol.clamp(-60.0, 12.0);
                }
                state
            })
            .collect();

        let mixer_state = MixerState { inputs, outputs };

        // Create audio engine
        let mut audio_engine = AudioEngine::new(config.clone())?;
        
        // Send initial volume levels to audio thread
        for (i, c) in config.inputs.iter().enumerate() {
            if let Some(vol) = c.volume_db {
                let _ = audio_engine.send_control(ControlMsg::SetInputVolume {
                    channel: i,
                    volume_db: vol.clamp(-60.0, 12.0),
                });
            }
        }
        for (i, c) in config.outputs.iter().enumerate() {
            if let Some(vol) = c.volume_db {
                let _ = audio_engine.send_control(ControlMsg::SetOutputVolume {
                    channel: i,
                    volume_db: vol.clamp(-60.0, 12.0),
                });
            }
        }

        Ok(Self {
            audio_engine,
            mixer_state,
            selected_channel: 0,
            selection_type: SelectionType::Input,
            should_quit: false,
            last_frame: Instant::now(),
            client_name,
            config,
        })
    }

    /// Run the main application loop
    pub fn run(mut self) -> Result<()> {
        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let result = self.main_loop(&mut terminal);

        // Restore terminal
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        // Save volumes to config
        self.save_volumes();

        // Stop audio engine
        self.audio_engine.quit();

        result
    }
    
    /// Save current volume levels to config file
    fn save_volumes(&mut self) {
        let input_volumes: Vec<f32> = self.mixer_state.inputs.iter().map(|c| c.volume_db).collect();
        let output_volumes: Vec<f32> = self.mixer_state.outputs.iter().map(|c| c.volume_db).collect();
        
        self.config.update_volumes(&input_volumes, &output_volumes);
        
        if let Err(e) = self.config.save() {
            eprintln!("Warning: Failed to save config: {}", e);
        }
    }

    /// Main event loop
    fn main_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let frame_duration = Duration::from_millis(1000 / TARGET_FPS);

        while !self.should_quit {
            // Process meter updates from audio thread
            self.process_meter_updates();

            // Draw UI
            terminal.draw(|f| self.render(f))?;

            // Handle input with timeout
            let timeout = frame_duration.saturating_sub(self.last_frame.elapsed());
            if event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.handle_key(key.code)?;
                    }
                }
            }

            self.last_frame = Instant::now();
        }

        Ok(())
    }

    /// Process meter updates from the audio thread
    fn process_meter_updates(&mut self) {
        while let Some(meter) = self.audio_engine.try_recv_meter() {
            let num_inputs = self.mixer_state.inputs.len();

            if meter.channel_index < num_inputs {
                // Input channel
                self.mixer_state.inputs[meter.channel_index]
                    .update_meter(meter.peaks, PEAK_HOLD_DURATION);
            } else {
                // Output channel
                let output_idx = meter.channel_index - num_inputs;
                if output_idx < self.mixer_state.outputs.len() {
                    self.mixer_state.outputs[output_idx]
                        .update_meter(meter.peaks, PEAK_HOLD_DURATION);
                }
            }
        }
    }

    /// Handle keyboard input
    fn handle_key(&mut self, code: KeyCode) -> Result<()> {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_quit = true;
            }
            KeyCode::Left => {
                self.select_previous();
            }
            KeyCode::Right => {
                self.select_next();
            }
            KeyCode::Up => {
                self.adjust_volume(VOLUME_STEP_DB)?;
            }
            KeyCode::Down => {
                self.adjust_volume(-VOLUME_STEP_DB)?;
            }
            KeyCode::Char('m') => {
                self.toggle_mute()?;
            }
            KeyCode::Char('s') => {
                self.toggle_solo()?;
            }
            KeyCode::Char('0') => {
                self.reset_volume_to_zero()?;
            }
            KeyCode::Tab => {
                self.toggle_section();
            }
            _ => {}
        }
        Ok(())
    }

    /// Select the previous channel
    fn select_previous(&mut self) {
        let max_idx = match self.selection_type {
            SelectionType::Input => self.mixer_state.inputs.len(),
            SelectionType::Output => self.mixer_state.outputs.len(),
        };

        if self.selected_channel > 0 {
            self.selected_channel -= 1;
        } else if max_idx > 0 {
            // Wrap around or switch section
            match self.selection_type {
                SelectionType::Input => {
                    if !self.mixer_state.outputs.is_empty() {
                        self.selection_type = SelectionType::Output;
                        self.selected_channel = self.mixer_state.outputs.len() - 1;
                    } else {
                        self.selected_channel = max_idx - 1;
                    }
                }
                SelectionType::Output => {
                    if !self.mixer_state.inputs.is_empty() {
                        self.selection_type = SelectionType::Input;
                        self.selected_channel = self.mixer_state.inputs.len() - 1;
                    } else {
                        self.selected_channel = max_idx - 1;
                    }
                }
            }
        }
    }

    /// Select the next channel
    fn select_next(&mut self) {
        let max_idx = match self.selection_type {
            SelectionType::Input => self.mixer_state.inputs.len(),
            SelectionType::Output => self.mixer_state.outputs.len(),
        };

        if self.selected_channel + 1 < max_idx {
            self.selected_channel += 1;
        } else {
            // Wrap around or switch section
            match self.selection_type {
                SelectionType::Input => {
                    if !self.mixer_state.outputs.is_empty() {
                        self.selection_type = SelectionType::Output;
                        self.selected_channel = 0;
                    } else {
                        self.selected_channel = 0;
                    }
                }
                SelectionType::Output => {
                    if !self.mixer_state.inputs.is_empty() {
                        self.selection_type = SelectionType::Input;
                        self.selected_channel = 0;
                    } else {
                        self.selected_channel = 0;
                    }
                }
            }
        }
    }

    /// Toggle between input and output sections
    fn toggle_section(&mut self) {
        match self.selection_type {
            SelectionType::Input => {
                if !self.mixer_state.outputs.is_empty() {
                    self.selection_type = SelectionType::Output;
                    self.selected_channel = 0;
                }
            }
            SelectionType::Output => {
                if !self.mixer_state.inputs.is_empty() {
                    self.selection_type = SelectionType::Input;
                    self.selected_channel = 0;
                }
            }
        }
    }

    /// Adjust volume of the selected channel
    fn adjust_volume(&mut self, delta: f32) -> Result<()> {
        match self.selection_type {
            SelectionType::Input => {
                if self.selected_channel < self.mixer_state.inputs.len() {
                    let channel = &mut self.mixer_state.inputs[self.selected_channel];
                    channel.adjust_volume(delta);
                    self.audio_engine.send_control(ControlMsg::SetInputVolume {
                        channel: self.selected_channel,
                        volume_db: channel.volume_db,
                    })?;
                }
            }
            SelectionType::Output => {
                if self.selected_channel < self.mixer_state.outputs.len() {
                    let channel = &mut self.mixer_state.outputs[self.selected_channel];
                    channel.adjust_volume(delta);
                    self.audio_engine.send_control(ControlMsg::SetOutputVolume {
                        channel: self.selected_channel,
                        volume_db: channel.volume_db,
                    })?;
                }
            }
        }
        Ok(())
    }

    /// Toggle mute on the selected channel
    fn toggle_mute(&mut self) -> Result<()> {
        match self.selection_type {
            SelectionType::Input => {
                if self.selected_channel < self.mixer_state.inputs.len() {
                    self.mixer_state.inputs[self.selected_channel].muted =
                        !self.mixer_state.inputs[self.selected_channel].muted;
                    self.audio_engine.send_control(ControlMsg::ToggleInputMute {
                        channel: self.selected_channel,
                    })?;
                }
            }
            SelectionType::Output => {
                if self.selected_channel < self.mixer_state.outputs.len() {
                    self.mixer_state.outputs[self.selected_channel].muted =
                        !self.mixer_state.outputs[self.selected_channel].muted;
                    self.audio_engine
                        .send_control(ControlMsg::ToggleOutputMute {
                            channel: self.selected_channel,
                        })?;
                }
            }
        }
        Ok(())
    }

    /// Toggle solo on the selected channel (input only)
    fn toggle_solo(&mut self) -> Result<()> {
        if self.selection_type == SelectionType::Input {
            if self.selected_channel < self.mixer_state.inputs.len() {
                self.mixer_state.inputs[self.selected_channel].soloed =
                    !self.mixer_state.inputs[self.selected_channel].soloed;
                self.audio_engine.send_control(ControlMsg::ToggleInputSolo {
                    channel: self.selected_channel,
                })?;
            }
        }
        Ok(())
    }

    /// Reset volume of the selected channel to 0 dB
    fn reset_volume_to_zero(&mut self) -> Result<()> {
        match self.selection_type {
            SelectionType::Input => {
                if self.selected_channel < self.mixer_state.inputs.len() {
                    self.mixer_state.inputs[self.selected_channel].volume_db = 0.0;
                    self.audio_engine.send_control(ControlMsg::SetInputVolume {
                        channel: self.selected_channel,
                        volume_db: 0.0,
                    })?;
                }
            }
            SelectionType::Output => {
                if self.selected_channel < self.mixer_state.outputs.len() {
                    self.mixer_state.outputs[self.selected_channel].volume_db = 0.0;
                    self.audio_engine.send_control(ControlMsg::SetOutputVolume {
                        channel: self.selected_channel,
                        volume_db: 0.0,
                    })?;
                }
            }
        }
        Ok(())
    }

    /// Render the UI
    fn render(&self, frame: &mut Frame) {
        let area = frame.area();

        // Main layout: title bar, channels, help bar
        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Title
                Constraint::Min(10),   // Channels
                Constraint::Length(2), // Help
            ])
            .split(area);

        // Title bar
        self.render_title(frame, main_chunks[0]);

        // Channels area
        self.render_channels(frame, main_chunks[1]);

        // Help bar
        self.render_help(frame, main_chunks[2]);
    }

    /// Render the title bar
    fn render_title(&self, frame: &mut Frame, area: Rect) {
        let title = format!(" RMixer - {} ", self.client_name);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title);
        frame.render_widget(block, area);
    }

    /// Render all channels
    fn render_channels(&self, frame: &mut Frame, area: Rect) {
        // Split into inputs and outputs sections
        let total_inputs = self.mixer_state.inputs.len();
        let total_outputs = self.mixer_state.outputs.len();
        let total_channels = total_inputs + total_outputs;

        if total_channels == 0 {
            return;
        }

        // Calculate constraints for channel strips
        let input_ratio = total_inputs as f32 / total_channels as f32;
        let output_ratio = total_outputs as f32 / total_channels as f32;

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage((input_ratio * 100.0) as u16),
                Constraint::Length(1), // Separator
                Constraint::Percentage((output_ratio * 100.0) as u16),
            ])
            .split(area);

        // Render inputs
        if !self.mixer_state.inputs.is_empty() {
            self.render_channel_section(
                frame,
                chunks[0],
                &self.mixer_state.inputs,
                "INPUTS",
                true,
                self.selection_type == SelectionType::Input,
            );
        }

        // Render separator
        let sep = Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(sep, chunks[1]);

        // Render outputs
        if !self.mixer_state.outputs.is_empty() {
            self.render_channel_section(
                frame,
                chunks[2],
                &self.mixer_state.outputs,
                "OUTPUTS",
                false,
                self.selection_type == SelectionType::Output,
            );
        }
    }

    /// Render a section of channels (inputs or outputs)
    fn render_channel_section(
        &self,
        frame: &mut Frame,
        area: Rect,
        channels: &[ChannelState],
        title: &str,
        is_input: bool,
        is_selected_section: bool,
    ) {
        let section_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(5)])
            .split(area);

        // Section title
        let title_style = if is_selected_section {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let title_para = Paragraph::new(title).style(title_style);
        frame.render_widget(title_para, section_chunks[0]);

        // Channel strips
        let strip_area = section_chunks[1];
        let num_channels = channels.len();
        if num_channels == 0 {
            return;
        }

        // Calculate width for each channel strip
        let strip_width = (strip_area.width / num_channels as u16).max(8);
        let constraints: Vec<Constraint> = (0..num_channels)
            .map(|_| Constraint::Length(strip_width))
            .collect();

        let strip_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(strip_area);

        for (i, channel) in channels.iter().enumerate() {
            let selected =
                is_selected_section && is_input == (self.selection_type == SelectionType::Input)
                    && i == self.selected_channel
                    && is_selected_section;
            let strip = ChannelStrip::new(channel, is_input).selected(selected);
            frame.render_widget(strip, strip_chunks[i]);
        }
    }

    /// Render the help bar
    fn render_help(&self, frame: &mut Frame, area: Rect) {
        let help_text = Line::from(vec![
            Span::styled("←/→", Style::default().fg(Color::Yellow)),
            Span::raw(" Sel "),
            Span::styled("↑/↓", Style::default().fg(Color::Yellow)),
            Span::raw(" Vol "),
            Span::styled("0", Style::default().fg(Color::Yellow)),
            Span::raw(" 0dB "),
            Span::styled("m", Style::default().fg(Color::Yellow)),
            Span::raw(" Mute "),
            Span::styled("s", Style::default().fg(Color::Yellow)),
            Span::raw(" Solo "),
            Span::styled("Tab", Style::default().fg(Color::Yellow)),
            Span::raw(" Switch "),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::raw(" Quit"),
        ]);

        let help = Paragraph::new(help_text).style(Style::default().fg(Color::Gray));
        frame.render_widget(help, area);
    }
}
