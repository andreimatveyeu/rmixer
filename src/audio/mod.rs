//! Audio engine module for rmixer
//!
//! Handles Pipewire integration including client registration,
//! port creation, and real-time audio processing.

mod engine;

pub use engine::AudioEngine;
