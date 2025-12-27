//! Level meter widget
//!
//! Renders a vertical level meter with green/yellow/red zones
//! and peak hold indicator.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::Widget,
};

use crate::ipc::VOLUME_MIN_DB;

/// Threshold where yellow zone starts (dB)
const YELLOW_THRESHOLD_DB: f32 = -12.0;

/// Threshold where red zone starts (dB)
const RED_THRESHOLD_DB: f32 = 0.0;

/// Characters for meter display (from empty to full)
const METER_CHARS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// A vertical level meter widget
pub struct Meter {
    /// Current level in linear scale (0.0 to 1.0+)
    level: f32,

    /// Peak hold level in linear scale
    peak_hold: f32,

    /// Minimum dB value (bottom of meter)
    min_db: f32,

    /// Maximum dB value (top of meter)
    max_db: f32,
}

impl Meter {
    /// Create a new meter with the given level
    pub fn new(level: f32) -> Self {
        Self {
            level,
            peak_hold: level,
            min_db: VOLUME_MIN_DB,
            max_db: 6.0, // +6 dB headroom display
        }
    }

    /// Set the peak hold level
    pub fn peak_hold(mut self, peak: f32) -> Self {
        self.peak_hold = peak;
        self
    }

    /// Convert linear level to dB
    fn linear_to_db(linear: f32) -> f32 {
        if linear <= 0.0 {
            VOLUME_MIN_DB
        } else {
            20.0 * linear.log10()
        }
    }

    /// Convert dB to normalized position (0.0 to 1.0)
    fn db_to_position(&self, db: f32) -> f32 {
        let db_clamped = db.clamp(self.min_db, self.max_db);
        (db_clamped - self.min_db) / (self.max_db - self.min_db)
    }

    /// Get the color for a given dB level
    fn color_for_db(db: f32) -> Color {
        if db >= RED_THRESHOLD_DB {
            Color::Red
        } else if db >= YELLOW_THRESHOLD_DB {
            Color::Yellow
        } else {
            Color::Green
        }
    }

    /// Get dimmed color for inactive meter zones
    fn dimmed_color_for_db(db: f32) -> Color {
        if db >= RED_THRESHOLD_DB {
            Color::Rgb(60, 20, 20)  // Dark red
        } else if db >= YELLOW_THRESHOLD_DB {
            Color::Rgb(50, 50, 20)  // Dark yellow/olive
        } else {
            Color::Rgb(20, 50, 20)  // Dark green
        }
    }
}

impl Widget for Meter {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let level_db = Self::linear_to_db(self.level);
        let peak_db = Self::linear_to_db(self.peak_hold);

        let level_pos = self.db_to_position(level_db);
        let peak_pos = self.db_to_position(peak_db);

        // Calculate how many rows should be filled
        let total_rows = area.height as f32;
        let filled_rows = (level_pos * total_rows).ceil() as u16;
        let peak_row = ((1.0 - peak_pos) * total_rows).floor() as u16;

        // Render from bottom to top
        for row in 0..area.height {
            let y = area.y + row;
            let row_from_bottom = area.height - 1 - row;

            // Calculate the dB level at this row
            let row_position = row_from_bottom as f32 / total_rows;
            let row_db = self.min_db + row_position * (self.max_db - self.min_db);
            let color = Self::color_for_db(row_db);

            for col in 0..area.width {
                let x = area.x + col;

                if row_from_bottom < filled_rows {
                    // Filled part of meter - bright colors
                    buf[(x, y)]
                        .set_char('█')
                        .set_style(Style::default().fg(color));
                } else if row == peak_row.min(area.height - 1) {
                    // Peak hold indicator
                    let peak_color = Self::color_for_db(peak_db);
                    buf[(x, y)]
                        .set_char('━')
                        .set_style(Style::default().fg(peak_color));
                } else {
                    // Empty part - dimmed version of the zone color
                    let dimmed_color = Self::dimmed_color_for_db(row_db);
                    buf[(x, y)]
                        .set_char('░')
                        .set_style(Style::default().fg(dimmed_color));
                }
            }
        }
    }
}

/// A horizontal level meter (alternative style)
pub struct HorizontalMeter {
    level: f32,
    peak_hold: f32,
    min_db: f32,
    max_db: f32,
}

impl HorizontalMeter {
    pub fn new(level: f32) -> Self {
        Self {
            level,
            peak_hold: level,
            min_db: VOLUME_MIN_DB,
            max_db: 6.0,
        }
    }

    pub fn peak_hold(mut self, peak: f32) -> Self {
        self.peak_hold = peak;
        self
    }

    fn linear_to_db(linear: f32) -> f32 {
        if linear <= 0.0 {
            VOLUME_MIN_DB
        } else {
            20.0 * linear.log10()
        }
    }

    fn db_to_position(&self, db: f32) -> f32 {
        let db_clamped = db.clamp(self.min_db, self.max_db);
        (db_clamped - self.min_db) / (self.max_db - self.min_db)
    }

    fn color_for_db(db: f32) -> Color {
        if db >= RED_THRESHOLD_DB {
            Color::Red
        } else if db >= YELLOW_THRESHOLD_DB {
            Color::Yellow
        } else {
            Color::Green
        }
    }
}

impl Widget for HorizontalMeter {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let level_db = Self::linear_to_db(self.level);
        let peak_db = Self::linear_to_db(self.peak_hold);

        let level_pos = self.db_to_position(level_db);
        let peak_pos = self.db_to_position(peak_db);

        let total_cols = area.width as f32;
        let filled_cols = (level_pos * total_cols).ceil() as u16;
        let peak_col = (peak_pos * total_cols).floor() as u16;

        let y = area.y;

        for col in 0..area.width {
            let x = area.x + col;
            let col_position = col as f32 / total_cols;
            let col_db = self.min_db + col_position * (self.max_db - self.min_db);
            let color = Self::color_for_db(col_db);

            if col < filled_cols {
                buf[(x, y)]
                    .set_char('█')
                    .set_style(Style::default().fg(color));
            } else if col == peak_col.min(area.width - 1) {
                let peak_color = Self::color_for_db(peak_db);
                buf[(x, y)]
                    .set_char('│')
                    .set_style(Style::default().fg(peak_color));
            } else {
                buf[(x, y)]
                    .set_char('─')
                    .set_style(Style::default().fg(Color::DarkGray));
            }
        }
    }
}
