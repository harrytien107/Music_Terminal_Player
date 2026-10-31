use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rodio::{OutputStream, OutputStreamBuilder};

pub(crate) const DEVICE_UNAVAILABLE_MESSAGE: &str = "Audio output device disconnected. The requested device is no longer available; it may have been unplugged or disconnected.";

pub(crate) struct AudioOutput {
    stream: OutputStream,
    lost: Arc<AtomicBool>,
    #[cfg(windows)]
    endpoint_monitor: EndpointMonitor,
}

#[cfg(windows)]
struct EndpointMonitor {
    changed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

#[cfg(windows)]
impl EndpointMonitor {
    fn start(endpoint_id: Option<String>) -> Self {
        let changed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(previous) = endpoint_id {
            let changed_worker = Arc::clone(&changed);
            let stop_worker = Arc::clone(&stop);
            let _ = std::thread::Builder::new()
                .name("audio-endpoint-monitor".to_owned())
                .spawn(move || {
                    while !stop_worker.load(Ordering::Acquire) {
                        if endpoint_id_changed(&previous, default_output_endpoint_id().as_deref()) {
                            changed_worker.store(true, Ordering::Release);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(300));
                    }
                });
        }
        Self { changed, stop }
    }

    fn changed(&self) -> bool {
        self.changed.load(Ordering::Acquire)
    }
}

#[cfg(windows)]
impl Drop for EndpointMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(windows)]
fn endpoint_id_changed(previous: &str, current: Option<&str>) -> bool {
    current.is_some_and(|current| previous != current)
}

impl AudioOutput {
    pub(crate) fn open() -> Result<Self> {
        #[cfg(windows)]
        let endpoint_id = default_output_endpoint_id();
        let lost = Arc::new(AtomicBool::new(false));
        let callback_lost = Arc::clone(&lost);
        let stream = OutputStreamBuilder::from_default_device()
            .context("failed to find the default audio output")?
            .with_error_callback(move |_error| callback_lost.store(true, Ordering::Release))
            .open_stream()
            .context("failed to open default audio output")?;
        #[cfg(windows)]
        let endpoint_monitor = EndpointMonitor::start(endpoint_id);
        Ok(Self {
            stream,
            lost,
            #[cfg(windows)]
            endpoint_monitor,
        })
    }

    pub(crate) fn stream(&self) -> &OutputStream {
        &self.stream
    }

    pub(crate) fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    pub(crate) fn default_changed(&self) -> bool {
        #[cfg(windows)]
        {
            return self.endpoint_monitor.changed();
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

#[cfg(windows)]
fn default_output_endpoint_id() -> Option<String> {
    use windows::Win32::Media::Audio::{
        IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize,
    };

    struct ComInit(bool);
    impl Drop for ComInit {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    // A Tokio worker may already have COM initialized in another apartment.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if init.is_err() && init.0 != 0x80010106u32 as i32 {
        return None;
    }
    let _com = ComInit(init.is_ok());
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.ok()?;
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.ok()?;
    let raw_id = unsafe { device.GetId() }.ok()?;
    let id = unsafe { raw_id.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(raw_id.0.cast())) };
    id
}

pub(crate) fn device_unavailable_error() -> anyhow::Error {
    anyhow!(DEVICE_UNAVAILABLE_MESSAGE)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn endpoint_switch_ignores_failed_lookup_and_detects_new_default() {
        assert!(!endpoint_id_changed("headset", None));
        assert!(!endpoint_id_changed("headset", Some("headset")));
        assert!(endpoint_id_changed("headset", Some("headphones")));
    }

    #[test]
    #[ignore = "requires a working Windows default audio output"]
    fn can_close_and_reopen_default_audio_output() {
        let first = AudioOutput::open().expect("open first default audio output");
        assert!(!first.is_lost());
        drop(first);
        let second = AudioOutput::open().expect("reopen default audio output");
        assert!(!second.is_lost());
    }
}
