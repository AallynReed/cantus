use std::num::NonZero;

use cantus_core::Pcm;
use rodio::buffer::SamplesBuffer;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player as RodioPlayer};

/// Plays one decoded clip at a time, to audition tracks while reviewing them.
#[derive(Default)]
pub struct Player {
    device: Option<MixerDeviceSink>,
    current: Option<RodioPlayer>,
}

impl Player {
    pub fn play(&mut self, pcm: Pcm) -> Result<(), String> {
        let channels = NonZero::new(pcm.channels).ok_or("no audio to play")?;
        let rate = NonZero::new(pcm.sample_rate).ok_or("no audio to play")?;
        if self.device.is_none() {
            let mut device = DeviceSinkBuilder::open_default_sink().map_err(|e| e.to_string())?;
            device.log_on_drop(false);
            self.device = Some(device);
        }
        let Some(device) = &self.device else {
            return Err("no audio device".into());
        };
        let player = RodioPlayer::connect_new(device.mixer());
        player.append(SamplesBuffer::new(channels, rate, pcm.samples));
        self.current = Some(player);
        Ok(())
    }

    pub fn stop(&mut self) {
        self.current = None;
    }

    pub fn is_playing(&self) -> bool {
        self.current.as_ref().is_some_and(|p| !p.empty())
    }
}
