//! Mixer core
//!
//! The single source of truth for volume, mute and solo. The TUI and the API
//! server both change the mixer through [`MixerCore`], which owns the producer
//! side of the control queue and is shared between them behind a mutex.
//!
//! The audio thread never touches the core or its lock. It only sees the
//! control queue, and because messages are queued under the lock, in the same
//! order as the changes to the core state, its copy of the state cannot drift.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rtrb::Producer;

use crate::ipc::{ChannelKind, ChannelState, ControlMsg, MixerState, VOLUME_MAX_DB, VOLUME_MIN_DB};

/// A change of volume
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VolumeChange {
    /// Set the volume to this many dB
    Absolute(f32),

    /// Change the volume by this many dB
    Delta(f32),
}

/// A change to one channel, applied as a whole or not at all
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ChannelUpdate {
    pub volume: Option<VolumeChange>,
    pub muted: Option<bool>,
    pub soloed: Option<bool>,
}

/// Why a change was refused. The core state is unchanged in every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreError {
    /// The control queue has no room for the change
    QueueFull,

    /// No channel has this index
    UnknownChannel,

    /// Solo was requested on an output
    SoloOnOutput,

    /// The requested volume is not a finite number
    InvalidVolume,

    /// The mixer is shutting down and has saved its final state
    Closed,
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CoreError::QueueFull => "control queue is full",
            CoreError::UnknownChannel => "unknown channel",
            CoreError::SoloOnOutput => "solo is only available on inputs",
            CoreError::InvalidVolume => "volume must be a finite number",
            CoreError::Closed => "mixer is shutting down",
        })
    }
}

/// Owns the mixer state and the queue to the audio thread
pub struct MixerCore {
    state: MixerState,
    control_producer: Producer<ControlMsg>,
    closed: bool,
}

impl MixerCore {
    /// Create a core. `state` must match the state the audio thread starts
    /// from, and `control_producer` must feed that thread.
    pub fn new(state: MixerState, control_producer: Producer<ControlMsg>) -> Self {
        Self {
            state,
            control_producer,
            closed: false,
        }
    }

    /// Current state
    pub fn state(&self) -> &MixerState {
        &self.state
    }

    /// Index of the channel with this name
    pub fn find_channel(&self, kind: ChannelKind, name: &str) -> Option<usize> {
        self.state
            .channels(kind)
            .iter()
            .position(|channel| channel.name == name)
    }

    /// Current volumes of all channels of one kind, in channel order
    pub fn volumes(&self, kind: ChannelKind) -> Vec<f32> {
        self.state
            .channels(kind)
            .iter()
            .map(|channel| channel.volume_db)
            .collect()
    }

    /// Copy volume, mute and solo into `target`, leaving its meters alone.
    /// `target` must have the same channels as the core.
    pub fn copy_settings_to(&self, target: &mut MixerState) {
        let copy = |target: &mut [ChannelState], source: &[ChannelState]| {
            for (target, source) in target.iter_mut().zip(source) {
                target.copy_settings_from(source);
            }
        };
        copy(&mut target.inputs, &self.state.inputs);
        copy(&mut target.outputs, &self.state.outputs);
    }

    /// Apply a change to one channel. The audio thread gets the change and
    /// the core state is updated, or neither happens.
    pub fn update(
        &mut self,
        kind: ChannelKind,
        index: usize,
        update: ChannelUpdate,
    ) -> Result<(), CoreError> {
        if self.closed {
            return Err(CoreError::Closed);
        }
        if kind == ChannelKind::Output && update.soloed.is_some() {
            return Err(CoreError::SoloOnOutput);
        }
        let current_volume = self
            .state
            .channels(kind)
            .get(index)
            .ok_or(CoreError::UnknownChannel)?
            .volume_db;

        let volume_db = update
            .volume
            .map(|change| {
                let target = match change {
                    VolumeChange::Absolute(db) => db,
                    VolumeChange::Delta(delta) => current_volume + delta,
                };
                if target.is_finite() {
                    Ok(target.clamp(VOLUME_MIN_DB, VOLUME_MAX_DB))
                } else {
                    Err(CoreError::InvalidVolume)
                }
            })
            .transpose()?;

        let channel = index;
        let messages = [
            volume_db.map(|volume_db| match kind {
                ChannelKind::Input => ControlMsg::SetInputVolume { channel, volume_db },
                ChannelKind::Output => ControlMsg::SetOutputVolume { channel, volume_db },
            }),
            update.muted.map(|muted| match kind {
                ChannelKind::Input => ControlMsg::SetInputMute { channel, muted },
                ChannelKind::Output => ControlMsg::SetOutputMute { channel, muted },
            }),
            update
                .soloed
                .map(|soloed| ControlMsg::SetInputSolo { channel, soloed }),
        ];

        // Reserve room for all messages at once, so that a change to several
        // settings cannot be left half applied.
        let count = messages.iter().flatten().count();
        self.control_producer
            .write_chunk_uninit(count)
            .map_err(|_| CoreError::QueueFull)?
            .fill_from_iter(messages.into_iter().flatten());

        for msg in messages.into_iter().flatten() {
            self.state.apply_control(msg);
        }
        Ok(())
    }

