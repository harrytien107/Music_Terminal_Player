use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow};
use rodio::{OutputStream, OutputStreamBuilder};

pub(crate) const DEVICE_UNAVAILABLE_MESSAGE: &str = "Audio output device disconnected. The requested device is no longer available; it may have been unplugged or disconnected.";

pub(crate) struct AudioOutput {
    stream: OutputStream,
    lost: Arc<AtomicBool>,
}

impl AudioOutput {
    pub(crate) fn open() -> Result<Self> {
        let lost = Arc::new(AtomicBool::new(false));
        let callback_lost = Arc::clone(&lost);
        let stream = OutputStreamBuilder::from_default_device()
            .context("failed to find the default audio output")?
            .with_error_callback(move |_error| callback_lost.store(true, Ordering::Release))
            .open_stream()
            .context("failed to open default audio output")?;
        Ok(Self { stream, lost })
    }

    pub(crate) fn stream(&self) -> &OutputStream {
        &self.stream
    }

    pub(crate) fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }
}

pub(crate) fn device_unavailable_error() -> anyhow::Error {
    anyhow!(DEVICE_UNAVAILABLE_MESSAGE)
}
