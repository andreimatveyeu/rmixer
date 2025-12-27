//! Channel strip widget
//!
//! Renders a complete channel strip with name, meters, fader value,
//! and mute/solo indicators.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};

use super::Meter;
use crate::ipc::ChannelState;

/// A channel strip widget showing meters, fader, and controls
pub struct ChannelStrip<'a> {
    /// Channel state
    state: &'a ChannelState,

    /// Whether this channel is selected
    selected: bool,

    /// Whether this is an input (true) or output (false) channel
    is_input: bool,
}

impl<'a> ChannelStrip<'a> {
    /// Create a new channel strip
    pub fn new(state: &'a ChannelState, is_input: bool) -> Self {
        Self {
            state,
            selected: false,
            is_input,
        }
    }

    /// Mark this channel as selected
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
}

impl Widget for ChannelStrip<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Create a border with the channel name
        let border_style = if self.selected {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(format!(" {} ", self.state.name));

        let inner = block.inner(area);
        block.render(area, buf);

        if inner.height < 4 || inner.width < 3 {
            return;
        }

        // Layout: meters at top, controls at bottom
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    // Meters
                Constraint::Length(1), // Volume
                Constraint::Length(1), // Mute/Solo
            ])
            .split(inner);

        // Render meters
        let meter_area = chunks[0];
        if self.state.port_count == 1 {
            // Mono: single meter centered
            let meter_width = 3.min(meter_area.width);
            let x_offset = (meter_area.width - meter_width) / 2;
            let meter_rect = Rect {
                x: meter_area.x + x_offset,
                y: meter_area.y,
                width: meter_width,
                height: meter_area.height,
            };
            Meter::new(self.state.current_peaks[0])
                .peak_hold(self.state.peak_hold[0])
                .render(meter_rect, buf);
        } else {
            // Stereo: two meters side by side
            let meter_width = 2.min(meter_area.width / 2);
            let gap = 1.min(meter_area.width.saturating_sub(meter_width * 2));
            let total_width = meter_width * 2 + gap;
            let x_offset = (meter_area.width - total_width) / 2;

            // Left meter
            let left_rect = Rect {
                x: meter_area.x + x_offset,
                y: meter_area.y,
                width: meter_width,
                height: meter_area.height,
            };
            Meter::new(self.state.current_peaks[0])
                .peak_hold(self.state.peak_hold[0])
                .render(left_rect, buf);

            // Right meter
            let right_rect = Rect {
                x: meter_area.x + x_offset + meter_width + gap,
                y: meter_area.y,
                width: meter_width,
                height: meter_area.height,
            };
            Meter::new(self.state.current_peaks[1])
                .peak_hold(self.state.peak_hold[1])
                .render(right_rect, buf);
        }

        // Render volume display
        let vol_area = chunks[1];
        let volume_text = format!("{:+.1}", self.state.volume_db);
        let volume_style = if self.state.muted {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        let volume_para = Paragraph::new(volume_text)
            .style(volume_style)
            .alignment(ratatui::layout::Alignment::Center);
        volume_para.render(vol_area, buf);

        // Render mute/solo indicators
        let control_area = chunks[2];
        let mut spans = Vec::new();

        // Mute indicator
        let mute_style = if self.state.muted {
            Style::default().fg(Color::Black).bg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled("M", mute_style));

        // Only show solo for input channels
        if self.is_input {
            spans.push(Span::raw(" "));
            let solo_style = if self.state.soloed {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled("S", solo_style));
        }

        let control_para = Paragraph::new(Line::from(spans))
            .alignment(ratatui::layout::Alignment::Center);
        control_para.render(control_area, buf);
    }
}