    /// Set the opposite of the channel's current mute state
    pub fn toggle_muted(&mut self, kind: ChannelKind, index: usize) -> Result<(), CoreError> {
        let channel = self
            .state
            .channels(kind)
            .get(index)
            .ok_or(CoreError::UnknownChannel)?;
        let update = ChannelUpdate {
            muted: Some(!channel.muted),
            ..Default::default()
        };
        self.update(kind, index, update)
    }

    /// Set the opposite of the input's current solo state
    pub fn toggle_soloed(&mut self, index: usize) -> Result<(), CoreError> {
        let channel = self
            .state
            .inputs
            .get(index)
            .ok_or(CoreError::UnknownChannel)?;
        let update = ChannelUpdate {
            soloed: Some(!channel.soloed),
            ..Default::default()
        };
        self.update(ChannelKind::Input, index, update)
    }

    /// Refuse all further changes. The state read after this call is final.
    pub fn close(&mut self) {
        self.closed = true;
    }
}

/// Handle to the core, shared between the TUI and the API server
#[derive(Clone)]
pub struct SharedCore(Arc<Mutex<MixerCore>>);

impl SharedCore {
    pub fn new(core: MixerCore) -> Self {
        Self(Arc::new(Mutex::new(core)))
    }

    /// Lock the core. It is never left inconsistent by a panic, as `update`
    /// changes the state in one step, so a poisoned lock is still usable.
    pub fn lock(&self) -> MutexGuard<'_, MixerCore> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rtrb::{Consumer, RingBuffer};
    use std::thread;

    /// Two mono inputs and two mono outputs, with Mic at -6 dB
    pub(crate) fn test_state() -> MixerState {
        let mut mic = ChannelState::new("Mic".to_string(), 1);
        mic.set_volume_db(-6.0);
        MixerState {
            inputs: vec![mic, ChannelState::new("Music".to_string(), 2)],
            outputs: vec![
                ChannelState::new("Main".to_string(), 2),
                ChannelState::new("Monitor".to_string(), 2),
            ],
        }
    }

    pub(crate) fn test_core(capacity: usize) -> (MixerCore, Consumer<ControlMsg>) {
        let (producer, consumer) = RingBuffer::new(capacity);
        (MixerCore::new(test_state(), producer), consumer)
    }

    fn volume(db: f32) -> ChannelUpdate {
        ChannelUpdate {
            volume: Some(VolumeChange::Absolute(db)),
            ..Default::default()
        }
    }

    fn delta(db: f32) -> ChannelUpdate {
        ChannelUpdate {
            volume: Some(VolumeChange::Delta(db)),
            ..Default::default()
        }
    }

    fn muted(muted: bool) -> ChannelUpdate {
        ChannelUpdate {
            muted: Some(muted),
            ..Default::default()
        }
    }

    fn soloed(soloed: bool) -> ChannelUpdate {
        ChannelUpdate {
            soloed: Some(soloed),
            ..Default::default()
        }
    }

    /// The settings of every channel, for comparing states
    fn settings(state: &MixerState) -> Vec<(f32, f32, bool, bool)> {
        state
            .inputs
            .iter()
            .chain(&state.outputs)
            .map(|c| (c.volume_db, c.volume_linear(), c.muted, c.soloed))
            .collect()
    }

    fn drain_into(consumer: &mut Consumer<ControlMsg>, audio_state: &mut MixerState) {
        while let Ok(msg) = consumer.pop() {
            audio_state.apply_control(msg);
        }
    }

    #[test]
    fn absolute_volume_is_clamped_to_range() {
        let (mut core, _consumer) = test_core(8);
        core.update(ChannelKind::Input, 0, volume(40.0)).unwrap();
        assert_eq!(core.state().inputs[0].volume_db, VOLUME_MAX_DB);
        core.update(ChannelKind::Input, 0, volume(-200.0)).unwrap();
        assert_eq!(core.state().inputs[0].volume_db, VOLUME_MIN_DB);
    }

