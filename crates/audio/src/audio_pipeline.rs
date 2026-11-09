use anyhow::{Context as _, Result};
use collections::HashMap;
use cpal::{DeviceId, Error, ErrorKind, default_host, traits::HostTrait};
use futures::channel::oneshot;
use gpui::{App, BorrowAppContext, Global, Task};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Source, mixer::Mixer, source::Buffered};
use settings::Settings;
use std::io::Cursor;
use util::ResultExt;

pub use crate::audio_settings::AudioSettings;

use crate::Sound;

use super::{CHANNEL_COUNT, SAMPLE_RATE};

pub fn init(_cx: &mut App) {}

#[derive(Default)]
pub struct Audio {
    output: Option<(MixerDeviceSink, Mixer)>,
    output_error_task: Option<Task<()>>,
    source_cache: HashMap<Sound, Buffered<Decoder<Cursor<Vec<u8>>>>>,
}

impl Global for Audio {}

impl Audio {
    fn ensure_output_exists(
        &mut self,
        output_audio_device: Option<DeviceId>,
        cx: &mut App,
    ) -> Result<&Mixer> {
        #[cfg(debug_assertions)]
        log::warn!(
            "Audio does not sound correct without optimizations. Use a release build to debug audio issues"
        );

        if self.output.is_none() {
            let (error_callback, error_task) = output_error_handler(cx);
            let output = open_output_stream(output_audio_device, error_callback)?;
            self.output = Some(output);
            self.output_error_task = Some(error_task);
        }

        Ok(self
            .output
            .as_ref()
            .map(|(_, mixer)| mixer)
            .expect("we only get here if opening the outputstream succeeded"))
    }

    pub fn play_sound(sound: Sound, cx: &mut App) {
        let output_audio_device = AudioSettings::get_global(cx).output_audio_device.clone();
        cx.update_default_global(|this: &mut Self, cx| {
            let source = this.sound_source(sound, cx).log_err()?;
            let output_mixer = this
                .ensure_output_exists(output_audio_device, cx)
                .context("Could not get output mixer")
                .log_err()?;

            output_mixer.add(source);
            Some(())
        });
    }

    /// Drops the output stream so the next sound reopens it.
    fn reset_output(cx: &mut App) {
        cx.update_default_global(|audio: &mut Self, _cx| {
            audio.output_error_task.take();
            audio.output.take();
        });
    }

    fn sound_source(&mut self, sound: Sound, cx: &App) -> Result<impl Source + use<>> {
        if let Some(wav) = self.source_cache.get(&sound) {
            return Ok(wav.clone());
        }

        let path = format!("sounds/{}.wav", sound.file());
        let bytes = cx
            .asset_source()
            .load(&path)?
            .map(anyhow::Ok)
            .with_context(|| format!("No asset available for path {path}"))??
            .into_owned();
        let cursor = Cursor::new(bytes);
        let source = Decoder::new(cursor)?.buffered();

        self.source_cache.insert(sound, source.clone());

        Ok(source)
    }
}

fn resolve_output_device(device_id: Option<&DeviceId>) -> anyhow::Result<cpal::Device> {
    if let Some(id) = device_id {
        if let Some(device) = default_host().device_by_id(id) {
            return Ok(device);
        }
        log::warn!("Selected audio device not found, falling back to default");
    }
    let host = default_host();
    let device = host
        .default_output_device()
        .context("no audio output device available")?;

    // CPAL's virtual default devices use ActivateAudioInterfaceAsync, which can fail
    // with RPC_E_CHANGED_MODE. Resolve a concrete WASAPI endpoint instead; the
    // current default is selected again when the stream is reopened.
    #[cfg(target_os = "windows")]
    let device = host
        .device_by_id(&cpal::traits::DeviceTrait::id(&device)?)
        .context("default audio endpoint is no longer available")?;

    Ok(device)
}

fn open_output_stream(
    device_id: Option<DeviceId>,
    error_callback: impl FnMut(Error) + Send + 'static,
) -> anyhow::Result<(MixerDeviceSink, Mixer)> {
    let device = resolve_output_device(device_id.as_ref())?;
    let mut output_handle = DeviceSinkBuilder::from_device(device)?
        .with_buffer_size(cpal::BufferSize::Default)
        .with_error_callback(error_callback)
        .open_stream()
        .context("Could not open output stream")?;
    output_handle.log_on_drop(false);
    log::info!("Output stream: {:?}", output_handle);

    let (output_mixer, source) = rodio::mixer::mixer(CHANNEL_COUNT, SAMPLE_RATE);
    // otherwise the mixer ends as it's empty
    output_mixer.add(rodio::source::Zero::new(CHANNEL_COUNT, SAMPLE_RATE));
    output_handle.mixer().add(source);

    Ok((output_handle, output_mixer))
}

fn output_error_handler(cx: &mut App) -> (impl FnMut(Error) + Send + 'static + use<>, Task<()>) {
    let (sender, receiver) = oneshot::channel();
    let mut sender = Some(sender);
    let callback = move |error: Error| {
        if matches!(
            error.kind(),
            ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
        ) {
            return;
        }
        if let Some(sender) = sender.take() {
            sender.send(error).ok();
        }
    };
    let task = cx.spawn(async move |cx| {
        if let Ok(error) = receiver.await {
            log::error!("Audio output stream failed: {error}");
            cx.update(Audio::reset_output);
        }
    });
    (callback, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn test_output_error_handler(cx: &mut TestAppContext) {
        for error_kind in [
            ErrorKind::DeviceNotAvailable,
            ErrorKind::StreamInvalidated,
            ErrorKind::BackendError,
        ] {
            let mut callback = cx.update(install_output_error_handler);
            for recoverable_kind in [
                ErrorKind::Xrun,
                ErrorKind::DeviceChanged,
                ErrorKind::RealtimeDenied,
            ] {
                callback(Error::new(recoverable_kind));
                cx.run_until_parked();
                cx.update(|cx| assert!(cx.global::<Audio>().output_error_task.is_some()));
            }

            callback(Error::new(error_kind));
            callback(Error::new(error_kind));
            cx.run_until_parked();
            cx.update(|cx| assert!(cx.global::<Audio>().output_error_task.is_none()));
        }
    }

    #[gpui::test]
    fn test_output_error_handler_cancellation(cx: &mut TestAppContext) {
        for error_before_close in [false, true] {
            let mut old_callback = cx.update(install_output_error_handler);
            if error_before_close {
                old_callback(Error::new(ErrorKind::DeviceNotAvailable));
            }
            cx.update(Audio::reset_output);

            let mut callback = cx.update(install_output_error_handler);
            old_callback(Error::new(ErrorKind::DeviceNotAvailable));
            cx.run_until_parked();
            cx.update(|cx| assert!(cx.global::<Audio>().output_error_task.is_some()));

            callback(Error::new(ErrorKind::StreamInvalidated));
            cx.run_until_parked();
            cx.update(|cx| assert!(cx.global::<Audio>().output_error_task.is_none()));
        }

        let callback = cx.update(install_output_error_handler);
        drop(callback);
        cx.run_until_parked();
        cx.update(|cx| assert!(cx.global::<Audio>().output_error_task.is_some()));
        cx.update(Audio::reset_output);
    }

    fn install_output_error_handler(cx: &mut App) -> impl FnMut(Error) + Send + 'static + use<> {
        let (callback, task) = output_error_handler(cx);
        cx.update_default_global(|audio: &mut Audio, _cx| {
            audio.output_error_task = Some(task);
        });
        callback
    }
}