    #[test]
    fn delta_is_relative_and_clamped() {
        let (mut core, _consumer) = test_core(8);
        core.update(ChannelKind::Input, 0, delta(1.5)).unwrap();
        assert_eq!(core.state().inputs[0].volume_db, -4.5);
        core.update(ChannelKind::Input, 0, delta(100.0)).unwrap();
        assert_eq!(core.state().inputs[0].volume_db, VOLUME_MAX_DB);
    }

    #[test]
    fn audio_thread_receives_the_clamped_volume() {
        let (mut core, mut consumer) = test_core(8);
        core.update(ChannelKind::Output, 1, volume(40.0)).unwrap();
        match consumer.pop().unwrap() {
            ControlMsg::SetOutputVolume { channel, volume_db } => {
                assert_eq!(channel, 1);
                assert_eq!(volume_db, VOLUME_MAX_DB);
            }
            other => panic!("unexpected message: {:?}", other),
        }
    }

    #[test]
    fn mute_and_solo_are_absolute() {
        let (mut core, _consumer) = test_core(8);
        for _ in 0..2 {
            core.update(ChannelKind::Input, 1, muted(true)).unwrap();
            assert!(core.state().inputs[1].muted);
            core.update(ChannelKind::Output, 1, muted(true)).unwrap();
            assert!(core.state().outputs[1].muted);
            core.update(ChannelKind::Input, 1, soloed(true)).unwrap();
            assert!(core.state().inputs[1].soloed);
        }
    }

    #[test]
    fn toggles_set_the_opposite_of_the_core_state() {
        let (mut core, mut consumer) = test_core(8);
        core.toggle_muted(ChannelKind::Output, 0).unwrap();
        assert!(core.state().outputs[0].muted);
        core.toggle_muted(ChannelKind::Output, 0).unwrap();
        assert!(!core.state().outputs[0].muted);

        // A change from another source is taken into account
        core.update(ChannelKind::Input, 0, soloed(true)).unwrap();
        core.toggle_soloed(0).unwrap();
        assert!(!core.state().inputs[0].soloed);

        // The audio thread receives absolute values
        let mut audio_state = test_state();
        drain_into(&mut consumer, &mut audio_state);
        assert_eq!(settings(&audio_state), settings(core.state()));
    }

    #[test]
    fn solo_on_output_is_rejected() {
        let (mut core, consumer) = test_core(8);
        let update = ChannelUpdate {
            soloed: Some(false),
            muted: Some(true),
            ..Default::default()
        };
        assert_eq!(
            core.update(ChannelKind::Output, 0, update),
            Err(CoreError::SoloOnOutput)
        );
        assert!(!core.state().outputs[0].muted);
        assert_eq!(consumer.slots(), 0);
    }

    #[test]
    fn unknown_index_is_rejected() {
        let (mut core, _consumer) = test_core(8);
        assert_eq!(
            core.update(ChannelKind::Input, 2, muted(true)),
            Err(CoreError::UnknownChannel)
        );
        assert_eq!(
            core.toggle_muted(ChannelKind::Output, 2),
            Err(CoreError::UnknownChannel)
        );
        assert_eq!(core.toggle_soloed(2), Err(CoreError::UnknownChannel));
    }

    #[test]
    fn non_finite_volumes_are_rejected() {
        let (mut core, consumer) = test_core(8);
        for update in [
            volume(f32::NAN),
            volume(f32::INFINITY),
            delta(f32::NEG_INFINITY),
            delta(f32::NAN),
        ] {
            assert_eq!(
                core.update(ChannelKind::Input, 0, update),
                Err(CoreError::InvalidVolume)
            );
        }
        assert_eq!(core.state().inputs[0].volume_db, -6.0);
        assert_eq!(consumer.slots(), 0);
    }

    #[test]
    fn full_queue_leaves_the_state_unchanged() {
        let (mut core, mut consumer) = test_core(1);
        core.update(ChannelKind::Input, 0, volume(1.0)).unwrap();
        let before = settings(core.state());

        assert_eq!(
            core.update(ChannelKind::Input, 0, volume(2.0)),
            Err(CoreError::QueueFull)
        );
        assert_eq!(
            core.toggle_muted(ChannelKind::Output, 0),
            Err(CoreError::QueueFull)
        );
        assert_eq!(core.toggle_soloed(0), Err(CoreError::QueueFull));
        assert_eq!(settings(core.state()), before);

        // Once the audio thread catches up, it agrees with the core
        let mut audio_state = test_state();
        drain_into(&mut consumer, &mut audio_state);
        assert_eq!(settings(&audio_state), before);
        core.update(ChannelKind::Input, 0, volume(2.0)).unwrap();
        drain_into(&mut consumer, &mut audio_state);
        assert_eq!(settings(&audio_state), settings(core.state()));
    }

    #[test]
    fn update_that_does_not_fit_is_not_partly_applied() {
        let (mut core, consumer) = test_core(2);
        let update = ChannelUpdate {
            volume: Some(VolumeChange::Absolute(3.0)),
            muted: Some(true),
            soloed: Some(true),
        };
        let before = settings(core.state());
        assert_eq!(
            core.update(ChannelKind::Input, 0, update),
            Err(CoreError::QueueFull)
        );
        assert_eq!(settings(core.state()), before);
        assert_eq!(consumer.slots(), 0);
    }

    #[test]
    fn update_of_several_settings_is_applied_as_a_whole() {
        let (mut core, mut consumer) = test_core(3);
        let update = ChannelUpdate {
            volume: Some(VolumeChange::Delta(-1.0)),
            muted: Some(true),
            soloed: Some(true),
        };
        core.update(ChannelKind::Input, 0, update).unwrap();
        let channel = &core.state().inputs[0];
        assert_eq!(channel.volume_db, -7.0);
        assert!(channel.muted && channel.soloed);

        let mut audio_state = test_state();
        drain_into(&mut consumer, &mut audio_state);
        assert_eq!(settings(&audio_state), settings(core.state()));
    }

    #[test]
    fn closed_core_refuses_changes() {
        let (mut core, _consumer) = test_core(8);
        core.close();
        assert_eq!(
            core.update(ChannelKind::Input, 0, volume(1.0)),
            Err(CoreError::Closed)
        );
        assert_eq!(
            core.toggle_muted(ChannelKind::Input, 0),
            Err(CoreError::Closed)
        );
        assert_eq!(core.toggle_soloed(0), Err(CoreError::Closed));
        assert_eq!(core.state().inputs[0].volume_db, -6.0);
    }

    #[test]
    fn find_channel_looks_only_at_the_given_kind() {
        let (core, _consumer) = test_core(8);
        assert_eq!(core.find_channel(ChannelKind::Input, "Music"), Some(1));
        assert_eq!(core.find_channel(ChannelKind::Output, "Music"), None);
    }

    #[test]
    fn volumes_are_listed_in_channel_order() {
        let (mut core, _consumer) = test_core(8);
        core.update(ChannelKind::Output, 1, volume(-3.0)).unwrap();
        assert_eq!(core.volumes(ChannelKind::Input), vec![-6.0, 0.0]);
        assert_eq!(core.volumes(ChannelKind::Output), vec![0.0, -3.0]);
    }

    #[test]
    fn copying_settings_keeps_the_meters() {
        let (mut core, _consumer) = test_core(8);
        let mut display = test_state();
        display.inputs[0].update_meter([0.5, 0.0], 5.0);

        let update = ChannelUpdate {
            volume: Some(VolumeChange::Absolute(-12.0)),
            muted: Some(true),
            soloed: Some(true),
        };
        core.update(ChannelKind::Input, 0, update).unwrap();
        core.update(ChannelKind::Output, 1, muted(true)).unwrap();
        core.copy_settings_to(&mut display);

        assert_eq!(settings(&display), settings(core.state()));
        assert_eq!(display.inputs[0].current_peaks[0], 0.5);
        assert_eq!(display.inputs[0].peak_hold[0], 0.5);
    }

    #[test]
    fn concurrent_clients_do_not_make_the_audio_state_diverge() {
        const CHANGES_PER_CLIENT: usize = 2000;

        let (producer, mut consumer) = RingBuffer::new(4);
        let core = SharedCore::new(MixerCore::new(test_state(), producer));

        // Two clients, standing in for the TUI and the API, working the same
        // channels. Refused changes must leave no trace, so they are ignored.
        let clients: Vec<_> = (0..2)
            .map(|client| {
                let core = core.clone();
                thread::spawn(move || {
                    for i in 0..CHANGES_PER_CLIENT {
                        let mut core = core.lock();
                        let _ = match (client + i) % 4 {
                            0 => core.toggle_muted(ChannelKind::Input, 0),
                            1 => core.toggle_soloed(1),
                            2 => core.update(ChannelKind::Output, 0, delta(0.5)),
                            _ => core.update(ChannelKind::Input, 1, volume(i as f32 / 100.0)),
                        };
                    }
                })
            })
            .collect();

        // The audio thread, applying messages while the clients run
        let mut audio_state = test_state();
        while clients.iter().any(|client| !client.is_finished()) {
            drain_into(&mut consumer, &mut audio_state);
        }
        for client in clients {
            client.join().unwrap();
        }
        drain_into(&mut consumer, &mut audio_state);

        assert_eq!(settings(&audio_state), settings(core.lock().state()));
    }
}
